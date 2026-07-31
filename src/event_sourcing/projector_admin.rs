//! Deliberate projector reset / rebuild operations.
//!
//! A rebuild requires THREE coordinated resets - missing any one breaks
//! it in a different way:
//!
//!   1. Postgres checkpoint      - or replay() resumes at the end, no-op
//!   2. Idempotency/dedup state  - or the rebuild's reprocessing gets
//!      (processed_events,         treated as duplicates and no-ops on
//!       retry_attempts,           every event; natural-key read tables
//!       dead letters,             conflict-and-skip the same way
//!       read tables)
//!   3. Kafka consumer group     - or the restarted listener sees prior
//!      committed offsets          commits and skips the backfill branch
//!
//! And all three must happen with the projector STOPPED - resetting
//! state underneath a running pipeline races with its writes.
//!
//! Correct sequence, end to end:
//!
//!   stop pipeline -> reset_projector_state -> delete_consumer_group
//!     -> restart pipeline (its bootstrap sees "no commits, no
//!        checkpoint" and runs the normal fresh backfill -> live
//!        handoff, exactly like a first boot - no special rebuild path)
//!
//! `rebuild_projector` below wires that sequence together.

use rdkafka::ClientConfig;
use rdkafka::admin::{AdminClient, AdminOptions};
use rdkafka::client::DefaultClientContext;
use sqlx::{AssertSqlSafe, PgPool};
use std::time::Duration;

/// What a given projector owns, for reset purposes. Keep a registry of
/// these next to wherever reactors are constructed, so "what do I wipe
/// to rebuild X" is declared once, next to X, not remembered in a
/// runbook.
pub struct ProjectorResetSpec {
    /// Must match ProjectorMeta::name() exactly - it keys every state
    /// table.
    pub projector_name: &'static str,
    /// The Kafka consumer group this projector's listener uses. Usually
    /// the same string as projector_name in this codebase.
    pub consumer_group: String,
    /// Read tables this projector exclusively owns, to TRUNCATE.
    /// EXCLUSIVE ownership matters: truncating a table two projectors
    /// share wipes the other one's output too. If a table is shared,
    /// use `read_table_delete_predicates` instead.
    pub read_tables_truncate: Vec<&'static str>,
    /// (table, WHERE-clause) pairs for shared tables where only this
    /// projector's rows should go - e.g. ("audit_log", "source = 'x'").
    /// Clauses are trusted, developer-authored SQL fragments, NOT user
    /// input.
    pub read_table_delete_predicates: Vec<(&'static str, &'static str)>,
}

/// Step 1+2: wipe this projector's Postgres-side state, atomically.
/// The projector's pipeline must already be stopped.
pub async fn reset_projector_state(pool: &PgPool, spec: &ProjectorResetSpec) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    // -- 1. checkpoint --------------------------------------------------
    sqlx::query("DELETE FROM projector_checkpoints WHERE projector_name = $1")
        .bind(spec.projector_name)
        .execute(&mut *tx)
        .await?;

    // -- 2a. generic dedup guard ---------------------------------------
    sqlx::query("DELETE FROM processed_events WHERE projector_name = $1")
        .bind(spec.projector_name)
        .execute(&mut *tx)
        .await?;

    // -- 2b. in-flight retry counters ----------------------------------
    sqlx::query("DELETE FROM retry_attempts WHERE projector_name = $1")
        .bind(spec.projector_name)
        .execute(&mut *tx)
        .await?;

    // -- 2c. dead letters: resolve, don't delete - they're history a
    //        human may still want, and the rebuild supersedes them ------
    sqlx::query(
        "UPDATE projector_dead_letters
         SET resolved_at = now(),
             resolution_notes = COALESCE(resolution_notes || ' | ', '') || 'superseded by projector reset'
         WHERE projector_name = $1 AND resolved_at IS NULL",
    )
    .bind(spec.projector_name)
    .execute(&mut *tx)
    .await?;

    // -- 2d. read model output: natural-key idempotency means old rows
    //        would conflict-and-skip the rebuild's inserts --------------
    for table in &spec.read_tables_truncate {
        // Identifier position - trusted developer-supplied names only,
        // declared in code, never user input.
        sqlx::query(AssertSqlSafe(format!("TRUNCATE TABLE {table}")))
            .execute(&mut *tx)
            .await?;
    }
    for (table, predicate) in &spec.read_table_delete_predicates {
        sqlx::query(AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {predicate}"
        )))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    tracing::info!(
        projector = spec.projector_name,
        "postgres-side projector state reset"
    );
    Ok(())
}

