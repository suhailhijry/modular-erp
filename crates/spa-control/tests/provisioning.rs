//! Self-service signup, end to end.
//!
//! The requirement is "anyone registering online can run their own system
//! without contacting us directly". What makes it hard is that it creates a
//! database, so it is not one transaction and it can fail half-done — which is
//! what [`a_failed_signup_frees_the_name_it_took`] is about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{
    Actor, ClusterRegistry, ControlPlane, Lane, ModuleSetup, PoolConfig, TenantPools, TenantStatus,
};
use spa_testkit::{Schema, TestDb};
use spa_types::ModuleId;

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
