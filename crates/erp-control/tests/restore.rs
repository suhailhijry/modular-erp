//! **Getting a tenant back.**
//!
//! A backup nobody has restored is not a backup, and under D15 a failed restore
//! happens on infrastructure we cannot reach. So the procedure in
//! `docs/RUNNING.md` is executable rather than prose: these tests run it.
//!
//! # What has to be backed up, and what does not
//!
//! The event log **must** be. It is the only irreplaceable thing here — it is
//! append-only, so no later state can reconstruct it, and every projection is a
//! fold over it (L2).
//!
//! Projections need not be. They are pure functions of the log and
//! `migrator refresh <module>` rebuilds them. That is a real choice with a
//! measured price: `erp-projection/tests/rebuild_throughput.rs` puts a rebuild at
//! roughly four thousand events a second, so a tenant with a few million events
//! is a quarter of an hour of rebuilding to save backing up tables that
//! `pg_dump` would have compressed anyway. **Back them up.** The option matters
//! when a backup is corrupt, not when it is merely large.
//!
//! # The two planes must come back together
//!
//! A tenant is a row in the control plane *and* a database. Restore one without
//! the other and there is no error to see — see
//! [`a_tenant_database_without_its_control_row_is_unreachable`].

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use erp_control::{Actor, ClusterRegistry, ControlPlane, ModuleSetup, PoolConfig, TenantPools};
use erp_eventlog::{Metadata, NewEvent, append};
use erp_testkit::{Schema, TestDb};
use erp_types::{
    AggregateId, DomainName, EventName, ModuleId, SchemaVersion, Sequence, StreamId, TenantId,
};
use sqlx::Connection as _;

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);

fn no_events() -> &'static erp_eventlog::Upcasters {
    static NONE: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    NONE.get_or_init(erp_eventlog::Upcasters::new)
}

fn toy_module() -> ModuleSetup {
    ModuleSetup::new(
        ModuleId::new("toy").expect("valid"),
        "CREATE SCHEMA IF NOT EXISTS proj_toy;
         CREATE TABLE IF NOT EXISTS proj_toy.thing (id INT PRIMARY KEY);",
        &[("toy", "proj_toy")],
        no_events,
    )
}

struct Fixture {
    control: ControlPlane,
    _db: TestDb,
}

impl Fixture {
    async fn new() -> Self {
        let db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");
        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("parses");
        let control = ControlPlane::new(
            db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        );
        control
            .register_cluster(
                "primary",
                "ERP_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");
        Self { control, _db: db }
    }
}

/// The base URL without its database, so a name can be appended.
fn base_url() -> String {
    let url = erp_testkit::database_url();
    url.rsplit_once('/')
        .map_or(url.clone(), |(head, _)| head.to_owned())
}

async fn connect(database: &str) -> sqlx::PgConnection {
    sqlx::PgConnection::connect(&format!("{}/{database}", base_url()))
        .await
        .expect("connects")
}

/// Appends `n` events to a tenant's log, so there is something to lose.
async fn write_events(database: &str, n: i64) {
    let mut conn = connect(database).await;
    let stream = StreamId::new(
        DomainName::new("toy").unwrap(),
        AggregateId::new("thing").unwrap(),
    );
    for i in 0..n {
        append(
            &mut conn,
            &stream,
            Sequence::new(i).unwrap(),
            &[NewEvent::new(
                EventName::new("toy.happened").unwrap(),
                SchemaVersion::new(1).unwrap(),
                serde_json::json!({ "n": i }),
            )],
            &Metadata::default(),
        )
        .await
        .expect("appends");
    }
}

/// Every event, as the thing a restore has to reproduce exactly.
async fn log_contents(database: &str) -> Vec<(i64, String, serde_json::Value)> {
    let mut conn = connect(database).await;
    sqlx::query_as::<_, (i64, String, serde_json::Value)>(
        "SELECT position, event_name, payload FROM event ORDER BY position",
    )
    .fetch_all(&mut conn)
    .await
    .expect("reads the log")
}

