//! Self-service signup, end to end.
//!
//! The requirement is "anyone registering online can run their own system
//! without contacting us directly". What makes it hard is that it creates a
//! database, so it is not one transaction and it can fail half-done — which is
//! what [`a_failed_signup_frees_the_name_it_took`] is about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use erp_control::{
    Actor, ClusterRegistry, ControlPlane, Lane, ModuleSetup, PoolConfig, TenantPools, TenantStatus,
};
use erp_testkit::{Schema, TestDb};
use erp_types::ModuleId;
use sqlx::Connection as _;

/// A toy module writes no events, so it declares none. `Upcasters::new()` is the
/// honest answer rather than a stand-in for somebody else's.
fn no_events() -> &'static erp_eventlog::Upcasters {
    static NONE: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    NONE.get_or_init(erp_eventlog::Upcasters::new)
}

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);

/// A module with a trivial schema, so these tests do not depend on the ledger.
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
    db: TestDb,
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
    async fn tenant_connection(&self, tenant: &erp_control::Tenant) -> sqlx::PgConnection {
        use sqlx::Connection;
        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgConnection::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects")
    }

    async fn column_exists(
        &self,
        tenant: &erp_control::Tenant,
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

    async fn cleanup_tenant(&self, tenant: &erp_control::Tenant) {
        let _ = erp_testkit::drop_named_database(&tenant.database_name).await;
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
        erp_eventlog::integrity(&mut conn)
            .await
            .expect("checks")
            .is_contiguous()
    );
    drop(conn);
    drop(db);

    let _ = erp_testkit::drop_named_database(&done.tenant.database_name).await;
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
        no_events,
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
        !fixture.database_exists("erp_tenant_acme").await,
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

    let _ = erp_testkit::drop_named_database(&done.tenant.database_name).await;
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
        matches!(second, Err(erp_control::AccessError::SlugTaken(_))),
        "{second:?}"
    );
    // And the first tenant is untouched — a failed second signup must not
    // compensate the first one's database away.
    assert!(fixture.database_exists(&first.tenant.database_name).await);

    let _ = erp_testkit::drop_named_database(&first.tenant.database_name).await;
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

    let _ = erp_testkit::drop_named_database(&tenant.database_name).await;
}

// ---------------------------------------------------------------------------
// Demo tenants
// ---------------------------------------------------------------------------

