//! **Rebuilding a group's read models without an outage.**
//!
//! A changed read model used to mean: drop the schema, install it again, rewind
//! the checkpoint, and let the worker replay. Correct, and every screen in the
//! product is empty for as long as the replay takes — seconds on a small tenant,
//! minutes on a large one. That is an outage with a nicer name.
//!
//! `rebuild_swap` builds the new tables beside the live ones and exchanges them
//! at the end. These are the assertions that the exchange is atomic, that
//! nothing written during the build is lost, and that a failure leaves the live
//! tables exactly as they were.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use erp_eventlog::{DomainEvent, Envelope, Metadata, NewEvent, Upcasters, append, read_since};
use erp_projection::{
    Progress, Projection, ProjectionCtx, ProjectionError, ProjectionGroup, RunError,
    ensure_group_schema, run_once, run_to_head,
};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence, StreamId};
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

/// One row per event, so the row count is the position and a lost event is
/// visible as a number rather than as a sum that happens to be wrong.
struct Totals;

#[async_trait::async_trait]
impl Projection for Totals {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "totals"
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

        sqlx::query("INSERT INTO total (id, amount) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING")
            .bind(i32::try_from(envelope.position.get()).unwrap_or(i32::MAX))
            .bind(event.amount)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }
}

/// The same group, in a build whose read model is one version on.
struct LedgerV2;
impl ProjectionGroup for LedgerV2 {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
    const VERSION: i16 = 2;
}

/// [`Totals`], as that build projects it.
struct TotalsV2;

#[async_trait::async_trait]
impl Projection for TotalsV2 {
    type Group = LedgerV2;

    fn name(&self) -> &'static str {
        "totals"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        Totals.apply(ctx, envelope, conn).await
    }
}

/// The shape the fixture starts on.
const V1: &str = "CREATE TABLE IF NOT EXISTS total (id INT PRIMARY KEY, amount BIGINT NOT NULL)";

/// The same table with a column the old one did not have.
const V2: &str = "CREATE TABLE IF NOT EXISTS total (
                      id     INT PRIMARY KEY,
                      amount BIGINT NOT NULL,
                      label  TEXT NOT NULL DEFAULT 'rebuilt'
                  )";

/// A staging schema the projections cannot write into.
const WRONG_SHAPE: &str = "CREATE TABLE IF NOT EXISTS unrelated (id INT PRIMARY KEY)";

/// Qualified, so it lands in the live schema and staging stays empty.
const QUALIFIED: &str = "CREATE TABLE IF NOT EXISTS proj_ledger.total_v2 (id INT PRIMARY KEY)";

/// A tenant database with the group's schema, its checkpoint, and the v1 shape.
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
    sqlx::raw_sql("CREATE TABLE proj_ledger.total (id INT PRIMARY KEY, amount BIGINT NOT NULL)")
        .execute(&mut *conn)
        .await
        .expect("v1 shape");
    drop(conn);

    db
}

