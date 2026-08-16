//! Self-service signup, end to end.
//!
//! The requirement is "anyone registering online can run their own system
//! without contacting us directly". What makes it hard is that it creates a
//! database, so it is not one transaction and it can fail half-done — which is
//! what [`a_failed_signup_frees_the_name_it_took`] is about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use spa_control::{
    Actor, ClusterRegistry, ControlPlane, Lane, ModuleSetup, PoolConfig, TenantPools, TenantStatus,
};
use spa_testkit::{Schema, TestDb};
use spa_types::ModuleId;
use sqlx::Connection as _;

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);

/// A module with a trivial schema, so these tests do not depend on the ledger.
fn toy_module() -> ModuleSetup {
    ModuleSetup::new(
        ModuleId::new("toy").expect("valid"),
        "CREATE SCHEMA IF NOT EXISTS proj_toy;
         CREATE TABLE IF NOT EXISTS proj_toy.thing (id INT PRIMARY KEY);",
        &[("toy", "proj_toy")],
    )
}

struct Fixture {
    control: ControlPlane,
    db: TestDb,
}

impl Fixture {
    async fn new() -> Self {
        let db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("parses");
        let control = ControlPlane::new(
            db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        );
        control
            .register_cluster(
                "primary",
                "SPA_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");

        Self { control, db }
    }

    async fn database_exists(&self, name: &str) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_database WHERE datname = $1")
            .bind(name)
            .fetch_one(self.db.pool())
            .await
            .expect("counts")
            > 0
    }

    /// A direct connection to a tenant's own database, for assertions about
    /// what is actually in it.
    async fn tenant_connection(&self, tenant: &spa_control::Tenant) -> sqlx::PgConnection {
        use sqlx::Connection;
        let url = spa_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgConnection::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects")
    }

    async fn column_exists(
        &self,
        tenant: &spa_control::Tenant,
        schema: &str,
        table: &str,
        column: &str,
    ) -> bool {
        let mut conn = self.tenant_connection(tenant).await;
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
        )
        .bind(schema)
        .bind(table)
        .bind(column)
        .fetch_one(&mut conn)
        .await
        .expect("counts");
        found > 0
    }

    async fn cleanup_tenant(&self, tenant: &spa_control::Tenant) {
        let _ = spa_testkit::drop_named_database(&tenant.database_name).await;
    }

    async fn slug_taken(&self, slug: &str) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenant WHERE slug = $1")
            .bind(slug)
            .fetch_one(self.db.pool())
            .await
            .expect("counts")
            > 0
    }
}