/// Signs up a tenant and hands back what it needs to be found again.
async fn tenant(fixture: &Fixture, slug: &str) -> erp_control::Tenant {
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

    let _ = erp_testkit::drop_named_database(&ordinary.database_name).await;
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

    let _ = erp_testkit::drop_named_database(&converted.database_name).await;
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

    let _ = erp_testkit::drop_named_database(&real.database_name).await;
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
        no_events,
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
    erp_eventlog::numbering::start_at(&mut conn, "toy.document", 4108)
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
    let series = erp_eventlog::numbering::peek(&mut conn, "toy.document")
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

/// The toy module with data it cannot work without.
///
/// The insert names a table `install_sql` creates, so it can only succeed if the
/// seed runs **after** the install — which is the ordering, tested rather than
/// commented.
fn toy_module_seeded() -> ModuleSetup {
    toy_module().seeding(
        "INSERT INTO proj_toy.thing (id) VALUES (42) ON CONFLICT (id) DO NOTHING;
         INSERT INTO public.configuration (key, value, version, set_by)
         VALUES ('toy.rate', '{\"standard\":1500}'::jsonb,
                 nextval('public.configuration_version'), 'module:toy')
         ON CONFLICT (key) DO NOTHING;",
    )
}

/// **A module's seed runs, and it runs after its DDL.**
///
/// The Saudi VAT rate used to ride on `tax_sa`'s schema install, because that
/// was the only hook a module had. Splitting them meant `install_schema` grew a
/// second step — and a second step is a step that can be forgotten, dropped by a
/// refactor, or quietly skipped for an empty seed that was not actually empty.
///
/// It writes both a module table and the tenant's `public.configuration`, which
/// is what the Saudi rate does and the reason the seed runs under the module's
/// `search_path` rather than at `public`.
#[tokio::test]
async fn a_modules_seed_runs_when_a_tenant_gets_the_module() {
    let fixture = Fixture::new().await;
    let tenant = fixture
        .control
        .sign_up(
            "owner@seeded.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "seeded".to_owned(),
            "Seeded".to_owned(),
            vec![toy_module_seeded()],
        )
        .await
        .expect("signs up")
        .tenant;

    let mut conn = fixture.tenant_connection(&tenant).await;
    let thing: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_toy.thing WHERE id = 42")
        .fetch_one(&mut conn)
        .await
        .expect("counts");
    let setting: i64 =
        sqlx::query_scalar("SELECT count(*) FROM configuration WHERE key = 'toy.rate'")
            .fetch_one(&mut conn)
            .await
            .expect("counts");
    drop(conn);

    assert_eq!(thing, 1, "the seed did not run, or ran before the DDL");
    assert_eq!(
        setting, 1,
        "the seed ran somewhere `public.configuration` was not reachable"
    );

    fixture.cleanup_tenant(&tenant).await;
}

/// **A rebuild seeds again, and does not overwrite what the tenant changed.**
///
/// `refresh_module` drops the module's schema and installs it from scratch, so
/// its seed has to run again or the module comes back missing the data it cannot
/// work without. The tenant's `public.configuration` is *not* dropped, so the
/// same seed meets a row it already wrote — which is why every seed is written
/// `ON CONFLICT DO NOTHING`, and why a business that corrected the Saudi rate
/// keeps their correction.
#[tokio::test]
async fn a_rebuild_seeds_again_without_overwriting_the_tenants_own_value() {
    let fixture = Fixture::new().await;
    let tenant = fixture
        .control
        .sign_up(
            "owner@reseeded.test".to_owned(),
            "correct horse battery staple".to_owned(),
            "reseeded".to_owned(),
            "Reseeded".to_owned(),
            vec![toy_module_seeded()],
        )
        .await
        .expect("signs up")
        .tenant;

    // The tenant disagrees with the module about the number.
    let mut conn = fixture.tenant_connection(&tenant).await;
    sqlx::query(
        "UPDATE configuration SET value = '{\"standard\":500}'::jsonb WHERE key = 'toy.rate'",
    )
    .execute(&mut conn)
    .await
    .expect("corrects the rate");
    drop(conn);

    fixture
        .control
        .refresh_module(tenant.id, toy_module_seeded())
        .await
        .expect("refreshes");

    let mut conn = fixture.tenant_connection(&tenant).await;
    let thing: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_toy.thing WHERE id = 42")
        .fetch_one(&mut conn)
        .await
        .expect("counts");
    let rate: serde_json::Value =
        sqlx::query_scalar("SELECT value FROM configuration WHERE key = 'toy.rate'")
            .fetch_one(&mut conn)
            .await
            .expect("reads");
    drop(conn);

    assert_eq!(thing, 1, "the rebuild left the module without its own data");
    assert_eq!(
        rate["standard"], 500,
        "the rebuild stamped over a value the tenant set"
    );

    fixture.cleanup_tenant(&tenant).await;
}

/// A tenant with the toy module installed at its original shape.
async fn tenant_with_toy(fixture: &Fixture, slug: &str) -> erp_control::Tenant {
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

// ---------------------------------------------------------------------------
// Databases no tenant row claims
// ---------------------------------------------------------------------------

/// The grace arithmetic, over the rule that decides what may be dated at all.
///
/// **The rule itself is tested where it lives** —
/// `erp_types::TenantId::named_in_database`, beside the id whose shape it
/// reads. This checks only what this crate adds: that a name minted now is
/// inside the grace and one minted yesterday is outside it.
#[test]
fn a_fresh_name_is_inside_the_grace_and_an_old_one_is_not() {
    let fresh = format!("erp_tenant_{}", uuid::Uuid::now_v7().simple());
    let age = erp_control::orphan_age_seconds_for_tests(&fresh).expect("readable");
    assert!(age < 5, "a name minted now read as {age} seconds old");
    assert!(age < erp_control::ORPHAN_GRACE_SECONDS);

    let old = uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
        uuid::NoContext,
        u64::try_from(chrono::Utc::now().timestamp() - 36 * 60 * 60).expect("after 1970"),
        0,
    ));
    assert!(
        erp_control::orphan_age_seconds_for_tests(&format!("erp_tenant_{}", old.simple()))
            .expect("readable")
            >= erp_control::ORPHAN_GRACE_SECONDS
    );

    // And a name it refuses to date has no age to compare at all.
    assert!(erp_control::orphan_age_seconds_for_tests("erp_tenant_backup").is_none());
}