/// Appends `n` more events to the log.
async fn seed(db: &TestDb, n: i64) {
    let mut conn = db.pool().acquire().await.expect("connection");
    let already = read_since(&mut conn, erp_types::LogPosition::ZERO, 10_000)
        .await
        .expect("reads")
        .len();
    let already = i64::try_from(already).unwrap_or(i64::MAX);

    for i in 0..n {
        append(
            &mut conn,
            &StreamId::new(
                DomainName::new("ledger").unwrap(),
                AggregateId::new("cash").unwrap(),
            ),
            Sequence::new(already + i).unwrap(),
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
}

// ---------------------------------------------------------------------------
// Rebuilding without an outage
// ---------------------------------------------------------------------------

/// **A rebuild is invisible to a reader.**
///
/// The old `refresh_module` dropped the schema and rewound the checkpoint, so
/// every screen in the product was empty until the worker caught up. This is the
/// assertion that it no longer is: the rows are there before, the rows are there
/// after, and the shape changed in between.
#[tokio::test]
async fn a_rebuild_swaps_in_new_tables_without_ever_showing_none() {
    let db = fixture().await;
    seed(&db, 40).await;

    let totals: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    run_to_head::<Ledger>(db.pool(), &totals, &upcasters(), 100)
        .await
        .expect("projects");

    let before = rows(&db).await;
    assert_eq!(before, 40, "the live tables are populated to begin with");

    let reached = erp_projection::rebuild_swap::<Ledger>(db.pool(), V2, &totals, &upcasters(), 100)
        .await
        .expect("rebuilds and swaps");

    assert_eq!(reached.get(), 40, "it caught up to the head of the log");
    assert_eq!(rows(&db).await, 40, "and the rows survived the swap");
    assert!(
        column_exists(&db, "proj_ledger", "total", "label").await,
        "the new shape is what is live now"
    );

    // The checkpoint says where the *new* tables actually are, so the worker
    // neither replays what is already there nor skips what is not.
    let mut conn = db.pool().acquire().await.expect("connection");
    let checkpoint = erp_projection::checkpoint::<Ledger>(&mut conn)
        .await
        .expect("reads");
    drop(conn);
    assert_eq!(checkpoint.get(), 40);

    // Nothing left behind: a staging schema that survived would be rebuilt on
    // top of next time, and the swap would install a mixture of two runs.
    assert!(
        !schema_exists(&db, "proj_ledger_next").await,
        "the staging schema outlived the swap"
    );
}

/// **Events appended during the build are in the swapped-in tables.**
///
/// The window between "the build finished" and "the swap happened" is the one
/// place a rebuild can silently lose data. The catch-up runs under the same lock
/// a projection run takes, before the drop, so there is nothing to lose.
#[tokio::test]
async fn a_rebuild_catches_up_what_arrived_while_it_ran() {
    let db = fixture().await;
    seed(&db, 10).await;

    let totals: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    run_to_head::<Ledger>(db.pool(), &totals, &upcasters(), 100)
        .await
        .expect("projects");
    assert_eq!(rows(&db).await, 10);

    // Written after the live checkpoint the rebuild will start from, and never
    // projected — exactly the events a naive build-then-swap would drop.
    seed(&db, 5).await;

    let reached = erp_projection::rebuild_swap::<Ledger>(db.pool(), V1, &totals, &upcasters(), 100)
        .await
        .expect("rebuilds");

    assert_eq!(reached.get(), 15, "the swap covered the whole log");
    assert_eq!(
        rows(&db).await,
        15,
        "five events written during the rebuild were lost"
    );
}

/// A rebuild that fails leaves the live tables exactly as they were.
///
/// Postgres makes DDL transactional, which is what this depends on — and
/// depending on it is worth asserting rather than assuming.
#[tokio::test]
async fn a_failed_rebuild_leaves_the_live_tables_alone() {
    let db = fixture().await;
    seed(&db, 12).await;

    let totals: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    run_to_head::<Ledger>(db.pool(), &totals, &upcasters(), 100)
        .await
        .expect("projects");
    assert_eq!(rows(&db).await, 12);

    let failed =
        erp_projection::rebuild_swap::<Ledger>(db.pool(), WRONG_SHAPE, &totals, &upcasters(), 100)
            .await;
    assert!(failed.is_err(), "a rebuild into the wrong shape succeeded");

    assert_eq!(rows(&db).await, 12, "the live rows went with the failure");
    assert!(
        !column_exists(&db, "proj_ledger", "total", "label").await
            && column_exists(&db, "proj_ledger", "total", "amount").await,
        "the live shape changed despite the failure"
    );
}

/// Install SQL that still names its schema builds nothing in staging, and the
/// swap refuses rather than renaming an empty schema over a working one.
#[tokio::test]
async fn a_rebuild_refuses_schema_qualified_install_sql() {
    let db = fixture().await;
    seed(&db, 3).await;

    let totals: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    run_to_head::<Ledger>(db.pool(), &totals, &upcasters(), 100)
        .await
        .expect("projects");

    let refused =
        erp_projection::rebuild_swap::<Ledger>(db.pool(), QUALIFIED, &totals, &upcasters(), 100)
            .await;

    let message = refused.expect_err("is refused").to_string();
    assert!(
        message.contains("schema-qualified"),
        "the refusal does not say why: {message}"
    );
    assert_eq!(rows(&db).await, 3, "the live rows survived the refusal");
}

// ---------------------------------------------------------------------------

async fn rows(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM proj_ledger.total")
        .fetch_one(db.pool())
        .await
        .expect("counts")
}

async fn column_exists(db: &TestDb, schema: &str, table: &str, column: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM information_schema.columns
          WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
    )
    .bind(schema)
    .bind(table)
    .bind(column)
    .fetch_one(db.pool())
    .await
    .expect("reads")
        > 0
}

async fn schema_exists(db: &TestDb, schema: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM information_schema.schemata WHERE schema_name = $1",
    )
    .bind(schema)
    .fetch_one(db.pool())
    .await
    .expect("reads")
        > 0
}

