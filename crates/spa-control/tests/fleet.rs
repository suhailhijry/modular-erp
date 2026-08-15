//! Migrating every tenant that already exists.
//!
//! The test that carries this file is
//! [`a_tenant_behind_the_fleet_is_found_and_brought_current`]. It builds a
//! tenant whose database has never been migrated at all — the state every
//! existing tenant would be in the day a new tenant-plane migration ships — and
//! checks that a survey sees it and a migration fixes it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use spa_testkit::{Schema, TestDb};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);
/// A tenant database that has never had a migration run against it.
static UNMIGRATED: Schema = Schema::sql("unmigrated", &[]);

struct Fixture {
    control: ControlPlane,
    db: TestDb,
    databases: Vec<String>,
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

        Self {
            control,
            db,
            databases: Vec::new(),
        }
    }

    /// A tenant whose database is built from `schema` — which is how a tenant
    /// three migrations behind is produced without faking `_sqlx_migrations`.
    async fn tenant_with(&mut self, slug: &str, schema: &Schema) -> spa_control::Tenant {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("registers");
        spa_testkit::create_named_database(&tenant.database_name, schema)
            .await
            .expect("creates the database");
        self.databases.push(tenant.database_name.clone());
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
        tenant
    }

    /// A tenant row with no database behind it, for the unreachable case.
    async fn tenant_without_a_database(&self, slug: &str) -> spa_control::Tenant {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("registers");
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
        tenant
    }

    async fn cleanup(self) {
        for name in &self.databases {
            let _ = spa_testkit::drop_named_database(name).await;
        }
    }
}

/// **The requirement.** A tenant whose database predates a migration is found
/// by a survey and brought current by a run.
#[tokio::test]
async fn a_tenant_behind_the_fleet_is_found_and_brought_current() {
    let mut fixture = Fixture::new().await;

    fixture.tenant_with("uptodate", &TENANT).await;
    let behind = fixture.tenant_with("behind", &UNMIGRATED).await;

    let latest = ControlPlane::latest_tenant_migration();
    assert!(
        latest > 0,
        "this build expects a real migration version, or every tenant looks current"
    );

    let plan = fixture.control.survey_fleet().await.expect("surveys");
    assert!(!plan.is_uniform(), "one tenant is behind");
    assert_eq!(plan.current.len(), 1);
    assert_eq!(plan.behind.len(), 1);
    assert_eq!(plan.behind[0].tenant, behind.id);
    assert_eq!(
        plan.behind[0].version, None,
        "a database with no migrations table reports `None`, not version zero"
    );
    assert!(plan.failed.is_empty());

    // A survey changes nothing, which is what makes it safe to run before a
    // deploy rather than after.
    let again = fixture.control.survey_fleet().await.expect("surveys");
    assert_eq!(again.behind.len(), 1);

    let done = fixture.control.migrate_fleet().await.expect("migrates");
    assert_eq!(
        done.behind.len(),
        1,
        "reports what it had to do, not what it found afterwards"
    );
    assert!(done.failed.is_empty());

    // And now the fleet agrees with this build.
    let after = fixture.control.survey_fleet().await.expect("surveys");
    assert!(after.is_uniform(), "{after:?}");
    assert_eq!(after.current.len(), 2);
    assert!(after.current.iter().all(|t| t.version == Some(latest)));

    // Running it again is a no-op rather than an error — which is what makes a
    // failed run resumable by simply running it again.
    let repeat = fixture.control.migrate_fleet().await.expect("migrates");
    assert!(repeat.behind.is_empty());
    assert_eq!(repeat.current.len(), 2);

    fixture.cleanup().await;
}

/// **One unreachable tenant must not leave the rest of the fleet un-migrated.**
///
/// The property most likely to be got wrong, and the one that decides whether a
/// migration is a deploy step or an incident.
#[tokio::test]
async fn a_tenant_that_cannot_be_reached_does_not_stop_the_run() {
    let mut fixture = Fixture::new().await;

    // Ordered by `created_at`, so the broken one is visited first and the two
    // after it prove the walk carried on.
    let broken = fixture.tenant_without_a_database("ghost").await;
    fixture.tenant_with("behind", &UNMIGRATED).await;
    fixture.tenant_with("uptodate", &TENANT).await;

    let plan = fixture
        .control
        .migrate_fleet()
        .await
        .expect("does not fail");

    assert_eq!(plan.failed.len(), 1);
    assert_eq!(plan.failed[0].0, broken.id);
    assert_eq!(
        plan.behind.len(),
        1,
        "the tenant after the failure was migrated anyway"
    );
    assert_eq!(plan.current.len(), 1);
    assert_eq!(plan.total(), 3);
    assert!(
        !plan.is_uniform(),
        "a tenant nobody could reach is not a migrated tenant, so a deploy gate must say no"
    );

    // Not vacuous: with the broken tenant gone the same fleet is uniform.
    sqlx::query("DELETE FROM tenant WHERE id = $1")
        .bind(broken.id.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("removes the ghost");

    let plan = fixture.control.migrate_fleet().await.expect("migrates");
    assert!(plan.is_uniform(), "{plan:?}");

    fixture.cleanup().await;
}

/// Suspended tenants are migrated; half-built ones are left alone.
///
/// A suspended tenant is one that may come back, and coming back to a schema
/// three versions behind is the failure this whole thing exists to prevent. A
/// `provisioning` one has no finished database and is somebody else's problem
/// right now.
#[tokio::test]
async fn suspended_tenants_are_migrated_and_half_built_ones_are_skipped() {
    let mut fixture = Fixture::new().await;

    let suspended = fixture.tenant_with("paused", &UNMIGRATED).await;
    sqlx::query("UPDATE tenant SET status = 'suspended' WHERE id = $1")
        .bind(suspended.id.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("suspends");

    // Registered and never activated: still `provisioning`.
    let half_built = fixture
        .control
        .register_tenant_on("halfbuilt", "Half Built", "primary", Actor::system())
        .await
        .expect("registers");

    let plan = fixture.control.migrate_fleet().await.expect("migrates");

    assert_eq!(plan.total(), 1, "only the suspended tenant was visited");
    assert_eq!(plan.behind.len(), 1);
    assert_eq!(plan.behind[0].tenant, suspended.id);
    assert!(
        !plan.failed.iter().any(|(id, _)| *id == half_built.id),
        "a half-built tenant is skipped, not reported as a failure"
    );

    let after = fixture.control.survey_fleet().await.expect("surveys");
    assert!(
        after.is_uniform(),
        "the suspended tenant was brought current"
    );

    fixture.cleanup().await;
}