/// **Something created moments ago is not a finding.**
///
/// The grace is a noise filter, not a safety margin — nothing here deletes. A
/// database that appeared seconds ago and is not yet in the control plane is a
/// race with whoever is looking, and reporting it would train an operator to
/// ignore the message.
#[tokio::test]
async fn a_fresh_unclaimed_database_is_not_reported() {
    let fixture = Fixture::new().await;

    // Precisely the state a provisioning is in a millisecond before it writes
    // its row: the real naming, no row, seconds old.
    let in_flight = format!("erp_tenant_{}", uuid::Uuid::now_v7().simple());
    erp_testkit::create_named_database(&in_flight, &CONTROL)
        .await
        .expect("created");

    let found = fixture
        .control
        .find_orphaned_databases("primary", erp_control::ORPHAN_GRACE_SECONDS)
        .await
        .expect("the check runs");

    assert!(
        !found.iter().any(|u| u.database() == in_flight),
        "something created seconds ago was reported as unclaimed: {found:?}"
    );

    let _ = erp_testkit::drop_named_database(&in_flight).await;
}

/// **A claimed database is never reported, and an unclaimed one is.**
///
/// Run at zero grace so the age filter is switched off and the claim is the
/// only thing separating the two — which is the distinction that matters.
///
/// **Nothing is dropped by any of this.** The first version of this test ran a
/// destructive sweep at zero grace against the shared test cluster, which meant
/// it deleted other tests' tenant databases whenever the suite ran in parallel.
/// It passed while doing it, because it only asserted about the two databases
/// it knew of.
#[tokio::test]
async fn an_unclaimed_database_is_reported_and_a_claimed_one_is_not() {
    let fixture = Fixture::new().await;
    let signed_up = fixture
        .control
        .sign_up(
            "sara@bassat.test".to_owned(),
            "hunter2hunter2".to_owned(),
            "bassat".to_owned(),
            "Bassat".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");
    let tenant = fixture
        .control
        .tenant(signed_up.tenant.id)
        .await
        .expect("reads")
        .expect("exists");

    // Beside it: the right shape, old enough, and named by nothing.
    let unclaimed = format!(
        "erp_tenant_{}",
        // **A fresh name, not a backdated one.** These call the check with a
        // grace of zero, so age is not what makes it a candidate — and a name
        // backdated past `TENANT_SWEEP_GRACE_MILLIS` is one the harness's own
        // sweep would drop out from under this test.
        uuid::Uuid::now_v7().simple()
    );
    erp_testkit::create_named_database(&unclaimed, &CONTROL)
        .await
        .expect("created");

    let found = fixture
        .control
        .find_orphaned_databases("primary", 0)
        .await
        .expect("the check runs");

    assert!(
        found
            .iter()
            .any(|u| matches!(u, erp_control::Unclaimed::Empty(d) if *d == unclaimed)),
        "an unclaimed, empty database was not reported as empty: {found:?}"
    );
    assert!(
        !found.iter().any(|u| u.database() == tenant.database_name),
        "a live tenant's database was reported as unclaimed: {found:?}"
    );
    // And both are still there, because this reports and does not delete.
    assert!(fixture.database_exists(&unclaimed).await);
    assert!(fixture.database_exists(&tenant.database_name).await);

    let _ = erp_testkit::drop_named_database(&unclaimed).await;
    fixture.cleanup_tenant(&tenant).await;
}

/// A direct connection to a tenant database by name.
async fn connect_named(database: &str) -> sqlx::PgConnection {
    use sqlx::Connection as _;
    let url = erp_testkit::database_url();
    let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
    sqlx::PgConnection::connect(&format!("{base}/{database}"))
        .await
        .expect("connects")
}

/// Appends `n` events to a tenant's log, so there is something to lose.
///
/// Through `append`, not an `INSERT`: the log's shape is its own business and a
/// test that wrote rows by hand would be testing a different table.
async fn write_events(database: &str, n: i64) {
    use erp_eventlog::Metadata;
    use erp_eventlog::{NewEvent, append};
    use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence, StreamId};

    let mut conn = connect_named(database).await;
    let stream = StreamId::new(
        DomainName::new("toy").expect("valid"),
        AggregateId::new("thing").expect("valid"),
    );
    for i in 0..n {
        append(
            &mut conn,
            &stream,
            Sequence::new(i).expect("valid"),
            &[NewEvent::new(
                EventName::new("toy.happened").expect("valid"),
                SchemaVersion::new(1).expect("valid"),
                serde_json::json!({ "n": i }),
            )],
            &Metadata::default(),
        )
        .await
        .expect("appends");
    }
}