// ---------------------------------------------------------------------------
// Read-model versions
// ---------------------------------------------------------------------------

/// **A build does not project into tables built for an older read model.**
/// New rows in an old shape would be a table that agrees with neither build,
/// and the rebuild that fixes it is the deploy step's — so the runner refuses,
/// and the checkpoint and the tables stay exactly where they were.
#[tokio::test]
async fn a_group_built_for_an_older_read_model_is_not_projected_into() {
    let db = fixture().await;
    seed(&db, 5).await;

    let newer: Vec<&dyn Projection<Group = LedgerV2>> = vec![&TotalsV2];
    let refused = run_once::<LedgerV2>(db.pool(), &newer, &upcasters(), 100).await;

    assert!(
        matches!(
            refused,
            Err(RunError::OtherReadModel {
                group: "ledger",
                installed: 1,
                expected: 2
            })
        ),
        "a newer build projected into the old shape: {refused:?}"
    );
    assert_eq!(rows(&db).await, 0, "and wrote rows into it");
    let mut conn = db.pool().acquire().await.expect("connection");
    let checkpoint = erp_projection::checkpoint::<Ledger>(&mut conn)
        .await
        .expect("reads");
    assert_eq!(checkpoint.get(), 0, "and moved the checkpoint");
}

/// **The build still draining does not project into the newer shape** the
/// migrator swapped in under it. It would write rows by its old rules into
/// tables stamped new, the stamp would then say "current" to the migrator and
/// the request path alike, and those rows would keep the old rules for good.
/// The new build's workers project the group; until they are up it lags.
#[tokio::test]
async fn an_older_build_does_not_project_into_a_newer_shape() {
    let db = fixture().await;
    let newer: Vec<&dyn Projection<Group = LedgerV2>> = vec![&TotalsV2];
    erp_projection::rebuild_swap::<LedgerV2>(db.pool(), V2, &newer, &upcasters(), 100)
        .await
        .expect("the new build's migrator rebuilds");
    seed(&db, 3).await;

    let older: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    let refused = run_once::<Ledger>(db.pool(), &older, &upcasters(), 100).await;

    assert!(
        matches!(
            refused,
            Err(RunError::OtherReadModel {
                group: "ledger",
                installed: 2,
                expected: 1
            })
        ),
        "the draining build projected into the newer shape: {refused:?}"
    );
    assert_eq!(rows(&db).await, 0, "and wrote rows into it");

    // And the build the tables are stamped for takes the same events.
    let progress = run_once::<LedgerV2>(db.pool(), &newer, &upcasters(), 100)
        .await
        .expect("the new build projects");
    assert!(
        matches!(progress, Progress::Advanced { events: 3, .. }),
        "{progress:?}"
    );
}

/// **A rebuild brings the group up to the build that ran it**, stamped in the
/// swap: that build's worker projects on from it rather than refusing the
/// tables it just built.
#[tokio::test]
async fn a_rebuild_brings_a_group_up_to_this_builds_read_model() {
    let db = fixture().await;
    seed(&db, 4).await;
    let older: Vec<&dyn Projection<Group = Ledger>> = vec![&Totals];
    run_to_head::<Ledger>(db.pool(), &older, &upcasters(), 100)
        .await
        .expect("the old build projects");

    let newer: Vec<&dyn Projection<Group = LedgerV2>> = vec![&TotalsV2];
    erp_projection::rebuild_swap::<LedgerV2>(db.pool(), V2, &newer, &upcasters(), 100)
        .await
        .expect("rebuilds");
    seed(&db, 2).await;

    let progress = run_once::<LedgerV2>(db.pool(), &newer, &upcasters(), 100)
        .await
        .expect("the rebuilt group is this build's");
    assert!(
        matches!(progress, Progress::Advanced { events: 2, .. }),
        "{progress:?}"
    );
    assert_eq!(rows(&db).await, 6);
}
