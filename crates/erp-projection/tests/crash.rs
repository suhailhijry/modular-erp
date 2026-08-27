//! What survives a crash, and what does not.
//!
//! Every other test in this crate exercises the code's own error paths, which
//! are the paths it was written to have. These sever the connection with
//! `pg_terminate_backend` instead, so the transaction is rolled back by Postgres
//! rather than by anything under test — and the assertions are about what is
//! left behind, not about what error came back.
//!
//! The law being checked is L4: **a checkpoint advances in the same transaction
//! as the effects it records.** Its whole value is that it holds at moments
//! nobody chose.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_eventlog::{
    DomainEvent, Envelope, Metadata, NewEvent, Upcasters, append, integrity, read_since,
};
use erp_projection::{
    Progress, Projection, ProjectionCtx, ProjectionError, ProjectionGroup, checkpoint,
    ensure_group_schema, replay_shadow, run_once, run_to_head,
};
use erp_testkit::{Schema, Template, TestDb, kill_connection};
use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId,
};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

struct Ledger;
impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Posted {
    amount: i64,
}

fn posted_name() -> EventName {
    EventName::new("ledger.posted").unwrap()
}

impl DomainEvent for Posted {
    fn event_name(&self) -> EventName {
        posted_name()
    }
    fn schema_version(&self) -> SchemaVersion {
        SchemaVersion::new(1).unwrap()
    }
}

fn upcasters() -> Upcasters {
    Upcasters::new().declare(&posted_name(), SchemaVersion::new(1).unwrap())
}

/// Sums into one row, and kills its own connection on the `crash_after`-th
/// event of the run.
///
/// The counter is deliberately not reset between batches: the point is to crash
/// once, part-way through the first batch, and then let the retry succeed.
struct Crasher {
    applied: Arc<AtomicUsize>,
    crash_after: usize,
    crashed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Projection for Crasher {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "crasher"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if envelope.event_name != posted_name() {
            return Ok(());
        }
        let event: Posted = ctx
            .decode(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        sqlx::query(
            "INSERT INTO total (id, amount, events) VALUES (1, $1, 1)
             ON CONFLICT (id) DO UPDATE
                SET amount = total.amount + EXCLUDED.amount,
                    events = total.events + 1",
        )
        .bind(event.amount)
        .execute(&mut *conn)
        .await?;

        let so_far = self.applied.fetch_add(1, Ordering::SeqCst) + 1;
        if so_far == self.crash_after && self.crashed.fetch_add(1, Ordering::SeqCst) == 0 {
            // The database goes away here, holding a transaction with this and
            // every prior event of the batch in it.
            kill_connection(&mut *conn)
                .await
                .map_err(|e| ProjectionError::Rejected(e.to_string()))?;
        }

        Ok(())
    }
}

async fn fixture() -> TestDb {
    let db = Template::get(&TENANT)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones");

    let mut conn = db.pool().acquire().await.expect("connection");
    ensure_group_schema::<Ledger>(&mut conn)
        .await
        .expect("group schema");
    sqlx::raw_sql(
        "CREATE TABLE proj_ledger.total (
             id     INT PRIMARY KEY,
             amount BIGINT NOT NULL,
             events BIGINT NOT NULL
         )",
    )
    .execute(&mut *conn)
    .await
    .expect("group tables");

    for i in 0..12 {
        append(
            &mut conn,
            &StreamId::new(
                DomainName::new("ledger").unwrap(),
                AggregateId::new("cash").unwrap(),
            ),
            Sequence::new(i).unwrap(),
            &[NewEvent::new(
                posted_name(),
                SchemaVersion::new(1).unwrap(),
                serde_json::json!({ "amount": 10 }),
            )],
            &Metadata::default(),
        )
        .await
        .expect("appends");
    }

    db
}

async fn state(db: &TestDb) -> Option<(i64, i64)> {
    sqlx::query_as("SELECT amount, events FROM proj_ledger.total WHERE id = 1")
        .fetch_optional(db.pool())
        .await
        .expect("reads")
}

async fn current_checkpoint(db: &TestDb) -> LogPosition {
    let mut conn = db.pool().acquire().await.expect("connection");
    checkpoint::<Ledger>(&mut conn).await.expect("reads")
}

// ---------------------------------------------------------------------------

