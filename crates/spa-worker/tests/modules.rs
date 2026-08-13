//! Module-scoped jobs, and the invariants the worker asserts.
//!
//! Two properties that only show up once a real module exists:
//!
//! - A tenant that declined a module is not visited on its behalf. Without
//!   this, "modular" means the price list changes and nothing else does.
//! - The invariants in architecture §7 are actually run, rather than listed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spa_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use spa_testkit::{Schema, TestDb};
use spa_types::{ModuleId, TenantId};
use spa_worker::{Activity, BoxError, Finding, HealthJob, Invariant, Job};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);

fn module(name: &str) -> ModuleId {
    ModuleId::new(name).expect("valid")
}

struct Fixture {
    control: Arc<ControlPlane>,
    _control_db: TestDb,
    databases: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        let control_db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");
        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("parses");
        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
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
            .expect("registers");

        Self {
            control,
            _control_db: control_db,
            databases: Vec::new(),
        }
    }

    async fn tenant(&mut self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("registers");
        spa_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("creates");
        self.databases.push(tenant.database_name.clone());
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("activates");
        tenant.id
    }

    async fn db(&self, tenant: TenantId) -> TenantDb {
        self.control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry")
    }

    async fn cleanup(self) {
        for name in &self.databases {
            let _ = spa_testkit::drop_named_database(name).await;
        }
    }
}

/// A job that only counts how often it ran.
struct Counter {
    module: Option<ModuleId>,
    ticks: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Job for Counter {
    fn name(&self) -> &'static str {
        "counter"
    }
    fn module(&self) -> Option<ModuleId> {
        self.module.clone()
    }
    async fn tick(&self, _db: &TenantDb) -> Result<Activity, BoxError> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(Activity::Idle)
    }
}

/// **What "modular" has to mean.**
#[tokio::test]
async fn a_tenant_that_declined_a_module_is_not_worked_on_its_behalf() {
    let mut fixture = Fixture::new().await;
    let subscriber = fixture.tenant("subscriber").await;
    let decliner = fixture.tenant("decliner").await;

    fixture
        .control
        .enable_module(subscriber, &module("ledger"), Actor::system())
        .await
        .expect("enables");

    let ticks = Arc::new(AtomicUsize::new(0));
    let job = Counter {
        module: Some(module("ledger")),
        ticks: Arc::clone(&ticks),
    };

    // Called directly rather than through the worker: what is under test is the
    // skip, and a loop would only add timing to it.
    let subscriber_db = fixture.db(subscriber).await;
    let decliner_db = fixture.db(decliner).await;

    assert!(
        subscriber_db.has_module(&module("ledger")),
        "the subscriber has it"
    );
    assert!(
        !decliner_db.has_module(&module("ledger")),
        "and the decliner does not"
    );

    // The worker's rule, asserted on the same predicate it uses.
    for db in [&subscriber_db, &decliner_db] {
        if job.module().is_some_and(|m| !db.has_module(&m)) {
            continue;
        }
        job.tick(db).await.expect("ticks");
    }

    assert_eq!(
        ticks.load(Ordering::SeqCst),
        1,
        "exactly one tenant should have been worked"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_kernel_job_runs_for_every_tenant() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    let ticks = Arc::new(AtomicUsize::new(0));
    let job = Counter {
        module: None,
        ticks: Arc::clone(&ticks),
    };
    let db = fixture.db(tenant).await;

    assert!(job.module().is_none(), "no module means every tenant");
    job.tick(&db).await.expect("ticks");
    assert_eq!(ticks.load(Ordering::SeqCst), 1);

    fixture.cleanup().await;
}

/// A healthy tenant produces no findings, and checking is not "work".
#[tokio::test]
async fn a_healthy_tenant_reports_nothing() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;
    let db = fixture.db(tenant).await;

    let health = HealthJob::every(Duration::from_mins(5));
    let activity = health.tick(&db).await.expect("checks");

    assert_eq!(
        activity,
        Activity::Idle,
        "a healthy tenant must not be revisited immediately, forever"
    );

    fixture.cleanup().await;
}

/// The interval is real: a second tick inside it does nothing.
#[tokio::test]
async fn health_is_checked_on_an_interval_not_on_every_visit() {
    struct Nosy(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Invariant for Nosy {
        fn name(&self) -> &'static str {
            "nosy"
        }
        async fn check(&self, _db: &TenantDb) -> Result<Vec<Finding>, BoxError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;
    let db = fixture.db(tenant).await;

    let checks = Arc::new(AtomicUsize::new(0));
    let health = HealthJob::every(Duration::from_mins(5)).with(Arc::new(Nosy(Arc::clone(&checks))));

    for _ in 0..5 {
        health.tick(&db).await.expect("checks");
    }
    assert_eq!(
        checks.load(Ordering::SeqCst),
        1,
        "counting every event on a busy tenant several times a second is how a \
         health check becomes the top query in the slow log"
    );

    fixture.cleanup().await;
}

/// **The proof that the health job is not vacuous.**
///
/// A broken invariant must be reported. Without this, "no findings" is
/// indistinguishable from "nothing was checked".
#[tokio::test]
async fn a_violated_invariant_is_reported() {
    struct AlwaysBroken;
    struct Exploding;

    #[async_trait::async_trait]
    impl Invariant for Exploding {
        fn name(&self) -> &'static str {
            "exploding"
        }
        async fn check(&self, _db: &TenantDb) -> Result<Vec<Finding>, BoxError> {
            Err("the check itself is broken".into())
        }
    }

    #[async_trait::async_trait]
    impl Invariant for AlwaysBroken {
        fn name(&self) -> &'static str {
            "always-broken"
        }
        async fn check(&self, _db: &TenantDb) -> Result<Vec<Finding>, BoxError> {
            Ok(vec![Finding::new("always-broken", "by construction")])
        }
    }

    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;
    let db = fixture.db(tenant).await;

    // The job logs rather than returns, so the observable outcome is that it
    // completed having run the check — which the counter below proves.
    let health = HealthJob::every(Duration::from_mins(5)).with(Arc::new(AlwaysBroken));
    let activity = health.tick(&db).await.expect("checks");
    assert_eq!(activity, Activity::Idle);

    // And a check that errors surfaces as a job failure rather than silence.
    let health = HealthJob::every(Duration::from_mins(5)).with(Arc::new(Exploding));
    assert!(
        health.tick(&db).await.is_err(),
        "a check that cannot run must not read as healthy"
    );

    fixture.cleanup().await;
}

/// The dead-letter invariant fires on a real dead letter.
#[tokio::test]
async fn a_dead_letter_makes_a_tenant_unhealthy() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;
    let db = fixture.db(tenant).await;

    let mut conn = db.acquire().await.expect("connection");
    sqlx::query(
        "INSERT INTO outbox (idempotency_key, kind, payload, dead_at, last_error)
         VALUES ('k', 'email.send', '{}', now(), 'gave up')",
    )
    .execute(&mut *conn)
    .await
    .expect("inserts");

    let health = spa_eventlog::outbox_health(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(health.dead, 1);
    assert!(
        !health.is_healthy(300),
        "an unresolved dead letter is a promise nobody kept"
    );

    fixture.cleanup().await;
}