/// Step 3: delete the Kafka consumer group so the restarted listener's
/// bootstrap sees no prior commits. The group must have no active
/// members (i.e. the listener is stopped) or the broker refuses -
/// which is a FEATURE: it makes "you forgot to stop the pipeline"
/// loudly fail instead of silently racing.
pub async fn delete_consumer_group(bootstrap_servers: &str, group: &str) -> anyhow::Result<()> {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .create()?;

    let options = AdminOptions::new().operation_timeout(Some(Duration::from_secs(10)));
    let results = tokio::time::timeout(
        Duration::from_secs(15),
        admin.delete_groups(&[group], &options),
    )
    .await
    .map_err(|_| anyhow::anyhow!("delete_groups timed out - broker unreachable?"))??;

    for result in results {
        match result {
            Ok(name) => tracing::info!(group = name, "consumer group deleted"),
            Err((name, code)) if code == rdkafka::types::RDKafkaErrorCode::GroupIdNotFound => {
                // Already gone (never consumed, or previously deleted) -
                // that's the desired end state, not an error.
                tracing::info!(
                    group = name,
                    "consumer group did not exist - nothing to delete"
                );
            }
            Err((name, code)) => {
                anyhow::bail!(
                    "failed to delete consumer group '{name}': {code:?} (is the listener still running?)"
                );
            }
        }
    }
    Ok(())
}

/// The full orchestrated reset. `stop` and `start` are how the caller
/// stops/starts this projector's pipeline - passed in rather than
/// assumed, because how pipelines are supervised differs between the
/// worker binary (join handles + watch channel) and any future
/// orchestration.
///
/// After `start`, the listener's own bootstrap does the actual rebuild:
/// it finds no commits and no checkpoint, and runs the standard
/// fresh-backfill -> live handoff. There is deliberately NO special
/// rebuild code path - the path that runs on every fresh deploy is the
/// one that gets exercised, which is exactly why it can be trusted at
/// 3am.
pub async fn rebuild_projector<Stop, StopFut, Start, StartFut>(
    pool: &PgPool,
    bootstrap_servers: &str,
    spec: &ProjectorResetSpec,
    stop: Stop,
    start: Start,
) -> anyhow::Result<()>
where
    Stop: FnOnce() -> StopFut,
    StopFut: std::future::Future<Output = anyhow::Result<()>>,
    Start: FnOnce() -> StartFut,
    StartFut: std::future::Future<Output = anyhow::Result<()>>,
{
    tracing::info!(
        projector = spec.projector_name,
        "rebuild: stopping pipeline"
    );
    stop().await?;

    tracing::info!(
        projector = spec.projector_name,
        "rebuild: resetting postgres state"
    );
    reset_projector_state(pool, spec).await?;

    tracing::info!(
        projector = spec.projector_name,
        "rebuild: deleting consumer group"
    );
    delete_consumer_group(bootstrap_servers, &spec.consumer_group).await?;

    tracing::info!(
        projector = spec.projector_name,
        "rebuild: restarting pipeline (bootstrap will run the fresh backfill)"
    );
    start().await?;

    tracing::info!(
        projector = spec.projector_name,
        "rebuild initiated - monitor the projector's checkpoint to watch it catch up to head"
    );
    Ok(())
}

// =======================================================================
// Example specs for the accounting projectors, showing the ownership
// declarations. Live next to reactor construction in your codebase.
// =======================================================================

pub fn general_ledger_reset_spec() -> ProjectorResetSpec {
    ProjectorResetSpec {
        projector_name: "general_ledger",
        consumer_group: "general_ledger".to_string(),
        // Exclusively owned by this projector - safe to truncate.
        read_tables_truncate: vec!["general_ledger"],
        read_table_delete_predicates: vec![],
    }
}

pub fn trial_balance_reset_spec() -> ProjectorResetSpec {
    ProjectorResetSpec {
        projector_name: "trial_balance",
        consumer_group: "trial_balance".to_string(),
        read_tables_truncate: vec!["trial_balance"],
        read_table_delete_predicates: vec![],
    }
}