/// **L4 under a real crash.**
///
/// The connection is severed with three of a five-event batch applied. Postgres
/// rolls the transaction back, so the checkpoint and the projected rows must
/// both be exactly where they were before the batch started — not three events
/// ahead, and not three rows richer.
#[tokio::test]
async fn a_crash_mid_batch_leaves_neither_rows_nor_a_moved_checkpoint() {
    let db = fixture().await;

    let applied = Arc::new(AtomicUsize::new(0));
    let kill_count = Arc::new(AtomicUsize::new(0));
    let crasher = Crasher {
        applied: Arc::clone(&applied),
        crash_after: 3,
        crashed: Arc::clone(&kill_count),
    };
    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&crasher];

    let result = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 5).await;

    assert!(
        result.is_err(),
        "a severed connection must surface as a failure, not as progress"
    );
    assert_eq!(
        kill_count.load(Ordering::SeqCst),
        1,
        "the test must actually have killed the connection"
    );
    assert_eq!(
        applied.load(Ordering::SeqCst),
        3,
        "and it must have killed it part-way through the batch, not before it"
    );

    assert_eq!(
        current_checkpoint(&db).await,
        LogPosition::ZERO,
        "the checkpoint must not have moved"
    );
    assert_eq!(
        state(&db).await,
        None,
        "and the three applied events must have rolled back with it"
    );
}

/// After the crash, a fresh run replays exactly the events that were lost.
///
/// This is what L4 buys: recovery needs no dedup table and no reconciliation,
/// because the checkpoint already names the boundary.
#[tokio::test]
async fn recovery_replays_exactly_what_the_crash_lost() {
    let db = fixture().await;

    let applied = Arc::new(AtomicUsize::new(0));
    let kill_count = Arc::new(AtomicUsize::new(0));
    let crasher = Crasher {
        applied: Arc::clone(&applied),
        crash_after: 3,
        crashed: Arc::clone(&kill_count),
    };
    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&crasher];

    assert!(
        run_once::<Ledger>(db.pool(), &projections, &upcasters(), 5)
            .await
            .is_err()
    );

    // The same projection, which has now used up its one crash, catches up.
    let head = run_to_head::<Ledger>(db.pool(), &projections, &upcasters(), 5)
        .await
        .expect("recovers");

    assert_eq!(head.get(), 12);
    assert_eq!(
        state(&db).await,
        Some((120, 12)),
        "every event applied exactly once — the three that were rolled back \
         were replayed, and none of the others were applied twice"
    );

    // And the recovered state is what a clean build would have produced.
    let clean = Crasher {
        applied: Arc::new(AtomicUsize::new(0)),
        // Never reached: the shadow rebuild would be pointless if it crashed too.
        crash_after: usize::MAX,
        crashed: Arc::new(AtomicUsize::new(0)),
    };
    let shadow: Vec<&dyn Projection<Group = Ledger>> = vec![&clean];
    let report = replay_shadow::<Ledger>(db.pool(), &shadow, &upcasters(), 100)
        .await
        .expect("replays");
    assert!(
        report.is_reproducible(),
        "recovery must be indistinguishable from never having crashed: {:?}",
        report.differences()
    );
}

/// A crash during a batch does not corrupt the log itself.
#[tokio::test]
async fn a_crash_leaves_the_log_contiguous() {
    let db = fixture().await;

    let crasher = Crasher {
        applied: Arc::new(AtomicUsize::new(0)),
        crash_after: 2,
        crashed: Arc::new(AtomicUsize::new(0)),
    };
    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&crasher];
    assert!(
        run_once::<Ledger>(db.pool(), &projections, &upcasters(), 8)
            .await
            .is_err()
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    let report = integrity(&mut conn).await.expect("checks");
    assert!(
        report.is_contiguous(),
        "L1 must survive a crash on a reader: {report:?}"
    );
    assert_eq!(report.event_count, 12);

    // And nothing has been consumed, so a tailer starting from zero still sees
    // the whole log.
    let all = read_since(&mut conn, LogPosition::ZERO, 100)
        .await
        .expect("reads");
    assert_eq!(all.len(), 12);
}

/// The lease is released by the crash, so the group is not stuck.
///
/// The checkpoint row's `FOR UPDATE` lock is what excludes a second worker. A
/// lock held by a dead backend would leave the group unprocessable until someone
/// noticed — Postgres releases it when the backend dies, and this is the
/// assertion that we depend on that rather than on a timeout.
#[tokio::test]
async fn a_crash_does_not_leave_the_group_locked() {
    let db = fixture().await;

    let crasher = Crasher {
        applied: Arc::new(AtomicUsize::new(0)),
        crash_after: 1,
        crashed: Arc::new(AtomicUsize::new(0)),
    };
    let crashing: Vec<&dyn Projection<Group = Ledger>> = vec![&crasher];
    assert!(
        run_once::<Ledger>(db.pool(), &crashing, &upcasters(), 4)
            .await
            .is_err()
    );

    let clean = Crasher {
        applied: Arc::new(AtomicUsize::new(0)),
        crash_after: usize::MAX,
        crashed: Arc::new(AtomicUsize::new(0)),
    };
    let healthy: Vec<&dyn Projection<Group = Ledger>> = vec![&clean];

    let progress = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_once::<Ledger>(db.pool(), &healthy, &upcasters(), 4),
    )
    .await
    .expect("the next worker must not block on a dead backend's lock")
    .expect("runs");

    assert!(
        matches!(progress, Progress::Advanced { events: 4, .. }),
        "{progress:?}"
    );
}
