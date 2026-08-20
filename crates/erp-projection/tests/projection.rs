//! The projection runtime: grouping, isolation, checkpoints, and replay.
//!
//! The most important test here is
//! [`the_differ_catches_a_projection_that_reads_the_clock`]. Without it, a clean
//! shadow diff would be ambiguous between "replay is reproducible" and "the
//! differ does not work" — and the second is indistinguishable from the first
//! right up until it matters.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde::{Deserialize, Serialize};
use erp_eventlog::{
    DomainEvent, Envelope, Metadata, NewEvent, Upcasters, append, integrity, read_since,
};
use erp_projection::{
    Progress, Projection, ProjectionCtx, ProjectionError, ProjectionGroup, checkpoint,
    ensure_group_schema, replay_shadow, run_once, run_to_head,
};
use erp_testkit::{Schema, Template};
use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId,
};
use sqlx::PgConnection;

/// The tenant schema, plus two projection groups' tables.
///
/// `ledger` and `audit` are separate groups so cross-group isolation (L3) can be
/// tested: `audit` deliberately holds a table `ledger` must not be able to read.
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

struct Ledger;
impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

struct Audit;
impl ProjectionGroup for Audit {
    const NAME: &'static str = "audit";
    const SCHEMA: &'static str = "proj_audit";
}

// ---------------------------------------------------------------------------
// The event
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Posted {
    account: String,
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

// ---------------------------------------------------------------------------
// Projections
// ---------------------------------------------------------------------------

/// Sums per account. Pure: everything it writes comes from the event.
struct Balances;

#[async_trait::async_trait]
impl Projection for Balances {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "balances"
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

        // No dedup guard: L4 commits effects with the checkpoint, so this runs
        // exactly once per event and `total + $2` is safe.
        sqlx::query(
            "INSERT INTO balance (account, total) VALUES ($1, $2)
             ON CONFLICT (account) DO UPDATE SET total = balance.total + EXCLUDED.total",
        )
        .bind(&event.account)
        .bind(event.amount)
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// Records each posting with a derived key and the event's own timestamp.
struct Postings;

#[async_trait::async_trait]
impl Projection for Postings {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "postings"
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

        sqlx::query("INSERT INTO posting (id, account, amount, at) VALUES ($1, $2, $3, $4)")
            // Derived, not random: `Uuid::new_v4()` here would make every
            // replayed row differ from live.
            .bind(ctx.derive_id("posting"))
            .bind(&event.account)
            .bind(event.amount)
            // The event's time, not the wall clock.
            .bind(ctx.event_time())
            .execute(conn)
            .await?;
        Ok(())
    }
}

/// **Deliberately broken**: reads the wall clock. Exists so the differ has
/// something to catch.
struct ClockReader;

#[async_trait::async_trait]
impl Projection for ClockReader {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "clock-reader"
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

        sqlx::query("INSERT INTO posting (id, account, amount, at) VALUES ($1, $2, $3, now())")
            .bind(ctx.derive_id("posting"))
            .bind(&event.account)
            .bind(event.amount)
            .execute(conn)
            .await?;
        Ok(())
    }
}

/// **Deliberately broken**: reaches into another group's schema (L3).
struct CrossGroupReader;