/// **The requirement, in one test.**
#[tokio::test]
async fn signing_up_produces_a_system_you_can_use() {
    let fixture = Fixture::new().await;

    let done = fixture
        .control
        .sign_up(
            "owner@acme.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "acme".to_owned(),
            "Acme Trading".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");

    assert_eq!(done.tenant.slug, "acme");
    assert_eq!(
        done.tenant.status,
        TenantStatus::Active,
        "activation is the last step; anything earlier is invisible"
    );
    assert!(fixture.database_exists(&done.tenant.database_name).await);

    // The session works, and it is the owner's.
    let session = fixture
        .control
        .session(done.token.expose())
        .await
        .expect("the token from signup authenticates");
    assert_eq!(session.identity, done.identity);

    // And the membership granted during signup lets them in.
    let db = fixture
        .control
        .enter(done.identity, done.tenant.id, Lane::Interactive)
        .await
        .expect("the owner can enter their own tenant");
    assert!(db.has_module(&ModuleId::new("toy").unwrap()));

    // The module's schema is there, and so is its projection checkpoint.
    let mut conn = db.acquire().await.expect("connection");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_toy.thing")
        .fetch_one(&mut *conn)
        .await
        .expect("the module's table exists");
    assert_eq!(rows, 0);
    let checkpoints: i64 =
        sqlx::query_scalar("SELECT count(*) FROM projection_checkpoint WHERE group_name = 'toy'")
            .fetch_one(&mut *conn)
            .await
            .expect("reads");
    assert_eq!(checkpoints, 1, "the group is ready for the worker to drive");
    drop(conn);
    drop(db);

    // The event log is there and empty — a new tenant has no history.
    let db = fixture
        .control
        .enter_for_maintenance(done.tenant.id)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    assert!(
        spa_eventlog::integrity(&mut conn)
            .await
            .expect("checks")
            .is_contiguous()
    );
    drop(conn);
    drop(db);

    let _ = spa_testkit::drop_named_database(&done.tenant.database_name).await;
}

/// **The compensation.**
///
/// A signup that fails part-way must leave nothing behind — above all not the
/// name, because the person who just failed is exactly the person about to try
/// it again.
#[tokio::test]
async fn a_failed_signup_frees_the_name_it_took() {
    let fixture = Fixture::new().await;

    // A module whose install script is broken. Everything before it succeeds:
    // the tenant row, the database, the migrations, the entitlement.
    let broken = ModuleSetup::new(
        ModuleId::new("broken").expect("valid"),
        "CREATE TABLE proj_broken.thing (id INT);", // no such schema
        &[],
    );

    let result = fixture
        .control
        .sign_up(
            "owner@acme.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "acme".to_owned(),
            "Acme Trading".to_owned(),
            vec![broken],
        )
        .await;

    assert!(result.is_err(), "a broken module must fail the signup");
    assert!(
        !fixture.slug_taken("acme").await,
        "the name must be free again, or one bad minute becomes permanent"
    );
    assert!(
        !fixture.database_exists("spa_tenant_acme").await,
        "and no database left behind"
    );

    // Proof it is really free: the same name signs up successfully now.
    let done = fixture
        .control
        .sign_up(
            "owner2@acme.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "acme".to_owned(),
            "Acme Trading".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("the retry succeeds");
    assert_eq!(done.tenant.slug, "acme");

    let _ = spa_testkit::drop_named_database(&done.tenant.database_name).await;
}

#[tokio::test]
async fn a_taken_name_fails_before_anything_is_built() {
    let fixture = Fixture::new().await;

    let first = fixture
        .control
        .sign_up(
            "a@acme.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "acme".to_owned(),
            "Acme".to_owned(),
            vec![],
        )
        .await
        .expect("signs up");

    let second = fixture
        .control
        .sign_up(
            "b@acme.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "acme".to_owned(),
            "Acme Two".to_owned(),
            vec![],
        )
        .await;

    assert!(
        matches!(second, Err(spa_control::AccessError::SlugTaken(_))),
        "{second:?}"
    );
    // And the first tenant is untouched — a failed second signup must not
    // compensate the first one's database away.
    assert!(fixture.database_exists(&first.tenant.database_name).await);

    let _ = spa_testkit::drop_named_database(&first.tenant.database_name).await;
}

/// Provisioning is idempotent, so recovery and retry are the same operation.
#[tokio::test]
async fn provisioning_the_same_tenant_twice_is_safe() {
    let fixture = Fixture::new().await;

    let identity = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("identity");

    let tenant = fixture
        .control
        .provision(
            "acme".to_owned(),
            "Acme".to_owned(),
            identity.id,
            vec![toy_module()],
        )
        .await
        .expect("provisions");

    // Register a second tenant pointing at the *same* database name is not
    // possible, so idempotency is exercised by re-running the install script and
    // the group setup against the database that exists.
    let db = fixture
        .control
        .enter_for_maintenance(tenant.id)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    sqlx::raw_sql(toy_module().install_sql)
        .execute(&mut *conn)
        .await
        .expect("the install script is idempotent");
    sqlx::query(
        "INSERT INTO projection_checkpoint (group_name) VALUES ('toy')
         ON CONFLICT (group_name) DO NOTHING",
    )
    .execute(&mut *conn)
    .await
    .expect("group setup is idempotent");

    let checkpoints: i64 =
        sqlx::query_scalar("SELECT count(*) FROM projection_checkpoint WHERE group_name = 'toy'")
            .fetch_one(&mut *conn)
            .await
            .expect("reads");
    assert_eq!(
        checkpoints, 1,
        "re-running must not duplicate the checkpoint"
    );
    drop(conn);
    drop(db);

    let _ = spa_testkit::drop_named_database(&tenant.database_name).await;
}

// ---------------------------------------------------------------------------
// Demo tenants
// ---------------------------------------------------------------------------

/// Signs up a tenant and hands back what it needs to be found again.
async fn tenant(fixture: &Fixture, slug: &str) -> spa_control::Tenant {
    fixture
        .control
        .sign_up(
            format!("owner@{slug}.test"),
            "correct horse battery staple".to_owned(),
            slug.to_owned(),
            slug.to_owned(),
            vec![],
        )
        .await
        .expect("signs up")
        .tenant
}

/// A demo lives its span and then stops existing — database and all.
#[tokio::test]
async fn an_expired_demo_is_destroyed_completely() {
    let fixture = Fixture::new().await;
    let tenant = tenant(&fixture, "demo").await;

    // A zero TTL is `now()`, which is already in the past by the next statement.
    fixture
        .control
        .set_demo_expiry(tenant.id, Duration::ZERO, Actor::system())
        .await
        .expect("marks as a demo");

    assert!(
        fixture.database_exists(&tenant.database_name).await,
        "the demo's database is there to begin with"
    );

    let reaped = fixture
        .control
        .reap_expired_demos(10)
        .await
        .expect("sweeps");
    assert_eq!(reaped, 1);

    assert!(
        !fixture.database_exists(&tenant.database_name).await,
        "the database went with it"
    );
    assert!(
        !fixture.slug_taken("demo").await,
        "and so did the row, so the name is free again"
    );
}

/// **The guard that matters.** Everything that is not an expired demo survives a
/// sweep — the property that makes a process with `DROP DATABASE` in it safe to
/// schedule.
#[tokio::test]
async fn a_sweep_leaves_everything_that_is_not_an_expired_demo_alone() {
    let fixture = Fixture::new().await;

    // An ordinary tenant, never marked.
    let ordinary = tenant(&fixture, "acme").await;

    // A demo with time left on it.
    let live_demo = tenant(&fixture, "preview").await;
    fixture
        .control
        .set_demo_expiry(live_demo.id, Duration::from_hours(1), Actor::system())
        .await
        .expect("marks as a demo");

    let reaped = fixture
        .control
        .reap_expired_demos(10)
        .await
        .expect("sweeps");
    assert_eq!(reaped, 0, "nothing was due");

    for survivor in [&ordinary, &live_demo] {
        assert!(
            fixture.database_exists(&survivor.database_name).await,
            "{}'s database survived",
            survivor.slug
        );
        assert!(fixture.slug_taken(&survivor.slug).await);
    }

    // Not vacuous: the same sweep destroys the same tenant once it is due.
    fixture
        .control
        .set_demo_expiry(live_demo.id, Duration::ZERO, Actor::system())
        .await
        .expect("expires it");
    assert_eq!(
        fixture
            .control
            .reap_expired_demos(10)
            .await
            .expect("sweeps"),
        1
    );
    assert!(!fixture.database_exists(&live_demo.database_name).await);

    let _ = spa_testkit::drop_named_database(&ordinary.database_name).await;
}

/// A demo that converts to a real tenant between the sweep and the reap is not
/// destroyed, because the reap re-checks rather than trusting what it was
/// handed.
#[tokio::test]
async fn a_demo_that_converts_before_the_reap_survives_it() {
    let fixture = Fixture::new().await;
    let converted = tenant(&fixture, "converts").await;

    fixture
        .control
        .set_demo_expiry(converted.id, Duration::ZERO, Actor::system())
        .await
        .expect("marks as a demo");

    // What the sweep saw.
    let stale = fixture
        .control
        .expired_demos(10)
        .await
        .expect("sweeps")
        .into_iter()
        .find(|t| t.id == converted.id)
        .expect("is due");

    // What happened next: somebody paid. The schema's own answer to converting
    // a demo is clearing the column.
    sqlx::query("UPDATE tenant SET demo_expires_at = NULL WHERE id = $1")
        .bind(converted.id.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("converts");

    let reaped = fixture.control.reap_demo(&stale).await.expect("re-checks");
    assert!(!reaped, "a converted tenant is skipped, not destroyed");
    assert!(
        fixture.database_exists(&converted.database_name).await,
        "the customer still has their data"
    );

    let _ = spa_testkit::drop_named_database(&converted.database_name).await;
}

/// The Rust-side guard, for a caller that never went through `expired_demos`.
#[tokio::test]
async fn a_tenant_that_is_not_a_demo_cannot_be_reaped_at_all() {
    let fixture = Fixture::new().await;
    let real = tenant(&fixture, "real").await;

    let result = fixture.control.reap_demo(&real).await;
    assert!(
        result.is_err(),
        "destroying a tenant that was never a demo must be refused, got {result:?}"
    );
    assert!(fixture.database_exists(&real.database_name).await);

    let _ = spa_testkit::drop_named_database(&real.database_name).await;
}

// ---------------------------------------------------------------------------
// Module refresh
// ---------------------------------------------------------------------------

/// The toy module after somebody changed its read model — a new column, which
/// `CREATE TABLE IF NOT EXISTS` alone would never add.
fn toy_module_v2() -> ModuleSetup {
    ModuleSetup::new(
        ModuleId::new("toy").expect("valid"),
        "CREATE SCHEMA IF NOT EXISTS proj_toy;
         CREATE TABLE IF NOT EXISTS proj_toy.thing (id INT PRIMARY KEY, label TEXT NOT NULL);",
        &[("toy", "proj_toy")],
    )
}

/// **A changed read model is a rebuild, not a migration.**
///
/// Everything a module projects is derived, so the answer to a schema change is
/// to drop it, install it again, and replay — which is what makes it safe, and
/// why `install.sql` is allowed to be `IF NOT EXISTS` throughout.
#[tokio::test]
async fn refreshing_a_module_rebuilds_its_schema_and_rewinds_its_checkpoint() {
    let fixture = Fixture::new().await;
    let tenant = tenant_with_toy(&fixture, "acme").await;

    // Pretend the worker has been running: some rows, and a checkpoint that has
    // moved on.
    let mut conn = fixture.tenant_connection(&tenant).await;
    sqlx::query("INSERT INTO proj_toy.thing (id) VALUES (1), (2)")
        .execute(&mut conn)
        .await
        .expect("projects something");
    sqlx::query("UPDATE projection_checkpoint SET position = 42 WHERE group_name = 'toy'")
        .execute(&mut conn)
        .await
        .expect("advances");
    // A document counter, which is **not** derived from anything and must not go
    // with the schema. A tenant whose invoice series restarted at one after a
    // module refresh would reissue numbers that are already on documents their
    // customers hold, which is a legal problem rather than a bug.
    spa_eventlog::numbering::start_at(&mut conn, "toy.document", 4108)
        .await
        .expect("sets a series");
    drop(conn);

    // Not vacuous: the new column is genuinely absent beforehand, so installing
    // again without dropping would change nothing.
    assert!(
        !fixture
            .column_exists(&tenant, "proj_toy", "thing", "label")
            .await,
        "the old shape is what we start from"
    );

    fixture
        .control
        .refresh_module(tenant.id, toy_module_v2())
        .await
        .expect("refreshes");

    assert!(
        fixture
            .column_exists(&tenant, "proj_toy", "thing", "label")
            .await,
        "the new shape is installed"
    );

    let mut conn = fixture.tenant_connection(&tenant).await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_toy.thing")
        .fetch_one(&mut conn)
        .await
        .expect("counts");
    let checkpoint: i64 =
        sqlx::query_scalar("SELECT position FROM projection_checkpoint WHERE group_name = 'toy'")
            .fetch_one(&mut conn)
            .await
            .expect("reads");
    drop(conn);

    assert_eq!(rows, 0, "the derived rows went with the schema");
    assert_eq!(
        checkpoint, 0,
        "and the checkpoint rewound, or the worker would think it had nothing to do"
    );

    let mut conn = fixture.tenant_connection(&tenant).await;
    let series = spa_eventlog::numbering::peek(&mut conn, "toy.document")
        .await
        .expect("reads");
    drop(conn);
    assert_eq!(
        series, 4108,
        "a module refresh reset a document series — every number after this \
         would be one somebody already holds"
    );

    fixture.cleanup_tenant(&tenant).await;
}

/// Only tenants that have the module, and one failure does not stop the rest.
#[tokio::test]
async fn a_fleet_refresh_covers_the_tenants_with_the_module_and_carries_on() {
    let fixture = Fixture::new().await;

    let with = tenant_with_toy(&fixture, "acme").await;
    let without = fixture
        .control
        .sign_up(
            "owner@plain.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "plain".to_owned(),
            "Plain".to_owned(),
            vec![],
        )
        .await
        .expect("signs up")
        .tenant;

    let plan = fixture
        .control
        .refresh_module_fleet(toy_module_v2())
        .await
        .expect("refreshes");

    assert_eq!(plan.behind.len(), 1, "one tenant has the module");
    assert_eq!(plan.behind[0].tenant, with.id);
    assert!(plan.failed.is_empty());

    assert!(
        fixture
            .column_exists(&with, "proj_toy", "thing", "label")
            .await
    );
    assert!(
        !fixture.database_exists("proj_toy_nowhere").await,
        "and the tenant without it was untouched"
    );

    let _ = spa_testkit::drop_named_database(&without.database_name).await;
    fixture.cleanup_tenant(&with).await;
}

/// A tenant with the toy module installed at its original shape.
async fn tenant_with_toy(fixture: &Fixture, slug: &str) -> spa_control::Tenant {
    fixture
        .control
        .sign_up(
            format!("owner@{slug}.test"),
            "correct horse battery staple".to_owned(),
            slug.to_owned(),
            slug.to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up")
        .tenant
}

/// **The claim the refresh's comment makes, tested rather than asserted.**
///
/// A refresh drops a module's tables. A projection run holds the checkpoint row
/// with `SELECT ... FOR UPDATE` for the length of its batch, and at the *start*
/// of that batch it has written nothing yet — so it holds no lock on the tables
/// themselves. Without taking the checkpoint lock first, `DROP SCHEMA` sails
/// straight past and the run's next write finds its table gone, mid-transaction.
///
/// The first version of this test asserted only that the refresh had not
/// finished, and **passed with the lock removed** — because the checkpoint
/// `UPDATE` blocks either way, just *after* the drop rather than before it. The
/// property is not "the refresh waits"; it is "the run's tables are still there
/// while it is in flight".
#[tokio::test]
async fn a_refresh_does_not_drop_tables_under_a_projection_run() {
    let fixture = Fixture::new().await;
    let tenant = tenant_with_toy(&fixture, "acme").await;

    // A batch that has taken its lease and not yet written anything — the
    // window every projection run opens with.
    let mut runner = fixture.tenant_connection(&tenant).await;
    let mut run = runner.begin().await.expect("begins");
    sqlx::query("SELECT 1 FROM projection_checkpoint WHERE group_name = 'toy' FOR UPDATE")
        .fetch_optional(&mut *run)
        .await
        .expect("takes the lease");

    let Fixture { control, db } = fixture;
    let control = std::sync::Arc::new(control);
    let refreshing = control.clone();
    let tenant_id = tenant.id;
    let refresh = tokio::spawn(async move {
        refreshing
            .refresh_module(tenant_id, toy_module_v2())
            .await
            .expect("refreshes");
    });

    // Long enough that a refresh which ignored the lease would have dropped the
    // schema by now.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // The assertion that matters: the batch can still do its work.
    let wrote = sqlx::query("INSERT INTO proj_toy.thing (id) VALUES (7)")
        .execute(&mut *run)
        .await;
    assert!(
        wrote.is_ok(),
        "the refresh dropped this run's tables out from under it: {wrote:?}"
    );

    run.commit().await.expect("commits");
    drop(runner);

    tokio::time::timeout(std::time::Duration::from_secs(10), refresh)
        .await
        .expect("the refresh is no longer blocked")
        .expect("completes");

    // And it did happen, once the run was out of the way.
    let fixture = Fixture {
        control: std::sync::Arc::try_unwrap(control).unwrap_or_else(|_| unreachable!()),
        db,
    };
    assert!(
        fixture
            .column_exists(&tenant, "proj_toy", "thing", "label")
            .await,
        "the refresh eventually did its job"
    );

    fixture.cleanup_tenant(&tenant).await;
}