async fn event_count(database: &str) -> i64 {
    let mut conn = connect_named(database).await;
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM event")
        .fetch_one(&mut conn)
        .await
        .expect("counts")
}

/// **The data loss this feature was reverted for, now refused.**
///
/// A tenant that has been running is restored away — the control plane rolls
/// back to a point before its row existed. The database is untouched and full
/// of events, which is precisely the state
/// `restore.rs::a_tenant_database_without_its_control_row_is_unreachable` calls
/// dangerous.
///
/// The first version of this sweep destroyed it: the name was right, the age
/// was old, no row claimed it, nothing was connected. It asks the database now,
/// and the database says whose it is.
///
/// **And one occupied database refuses the whole cluster**, empty ones
/// included. A tenant with data and no row does not mean one row was lost; it
/// means the control plane is not a description of this cluster, and the next
/// name in the list is not evidence of anything either.
#[tokio::test]
async fn a_tenant_whose_control_row_was_lost_is_never_dropped() {
    let fixture = Fixture::new().await;
    let signed_up = fixture
        .control
        .sign_up(
            "sara@bassat.test".to_owned(),
            "hunter2hunter2".to_owned(),
            "bassat".to_owned(),
            "Bassat".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");
    let database = signed_up.tenant.database_name.clone();
    write_events(&database, 3).await;

    // Also an empty leftover, so the refusal can be shown to cover it too.
    let empty = format!(
        "erp_tenant_{}",
        // **A fresh name, not a backdated one.** These call the check with a
        // grace of zero, so age is not what makes it a candidate — and a name
        // backdated past `TENANT_SWEEP_GRACE_MILLIS` is one the harness's own
        // sweep would drop out from under this test.
        uuid::Uuid::now_v7().simple()
    );
    erp_testkit::create_named_database(&empty, &CONTROL)
        .await
        .expect("created");

    // The control plane restored to a point before this tenant existed.
    sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(signed_up.tenant.id.as_uuid())
        .execute(fixture.control.pool())
        .await
        .expect("deletes the control row");

    let found = fixture
        .control
        .find_orphaned_databases("primary", 0)
        .await
        .expect("the check runs");
    assert!(
        found.iter().any(|u| matches!(
            u,
            erp_control::Unclaimed::Occupied { database: d, .. } if *d == database
        )),
        "a database with three events in it was not called occupied: {found:?}"
    );

    let refused = fixture
        .control
        .drop_empty_orphans("primary", 0, 100)
        .await
        .expect_err("a cluster with an occupied orphan is not swept");
    assert!(
        matches!(refused, erp_control::AccessError::Corrupt(_)),
        "{refused:?}"
    );

    // Both are still there: the tenant's, and the empty one the refusal covered.
    assert!(
        fixture.database_exists(&database).await,
        "a restored-away tenant's database was destroyed"
    );
    assert_eq!(event_count(&database).await, 3, "its events are gone");
    assert!(fixture.database_exists(&empty).await);

    let _ = erp_testkit::drop_named_database(&empty).await;
    let _ = erp_testkit::drop_named_database(&database).await;
}

/// **A setting somebody chose is not rubbish either**, even with no events.
///
/// A module's own seed comes back by itself — `install` and `refresh_module`
/// both write it, marked `module:`. A rate a business corrected does not, and
/// nothing else in the system remembers it.
#[tokio::test]
async fn a_setting_somebody_chose_keeps_a_database_alive() {
    let fixture = Fixture::new().await;
    let signed_up = fixture
        .control
        .sign_up(
            "sara@bassat.test".to_owned(),
            "hunter2hunter2".to_owned(),
            "bassat".to_owned(),
            "Bassat".to_owned(),
            vec![toy_module()],
        )
        .await
        .expect("signs up");
    let database = signed_up.tenant.database_name.clone();

    {
        use sqlx::Connection as _;
        let mut conn = fixture.tenant_connection(&signed_up.tenant).await;
        // What a module install writes: recreated by `refresh_module`.
        sqlx::query(
            "INSERT INTO public.configuration (key, value, version, set_by)
             VALUES ('toy.seeded', '{}'::jsonb, nextval('public.configuration_version'),
                     'module:toy')",
        )
        .execute(&mut conn)
        .await
        .expect("seeds");
        conn.close().await.ok();
    }

    sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(signed_up.tenant.id.as_uuid())
        .execute(fixture.control.pool())
        .await
        .expect("deletes the control row");

    // A module's own seed does not make it somebody's.
    let found = fixture
        .control
        .find_orphaned_databases("primary", 0)
        .await
        .expect("the check runs");
    assert!(
        found.iter().any(|u| matches!(
            u,
            erp_control::Unclaimed::Empty(d) if *d == database
        )),
        "a database holding only module seeds was not called empty: {found:?}"
    );

    // Now a person sets one.
    {
        use sqlx::Connection as _;
        let mut conn = connect_named(&database).await;
        sqlx::query(
            "INSERT INTO public.configuration (key, value, version, set_by)
             VALUES ('ledger.vat_rates', '{\"standard\":500}'::jsonb,
                     nextval('public.configuration_version'),
                     '018f5c1e-0000-7000-8000-000000000000')",
        )
        .execute(&mut conn)
        .await
        .expect("sets");
        conn.close().await.ok();
    }

    let found = fixture
        .control
        .find_orphaned_databases("primary", 0)
        .await
        .expect("the check runs");
    assert!(
        found.iter().any(|u| matches!(
            u,
            erp_control::Unclaimed::Occupied { database: d, why }
                if *d == database && why.contains("setting")
        )),
        "a rate a business corrected did not keep its database alive: {found:?}"
    );

    let _ = erp_testkit::drop_named_database(&database).await;
}

/// **Not knowing is not the same as knowing it is empty.**
///
/// A database that cannot be opened tells this nothing, and nothing is not
/// evidence of rubbish. `ALLOW_CONNECTIONS false` is a real state — an operator
/// locking one during a restore, which is exactly when the control plane is
/// least likely to be a description of the cluster.
#[tokio::test]
async fn a_database_that_cannot_be_opened_is_never_dropped() {
    let fixture = Fixture::new().await;
    let shut = format!(
        "erp_tenant_{}",
        // **A fresh name, not a backdated one.** These call the check with a
        // grace of zero, so age is not what makes it a candidate — and a name
        // backdated past `TENANT_SWEEP_GRACE_MILLIS` is one the harness's own
        // sweep would drop out from under this test.
        uuid::Uuid::now_v7().simple()
    );
    erp_testkit::create_named_database(&shut, &CONTROL)
        .await
        .expect("created");

    // Closed to everybody, superusers included.
    // The name is a UUIDv7 this test just minted, so `AssertSqlSafe` is true.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER DATABASE \"{shut}\" WITH ALLOW_CONNECTIONS false"
    )))
    .execute(fixture.control.pool())
    .await
    .expect("closes it");

    let found = fixture
        .control
        .find_orphaned_databases("primary", 0)
        .await
        .expect("the check runs");
    assert!(
        found.iter().any(|u| matches!(
            u,
            erp_control::Unclaimed::Unreadable { database: d, .. } if *d == shut
        )),
        "a database that cannot be opened was not called unreadable: {found:?}"
    );

    let refused = fixture
        .control
        .drop_empty_orphans("primary", 0, 100)
        .await
        .expect_err("a cluster holding one it cannot read is not swept");
    assert!(
        matches!(refused, erp_control::AccessError::Corrupt(_)),
        "{refused:?}"
    );
    assert!(fixture.database_exists(&shut).await);

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER DATABASE \"{shut}\" WITH ALLOW_CONNECTIONS true"
    )))
    .execute(fixture.control.pool())
    .await
    .ok();
    let _ = erp_testkit::drop_named_database(&shut).await;
}
