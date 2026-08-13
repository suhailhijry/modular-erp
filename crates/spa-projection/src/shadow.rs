//! Rebuilding a group from scratch and diffing it against the live tables.
//!
//! # What this is for
//!
//! Laws L2 and L5 say projections are pure functions of the event stream. That
//! is a claim about code nobody can check by reading it — a `Utc::now()` twelve
//! calls deep in a helper looks exactly like a timestamp that came from the
//! event.
//!
//! So it is checked by experiment. Replay the whole log into an empty copy of
//! the group's tables, then diff. Identical means reproducible. Any difference
//! names the table, and usually the column, where determinism was lost.
//!
//! This runs in CI against the demo tenant on every commit, and is available as
//! an operator command per tenant. It is the difference between believing replay
//! works and knowing it.
//!
//! # Why it is trustworthy
//!
//! `tests/shadow.rs` includes projections that are *deliberately*
//! non-deterministic — one reading the wall clock, one generating a random id.
//! The differ must catch both. Without those, an empty diff would be ambiguous
//! between "reproducible" and "the differ does not work".

use spa_eventlog::Upcasters;
use spa_types::LogPosition;
use sqlx::PgPool;

use crate::group::{Projection, ProjectionGroup};
use crate::runner::{RunError, quote_ident, set_search_path};

/// How one table compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDiff {
    pub table: String,
    /// Rows in the live table that the rebuild did not produce.
    pub only_in_live: i64,
    /// Rows the rebuild produced that are not in the live table.
    pub only_in_replay: i64,
}

impl TableDiff {
    #[must_use]
    pub const fn is_identical(&self) -> bool {
        self.only_in_live == 0 && self.only_in_replay == 0
    }
}

/// The result of a shadow replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowReport {
    pub group: &'static str,
    /// Position both the live tables and the rebuild were compared at.
    pub position: LogPosition,
    pub tables: Vec<TableDiff>,
}

impl ShadowReport {
    /// Whether the rebuild matched live exactly.
    #[must_use]
    pub fn is_reproducible(&self) -> bool {
        self.tables.iter().all(TableDiff::is_identical)
    }

    /// The tables that differ, for an error message.
    #[must_use]
    pub fn differences(&self) -> Vec<&TableDiff> {
        self.tables.iter().filter(|t| !t.is_identical()).collect()
    }
}

/// Replays a group into a shadow schema and diffs it against live.
///
/// # How the comparison is made fair
///
/// The rebuild is stopped at exactly the live checkpoint. Comparing a rebuild
/// that ran further would report differences that are only "the log moved on",
/// which is noise that trains people to ignore the check.
///
/// Shadow tables are created with `LIKE … INCLUDING ALL`, so the structure comes
/// from the live tables rather than from a second declaration that could drift
/// from them.
///
/// # Cost
///
/// Replays the entire log. Cheap for a demo tenant, not something to run against
/// a large one during business hours.
pub async fn replay_shadow<G: ProjectionGroup>(
    pool: &PgPool,
    projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters,
    batch_size: i64,
) -> Result<ShadowReport, RunError> {
    let shadow = format!("shadow_{}", G::SCHEMA);

    // Pin the target before rebuilding, so both sides describe the same prefix
    // of the log even if writers are active.
    let mut conn = pool.acquire().await?;
    let target = crate::runner::checkpoint::<G>(&mut conn).await?;
    let tables = tables_in(&mut conn, G::SCHEMA).await?;
    drop(conn);

    prepare_shadow(pool, G::SCHEMA, &shadow, &tables).await?;
    rebuild_into(pool, &shadow, projections, upcasters, target, batch_size).await?;

    let mut conn = pool.acquire().await?;
    let mut diffs = Vec::with_capacity(tables.len());
    for table in &tables {
        diffs.push(diff_table(&mut conn, G::SCHEMA, &shadow, table).await?);
    }
    drop(conn);

    Ok(ShadowReport {
        group: G::NAME,
        position: target,
        tables: diffs,
    })
}