#[async_trait::async_trait]
impl Projection for CrossGroupReader {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "cross-group-reader"
    }

    async fn apply(
        &self,
        _ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if envelope.event_name != posted_name() {
            return Ok(());
        }
        // `audit_entry` belongs to the audit group. Unqualified, this must not
        // resolve.
        sqlx::query("SELECT count(*) FROM audit_entry")
            .execute(conn)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

async fn fixture() -> erp_testkit::TestDb {
    let db = Template::get(&TENANT)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones");

    let mut conn = db.pool().acquire().await.expect("connection");
    ensure_group_schema::<Ledger>(&mut conn)
        .await
        .expect("schema");
    ensure_group_schema::<Audit>(&mut conn)
        .await
        .expect("schema");

    sqlx::raw_sql(
        "CREATE TABLE proj_ledger.balance (account TEXT PRIMARY KEY, total BIGINT NOT NULL);
         CREATE TABLE proj_ledger.posting (
             id UUID PRIMARY KEY, account TEXT NOT NULL,
             amount BIGINT NOT NULL, at TIMESTAMPTZ NOT NULL
         );
         CREATE TABLE proj_audit.audit_entry (id BIGINT PRIMARY KEY, note TEXT NOT NULL);",
    )
    .execute(&mut *conn)
    .await
    .expect("group tables");

    db
}

async fn post(conn: &mut PgConnection, account: &str, amount: i64, sequence: i64) {
    append(
        conn,
        &StreamId::new(
            DomainName::new("ledger").unwrap(),
            AggregateId::new(account).unwrap(),
        ),
        Sequence::new(sequence).unwrap(),
        &[NewEvent::new(
            posted_name(),
            SchemaVersion::new(1).unwrap(),
            serde_json::json!({ "account": account, "amount": amount }),
        )],
        &Metadata::default(),
    )
    .await
    .expect("appends");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_group_advances_and_records_where_it_got_to() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..3 {
        post(&mut conn, "cash", 100, i).await;
    }
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances, &Postings];
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("runs");

    assert!(
        matches!(progress, Progress::Advanced { events: 3, .. }),
        "got {progress:?}"
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    assert_eq!(checkpoint::<Ledger>(&mut conn).await.unwrap().get(), 3);

    let total: i64 = sqlx::query_scalar("SELECT total FROM proj_ledger.balance WHERE account = $1")
        .bind("cash")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(total, 300);

    // A second pass has nothing to do — the checkpoint prevents reapplication.
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("runs");
    assert!(
        matches!(progress, Progress::UpToDate { .. }),
        "{progress:?}"
    );

    let total: i64 = sqlx::query_scalar("SELECT total FROM proj_ledger.balance WHERE account = $1")
        .bind("cash")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(total, 300, "a second pass must not double-count");
}

/// **L3.** A projection reaching into another group's schema fails.
#[tokio::test]
async fn a_projection_cannot_read_another_groups_tables() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    post(&mut conn, "cash", 100, 0).await;
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&CrossGroupReader];
    let result = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100).await;

    assert!(
        result.is_err(),
        "reading across groups must fail, not silently succeed"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("audit_entry") || message.contains("does not exist"),
        "the failure should name the unreachable table: {message}"
    );

    // And the checkpoint did not move — a failed batch commits nothing.
    let mut conn = db.pool().acquire().await.expect("connection");
    assert_eq!(
        checkpoint::<Ledger>(&mut conn).await.unwrap(),
        LogPosition::ZERO
    );
}

/// **L4.** A failing projection leaves neither effects nor a moved checkpoint.
#[tokio::test]
async fn a_failed_batch_commits_nothing() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..3 {
        post(&mut conn, "cash", 100, i).await;
    }
    drop(conn);

    // Balances succeeds, then the cross-group reader fails — so the first
    // projection's writes must be rolled back too.
    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances, &CrossGroupReader];
    assert!(
        run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
            .await
            .is_err()
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_ledger.balance")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(
        rows, 0,
        "an earlier projection's writes must roll back with the batch"
    );
    assert_eq!(
        checkpoint::<Ledger>(&mut conn).await.unwrap(),
        LogPosition::ZERO
    );
}

/// **L4.** The checkpoint row's lock is the lease: two workers cannot process
/// one group at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_worker_is_told_the_group_is_busy() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    post(&mut conn, "cash", 100, 0).await;
    drop(conn);

    // Hold the lease by hand, exactly as a running worker would.
    let mut holder = db.pool().begin().await.expect("transaction");
    sqlx::query("SELECT position FROM projection_checkpoint WHERE group_name = $1 FOR UPDATE")
        .bind(Ledger::NAME)
        .fetch_one(&mut *holder)
        .await
        .expect("takes the lease");

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances];
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("returns rather than blocking");
    assert_eq!(
        progress,
        Progress::Busy,
        "a second worker must be told the group is taken, not block on it"
    );

    // Once released, work proceeds.
    holder.rollback().await.expect("releases");
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("runs");
    assert!(
        matches!(progress, Progress::Advanced { .. }),
        "{progress:?}"
    );
}