/// `pg_dump -Fc`, the procedure `docs/RUNNING.md` documents.
fn dump(database: &str, to: &std::path::Path) {
    let out = Command::new("pg_dump")
        .args(["--format=custom", "--no-owner", "--no-privileges", "--file"])
        .arg(to)
        .arg(format!("{}/{database}", base_url()))
        .output()
        .expect("pg_dump runs");
    assert!(
        out.status.success(),
        "pg_dump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `pg_restore` into a database that must already exist and be empty.
fn restore(database: &str, from: &std::path::Path) {
    let out = Command::new("pg_restore")
        .args(["--no-owner", "--no-privileges", "--dbname"])
        .arg(format!("{}/{database}", base_url()))
        .arg(from)
        .output()
        .expect("pg_restore runs");
    assert!(
        out.status.success(),
        "pg_restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// **A tenant's database survives being destroyed and brought back.**
///
/// The log is compared row for row, because "the restore worked" and "the
/// restore produced a database that starts" are different claims and only the
/// first one matters.
#[tokio::test]
async fn a_tenant_database_round_trips_through_dump_and_restore() {
    let fixture = Fixture::new().await;
    let done = fixture
        .control
        .sign_up(
            "owner@restore.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "restoreme".to_owned(),
            "Restore Me".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");
    let database = done.tenant.database_name.clone();

    write_events(&database, 25).await;
    let before = log_contents(&database).await;
    assert_eq!(before.len(), 25, "the fixture must have written a log");

    let file = std::env::temp_dir().join(format!("erp-restore-{}.dump", done.tenant.id.as_uuid()));
    dump(&database, &file);

    // The disaster.
    erp_testkit::drop_named_database(&database)
        .await
        .expect("drops");

    // The recovery, exactly as documented: create empty, then restore into it.
    let mut admin = connect("postgres").await;
    // `AssertSqlSafe` because `CREATE DATABASE` cannot be parameterized. The
    // name came from `tenant_database_name`, whose column CHECK refuses anything
    // outside `[a-z][a-z0-9_]*` — the same argument `provision.rs` makes, and it
    // is verified there rather than assumed here.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        r#"CREATE DATABASE "{database}""#
    )))
    .execute(&mut admin)
    .await
    .expect("recreates");
    drop(admin);
    restore(&database, &file);

    let after = log_contents(&database).await;
    assert_eq!(
        before, after,
        "the restored log must be the log that was backed up, row for row"
    );

    // And the tenant is enterable again — the control row never went away, so
    // this is the half that proves the two planes still agree.
    let db = fixture.control.enter_for_maintenance(done.tenant.id).await;
    assert!(db.is_ok(), "a restored tenant must be enterable: {db:?}");

    let _ = std::fs::remove_file(&file);
    let _ = erp_testkit::drop_named_database(&database).await;
}

/// **A tenant database restored without its control row is unreachable.**
///
/// This is the failure an operator actually hits: the two planes are backed up
/// separately, restored to different points, and nothing errors. The data is
/// there, intact, and there is no route to it — `enter` refuses with the same
/// message it gives for a tenant that never existed, because §1.9 requires that
/// a stranger cannot tell those apart.
///
/// Which means: **restore the control plane and the tenant to the same point,
/// and check the tenant is enterable afterwards.** A restore that stops at "the
/// database is back" has not finished.
#[tokio::test]
async fn a_tenant_database_without_its_control_row_is_unreachable() {
    let fixture = Fixture::new().await;
    let done = fixture
        .control
        .sign_up(
            "owner@orphan.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "orphaned".to_owned(),
            "Orphaned".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");
    let database = done.tenant.database_name.clone();
    write_events(&database, 3).await;

    // The control plane restored to a point before this tenant existed. The
    // database is untouched and complete.
    sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(done.tenant.id.as_uuid())
        .execute(fixture.control.pool())
        .await
        .expect("deletes the control row");

    assert_eq!(
        log_contents(&database).await.len(),
        3,
        "the data is still there — that is what makes this dangerous"
    );

    let entered = fixture.control.enter_for_maintenance(done.tenant.id).await;
    assert!(
        matches!(entered, Err(erp_control::AccessError::NoSuchTenant)),
        "an orphaned database must be unreachable, got {entered:?}"
    );

    let _ = erp_testkit::drop_named_database(&database).await;
    let _ = TenantId::new();
}