/// Every table a group owns.
async fn tables_in(
    conn: &mut sqlx::PgConnection,
    schema: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let names = sqlx::query_scalar!(
        r#"SELECT tablename as "tablename!" FROM pg_tables
            WHERE schemaname = $1 ORDER BY tablename"#,
        schema,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(names)
}

/// Drops and recreates the shadow schema with empty copies of the live tables.
async fn prepare_shadow(
    pool: &PgPool,
    live: &str,
    shadow: &str,
    tables: &[String],
) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {} CASCADE; CREATE SCHEMA {};",
        quote_ident(shadow),
        quote_ident(shadow)
    )))
    .execute(&mut *conn)
    .await?;

    for table in tables {
        // `INCLUDING ALL` brings defaults, constraints, identity and indexes, so
        // the rebuild is constrained exactly as live is. A projection that
        // violates a unique constraint on replay but not live is itself a
        // determinism bug, and this is where it surfaces.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {}.{} (LIKE {}.{} INCLUDING ALL)",
            quote_ident(shadow),
            quote_ident(table),
            quote_ident(live),
            quote_ident(table),
        )))
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

/// Replays the log into the shadow schema, stopping at `target`.
async fn rebuild_into<G: ProjectionGroup>(
    pool: &PgPool,
    shadow: &str,
    projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters,
    target: LogPosition,
    batch_size: i64,
) -> Result<(), RunError> {
    use crate::group::ProjectionCtx;
    use spa_eventlog::read_since;

    let mut position = LogPosition::ZERO;

    while position < target {
        let mut tx = pool.begin().await?;

        // Read the log *before* narrowing `search_path`. Once it points at the
        // shadow schema the `event` table is out of scope — which is L3 working
        // as intended, and the same ordering `run_once` uses.
        let batch = read_since(&mut tx, position, batch_size).await?;
        if batch.is_empty() {
            tx.rollback().await?;
            break;
        }

        set_search_path(&mut tx, shadow).await?;

        for envelope in &batch {
            // Stop exactly at the live checkpoint, mid-batch if necessary.
            if envelope.position > target {
                break;
            }
            let ctx = ProjectionCtx::new(envelope.position, envelope.recorded_at, upcasters);
            for projection in projections {
                projection
                    .apply(&ctx, envelope, &mut tx)
                    .await
                    .map_err(|source| RunError::Projection {
                        projection: projection.name(),
                        position: envelope.position,
                        source,
                    })?;
            }
            position = envelope.position;
        }

        tx.commit().await?;
    }

    Ok(())
}

/// Counts rows present on one side and not the other.
///
/// The interpolated identifiers are a group's `&'static str` schema name and
/// table names read back from `pg_tables`, both passed through `quote_ident`.
/// Neither can carry input, which is why `AssertSqlSafe` is defensible here.
///
/// `EXCEPT ALL` compares whole rows including duplicates, so a projection that
/// writes a row twice on replay is caught as well as one that writes different
/// values. `SELECT *` is safe here because the shadow table was created with
/// `LIKE`, so column order and types match by construction.
async fn diff_table(
    conn: &mut sqlx::PgConnection,
    live: &str,
    shadow: &str,
    table: &str,
) -> Result<TableDiff, sqlx::Error> {
    let live_only: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM (
             SELECT * FROM {live}.{table} EXCEPT ALL SELECT * FROM {shadow}.{table}
         ) AS missing_from_replay",
        live = quote_ident(live),
        shadow = quote_ident(shadow),
        table = quote_ident(table),
    )))
    .fetch_one(&mut *conn)
    .await?;

    let replay_only: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM (
             SELECT * FROM {shadow}.{table} EXCEPT ALL SELECT * FROM {live}.{table}
         ) AS unexpected_in_replay",
        live = quote_ident(live),
        shadow = quote_ident(shadow),
        table = quote_ident(table),
    )))
    .fetch_one(&mut *conn)
    .await?;

    Ok(TableDiff {
        table: table.to_owned(),
        only_in_live: live_only,
        only_in_replay: replay_only,
    })
}