#[tokio::test]
async fn batches_are_bounded_and_resume_where_they_stopped() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..10 {
        post(&mut conn, "cash", 1, i).await;
    }
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances];

    let first = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 4)
        .await
        .expect("runs");
    assert!(
        matches!(first, Progress::Advanced { events: 4, .. }),
        "{first:?}"
    );

    let head = run_to_head::<Ledger>(db.pool(), &projections, &upcasters(), 4)
        .await
        .expect("catches up");
    assert_eq!(head.get(), 10);

    let mut conn = db.pool().acquire().await.expect("connection");
    let total: i64 = sqlx::query_scalar("SELECT total FROM proj_ledger.balance WHERE account = $1")
        .bind("cash")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(total, 10, "every event applied exactly once across batches");
}

// ---------------------------------------------------------------------------
// Shadow replay
// ---------------------------------------------------------------------------

/// A pure group rebuilds to exactly what is live.
#[tokio::test]
async fn a_pure_group_replays_identically() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..5 {
        post(&mut conn, "cash", 100, i).await;
    }
    for i in 0..3 {
        post(&mut conn, "sales", 50, i).await;
    }
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances, &Postings];
    run_to_head::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("catches up");

    let report = replay_shadow::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("replays");

    assert!(
        report.is_reproducible(),
        "a pure group must rebuild identically; differences: {:?}",
        report.differences()
    );
    assert_eq!(report.position.get(), 8);
    assert_eq!(report.tables.len(), 2, "both group tables were compared");
}

/// **The test that makes the differ trustworthy.**
///
/// A projection reading `now()` must be caught. If this ever passes, a clean
/// diff from any other test means nothing.
#[tokio::test]
async fn the_differ_catches_a_projection_that_reads_the_clock() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..3 {
        post(&mut conn, "cash", 100, i).await;
    }
    drop(conn);

    let broken: Vec<&dyn Projection<Group = Ledger>> = vec![&ClockReader];
    run_to_head::<Ledger>(db.pool(), &broken, &upcasters(), 100)
        .await
        .expect("catches up");

    // A moment passes, so the replay's `now()` differs from the live one.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let report = replay_shadow::<Ledger>(db.pool(), &broken, &upcasters(), 100)
        .await
        .expect("replays");

    assert!(
        !report.is_reproducible(),
        "the differ failed to notice a projection reading the wall clock — \
         every other reproducibility assertion in this suite is worthless"
    );

    let posting = report
        .tables
        .iter()
        .find(|t| t.table == "posting")
        .expect("posting was compared");
    assert_eq!(
        posting.only_in_live, 3,
        "all three live rows should be unmatched"
    );
    assert_eq!(
        posting.only_in_replay, 3,
        "and all three replayed rows should be unmatched"
    );
}

/// The shadow rebuild must not touch the live tables.
#[tokio::test]
async fn a_shadow_replay_leaves_the_live_tables_alone() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..4 {
        post(&mut conn, "cash", 25, i).await;
    }
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances];
    run_to_head::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("catches up");

    let mut conn = db.pool().acquire().await.expect("connection");
    let before: i64 = sqlx::query_scalar("SELECT total FROM proj_ledger.balance")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    let checkpoint_before = checkpoint::<Ledger>(&mut conn).await.unwrap();
    drop(conn);

    replay_shadow::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("replays");

    let mut conn = db.pool().acquire().await.expect("connection");
    let after: i64 = sqlx::query_scalar("SELECT total FROM proj_ledger.balance")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(after, before, "the rebuild must not write to live tables");
    assert_eq!(
        checkpoint::<Ledger>(&mut conn).await.unwrap(),
        checkpoint_before,
        "nor move the live checkpoint"
    );

    // And the log itself is untouched.
    assert!(integrity(&mut conn).await.expect("checks").is_contiguous());
}

/// The rebuild stops at the live checkpoint, so events written after it are not
/// reported as spurious differences.
#[tokio::test]
async fn the_rebuild_stops_at_the_live_checkpoint() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..3 {
        post(&mut conn, "cash", 100, i).await;
    }
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances];
    run_to_head::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("catches up");

    // More events arrive that live has not projected yet.
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 3..6 {
        post(&mut conn, "cash", 100, i).await;
    }
    let pending = read_since(&mut conn, LogPosition::new(3).unwrap(), 100)
        .await
        .expect("reads");
    assert_eq!(pending.len(), 3, "three events are unprojected");
    drop(conn);

    let report = replay_shadow::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("replays");

    assert_eq!(report.position.get(), 3, "compared at the live checkpoint");
    assert!(
        report.is_reproducible(),
        "unprojected events must not read as differences: {:?}",
        report.differences()
    );
}
