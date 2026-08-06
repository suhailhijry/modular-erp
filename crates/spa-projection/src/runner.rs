//! Driving a projection group forward.

use spa_eventlog::{ReadError, Upcasters, read_since};
use spa_types::LogPosition;
use sqlx::{PgConnection, PgPool};

use crate::group::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};

/// Postgres 55P03: could not obtain lock, `NOWAIT` was requested.
const LOCK_NOT_AVAILABLE: &str = "55P03";

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error("projection {projection} failed at position {position}: {source}")]
    Projection {
        projection: &'static str,
        position: LogPosition,
        #[source]
        source: ProjectionError,
    },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Nothing to do — already at the head of the log.
    UpToDate { at: LogPosition },
    /// Applied a batch and advanced.
    Advanced {
        from: LogPosition,
        to: LogPosition,
        events: usize,
    },
    /// Another worker holds the lease. Not an error: the group is being
    /// processed, just not by us.
    Busy,
}

impl Progress {
    /// Whether more work may remain. Drives "keep going until idle" loops.
    #[must_use]
    pub const fn may_have_more(&self) -> bool {
        matches!(self, Self::Advanced { .. })
    }
}

/// Ensures a group's schema exists.
///
/// Called when a module is enabled for a tenant. Separate from the migrations
/// because which groups exist depends on which modules that tenant has.
pub async fn ensure_group_schema<G: ProjectionGroup>(
    conn: &mut PgConnection,
) -> Result<(), sqlx::Error> {
    // `G::SCHEMA` is a `&'static str` from source, never input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(G::SCHEMA)
    )))
    .execute(&mut *conn)
    .await?;

    sqlx::query!(
        "INSERT INTO projection_checkpoint (group_name) VALUES ($1)
         ON CONFLICT (group_name) DO NOTHING",
        G::NAME,
    )
    .execute(conn)
    .await?;

    Ok(())
}

/// Where a group has got to.
pub async fn checkpoint<G: ProjectionGroup>(
    conn: &mut PgConnection,
) -> Result<LogPosition, sqlx::Error> {
    let position = sqlx::query_scalar!(
        "SELECT position FROM projection_checkpoint WHERE group_name = $1",
        G::NAME,
    )
    .fetch_optional(conn)
    .await?
    .unwrap_or(0);

    Ok(LogPosition::new(position).unwrap_or(LogPosition::ZERO))
}

/// Advances a group by at most `batch_size` events.
///
/// # What makes this correct (L4)
///
/// Everything below happens in **one transaction**:
///
/// 1. `SELECT … FOR UPDATE NOWAIT` on the checkpoint row — this is the lease, so
///    a second worker gets [`Progress::Busy`] instead of applying the same
///    events twice.
/// 2. `SET LOCAL search_path` to the group's schema — this is L3, so a
///    projection reaching into another group's tables fails here.
/// 3. Apply each event, in position order, through every projection.
/// 4. Update the checkpoint.
///
/// Because the checkpoint moves with the effects, a crash anywhere leaves the
/// two consistent: the events whose effects were lost are exactly the events the
/// checkpoint has not passed. No dedup table, no reconciliation.
pub async fn run_once<G: ProjectionGroup>(
    pool: &PgPool,
    projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters,
    batch_size: i64,
) -> Result<Progress, RunError> {
    let mut tx = pool.begin().await?;

    // 1. The lease. `NOWAIT` so a second worker returns immediately rather than
    //    blocking a connection until the first finishes.
    let held = sqlx::query_scalar!(
        "SELECT position FROM projection_checkpoint
          WHERE group_name = $1
          FOR UPDATE NOWAIT",
        G::NAME,
    )
    .fetch_optional(&mut *tx)
    .await;

    let position = match held {
        Ok(Some(position)) => position,
        // No row: the group's schema has not been created. Nothing to do.
        Ok(None) => {
            tx.rollback().await?;
            return Ok(Progress::UpToDate {
                at: LogPosition::ZERO,
            });
        }
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some(LOCK_NOT_AVAILABLE) => {
            tx.rollback().await?;
            return Ok(Progress::Busy);
        }
        Err(e) => {
            tx.rollback().await?;
            return Err(e.into());
        }
    };
    let from = LogPosition::new(position).unwrap_or(LogPosition::ZERO);

    let batch = read_since(&mut tx, from, batch_size).await?;
    if batch.is_empty() {
        tx.rollback().await?;
        return Ok(Progress::UpToDate { at: from });
    }

    // 2. L3. From here to commit, unqualified names resolve only inside the
    //    group's own schema.
    set_search_path(&mut tx, G::SCHEMA).await?;

    // 3. Apply, in order.
    let mut to = from;
    for envelope in &batch {
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
        to = envelope.position;
    }

    // 4. The checkpoint, in the same transaction as the effects above.
    //
    // Qualified because `search_path` now points at the group's schema, and the
    // checkpoint deliberately lives outside it — a projection must not be able
    // to reach its own checkpoint.
    sqlx::query!(
        "UPDATE public.projection_checkpoint
            SET position = $2, updated_at = now()
          WHERE group_name = $1",
        G::NAME,
        to.get(),
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Progress::Advanced {
        from,
        to,
        events: batch.len(),
    })
}

/// Runs until the group is at the head of the log.
pub async fn run_to_head<G: ProjectionGroup>(
    pool: &PgPool,
    projections: &[&dyn Projection<Group = G>],
    upcasters: &Upcasters,
    batch_size: i64,
) -> Result<LogPosition, RunError> {
    loop {
        match run_once::<G>(pool, projections, upcasters, batch_size).await? {
            Progress::Advanced { .. } => {}
            Progress::UpToDate { at } => return Ok(at),
            Progress::Busy => {
                // Someone else is driving this group. Report where they have
                // got to rather than fighting them for the lease.
                let mut conn = pool.acquire().await?;
                return Ok(checkpoint::<G>(&mut conn).await?);
            }
        }
    }
}

pub(crate) async fn set_search_path(
    conn: &mut PgConnection,
    schema: &str,
) -> Result<(), sqlx::Error> {
    // `SET LOCAL` is transaction-scoped, so the restriction cannot leak to the
    // next user of this pooled connection.
    //
    // `pg_catalog` is always searched first whether listed or not, so built-in
    // functions keep working while application tables outside the group do not.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path TO {}",
        quote_ident(schema)
    )))
    .execute(conn)
    .await?;
    Ok(())
}

/// Quotes a schema name.
///
/// Every value reaching here is a `&'static str` from a `ProjectionGroup`
/// declaration, never input — but "it's internal" is how injection bugs get
/// argued into existence, so the character set is checked rather than assumed.
pub(crate) fn quote_ident(name: &str) -> String {
    debug_assert!(
        !name.is_empty()
            && name.len() < 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "{name:?} is not a safe schema identifier"
    );
    format!("\"{}\"", name.replace('"', ""))
}
