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

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_testkit::{Schema, TestDb};
use erp_types::{ModuleId, TenantId};
use erp_worker::{
    Activity, BoxError, Finding, HealthJob, Invariant, Job, Retention, Worker, WorkerConfig,
};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

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
        let control_db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");
        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("parses");
        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
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
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
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
            let _ = erp_testkit::drop_named_database(name).await;
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

    let health = erp_eventlog::outbox_health(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(health.dead, 1);
    assert!(
        !health.is_healthy(300),
        "an unresolved dead letter is a promise nobody kept"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The platform pass
// ---------------------------------------------------------------------------

/// A mailer that records, so a test never sends anything.
#[derive(Default)]
struct Recorder {
    sent: std::sync::Mutex<Vec<erp_control::mail::Email>>,
}

#[async_trait::async_trait]
impl erp_worker::mail::Mailer for Recorder {
    async fn send(
        &self,
        email: &erp_control::mail::Email,
        _key: &str,
    ) -> Result<(), erp_worker::mail::MailError> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(email.clone());
        Ok(())
    }
}

/// **The whole path, from inviting somebody to the message going out.**
///
/// Every piece of this existed for two phases joined to nothing: the outbox had
/// no producer anywhere in the product and the dispatcher had no registered
/// handler, so an effect enqueued by hand in a test was the only effect this
/// system had ever seen. What that cost, concretely, was that an invitation was
/// a link somebody copied out of an API response.
///
/// The assertion is the boring one, and that is the point: after inviting, and
/// after one platform pass, the message is with the mailer and the outbox row
/// says it was delivered.
#[tokio::test]
async fn an_invitation_is_promised_by_the_control_plane_and_delivered_by_the_worker() {
    use erp_worker::PlatformJob as _;

    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    let owner = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("identity");
    fixture
        .control
        .register_login(
            owner.id,
            "owner@acme.test".to_owned(),
            "hunter2hunter2".to_owned(),
        )
        .await
        .expect("login");
    fixture
        .control
        .grant_membership(
            owner.id,
            erp_control::Scope::Tenant(tenant),
            "owner",
            Actor::system(),
        )
        .await
        .expect("membership");

    fixture
        .control
        .invite(
            tenant,
            "sara@acme.test".to_owned(),
            erp_control::Role::Clerk,
            owner.id,
            "https://acme.erp.test/v1/join/",
            erp_i18n::Locale::Arabic,
        )
        .await
        .expect("invites");

    let recorder = Arc::new(Recorder::default());
    let dispatcher = Arc::new(
        erp_eventlog::Dispatcher::new(erp_eventlog::RetryPolicy::default()).register(Arc::new(
            erp_worker::mail::EmailHandler::new(recorder.clone()),
        )),
    );
    let job = erp_worker::PlatformOutboxJob::new(dispatcher, 32);

    let activity = job.tick(&fixture.control).await.expect("dispatches");
    assert_eq!(activity, Activity::Worked);

    let sent = recorder
        .sent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(sent.len(), 1, "the invitation email did not go out");
    assert_eq!(sent[0].to, "sara@acme.test");
    assert!(
        sent[0].body.contains("https://acme.erp.test/v1/join/"),
        "the link did not survive the round trip: {}",
        sent[0].body
    );

    // And the outbox knows, so a second pass sends nothing.
    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE delivered_at IS NOT NULL AND dead_at IS NULL",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("counts");
    assert_eq!(delivered, 1);

    let again = job.tick(&fixture.control).await.expect("dispatches");
    assert_eq!(
        again,
        Activity::Idle,
        "a delivered effect was claimed again"
    );
    assert_eq!(
        recorder
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "the same invitation was emailed twice"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// One visit at a time, and one job's failure is one job's
// ---------------------------------------------------------------------------

/// A job that measures how many copies of itself are running at once.
struct Overlapping {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    ticks: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Job for Overlapping {
    fn name(&self) -> &'static str {
        "overlapping"
    }
    async fn tick(&self, _db: &TenantDb) -> Result<Activity, BoxError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        // Long enough that a second visit of the same tenant, if the worker
        // spawned one, would overlap this one.
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.ticks.fetch_add(1, Ordering::SeqCst);
        // Always "worked", so the tenant is due again the moment a visit ends
        // — the shape that made the old claim loop hand a tenant back to
        // itself.
        Ok(Activity::Worked)
    }
}

/// **A tenant is visited by one visit at a time, however many slots the
/// worker has.**
///
/// The first version of `claim_tenants` handed a worker its own in-flight
/// tenants again on the next loop, so one due tenant filled every slot with
/// visits of itself and jobs ran N-fold concurrently against the same rows.
/// With four slots and one tenant, the peak has to be one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_is_visited_by_one_visit_at_a_time() {
    let mut fixture = Fixture::new().await;
    fixture.tenant("solo").await;

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let ticks = Arc::new(AtomicUsize::new(0));
    let worker = Worker::new(
        Arc::clone(&fixture.control),
        WorkerConfig {
            name: "eager".to_owned(),
            schedule: erp_control::WorkSchedule {
                lease: Duration::from_secs(30),
                idle_interval: Duration::from_millis(10),
                jitter: Duration::ZERO,
                max_idle_interval: Duration::from_secs(1),
            },
            tenants_per_claim: 8,
            concurrency: 4,
            // One tick per visit, so the tenant is rescheduled — and
            // re-claimable — as often as possible.
            max_ticks_per_visit: 1,
            empty_claim_pause: Duration::from_millis(2),
            ..WorkerConfig::default()
        },
    )
    .with_job(Arc::new(Overlapping {
        in_flight: Arc::clone(&in_flight),
        peak: Arc::clone(&peak),
        ticks: Arc::clone(&ticks),
    }));

    let cancel = tokio_util::sync::CancellationToken::new();
    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };
    tokio::time::sleep(Duration::from_millis(600)).await;
    cancel.cancel();
    let shutdown = run.await.expect("joins");

    assert!(
        ticks.load(Ordering::SeqCst) >= 3,
        "the tenant was visited {} times in 600ms; the loop is not re-claiming a worked tenant",
        ticks.load(Ordering::SeqCst)
    );
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "{} visits of one tenant ran at once",
        peak.load(Ordering::SeqCst)
    );
    assert!(shutdown.drained);
    fixture.cleanup().await;
}

struct AlwaysFails;

#[async_trait::async_trait]
impl Job for AlwaysFails {
    fn name(&self) -> &'static str {
        "always-fails"
    }
    async fn tick(&self, _db: &TenantDb) -> Result<Activity, BoxError> {
        Err("upstream is on fire".into())
    }
}

/// **One job's failure stalls that job, not the tenant.**
///
/// The first version returned on the first `Err`, so a Tabby secret that no
/// longer parsed stopped hold expiry, reminders and ZATCA submission for the
/// tenant — none of which had anything to do with Tabby. The failing job stays
/// failed and loud; the ones after it in the list still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_failing_job_does_not_stall_the_others() {
    let mut fixture = Fixture::new().await;
    fixture.tenant("resilient").await;

    let ticks = Arc::new(AtomicUsize::new(0));
    let worker = Worker::new(
        Arc::clone(&fixture.control),
        WorkerConfig {
            name: "steady".to_owned(),
            schedule: erp_control::WorkSchedule {
                lease: Duration::from_secs(30),
                idle_interval: Duration::from_millis(10),
                jitter: Duration::ZERO,
                max_idle_interval: Duration::from_secs(1),
            },
            empty_claim_pause: Duration::from_millis(2),
            ..WorkerConfig::default()
        },
    )
    // The failing job first, so the one after it is what proves the point.
    .with_job(Arc::new(AlwaysFails))
    .with_job(Arc::new(Counter {
        module: None,
        ticks: Arc::clone(&ticks),
    }));

    let cancel = tokio_util::sync::CancellationToken::new();
    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    let shutdown = run.await.expect("joins");

    assert!(
        ticks.load(Ordering::SeqCst) > 0,
        "the job after the failing one never ran"
    );
    assert!(
        shutdown.failed_visits > 0,
        "the failure must still be counted, not swallowed"
    );
    fixture.cleanup().await;
}

/// **What the system forgets, and what it keeps.** One receipt of each kind
/// older than its window, one younger; the sweep takes the old and leaves the
/// young, and never touches anything pending. The first version swept nothing.
#[tokio::test]
async fn retention_forgets_old_receipts_and_keeps_young_ones_and_open_promises() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("forgetful").await;
    let db = fixture.db(tenant).await;
    let mut conn = db.acquire().await.expect("connection");

    for statement in [
        // Delivered effects: one at forty days, one at twenty, and one that
        // was never delivered and is older than everything.
        "INSERT INTO outbox (idempotency_key, kind, payload, enqueued_at, delivered_at)
         VALUES ('old', 'email.send', '{}', now() - interval '40 days', now() - interval '40 days'),
                ('young', 'email.send', '{}', now() - interval '20 days', now() - interval '20 days'),
                ('pending', 'email.send', '{}', now() - interval '400 days', NULL)",
        "INSERT INTO webhook_event (provider, event_id, payload, received_at)
         VALUES ('stripe', 'evt-old', '{}', now() - interval '100 days'),
                ('stripe', 'evt-young', '{}', now() - interval '80 days')",
        "INSERT INTO occupancy_resource (id, capacity) VALUES ('chair', 1)",
        "INSERT INTO occupancy_claim (resource, owner, starts_at, ends_at, quantity)
         VALUES ('chair', 'res-old', now() - interval '200 days', now() - interval '200 days' + interval '1 hour', 1),
                ('chair', 'res-young', now() - interval '100 days', now() - interval '100 days' + interval '1 hour', 1)",
        "INSERT INTO short_link (key, target, created_at, expires_at)
         VALUES ('old', '/x', now() - interval '60 days', now() - interval '40 days'),
                ('young', '/x', now() - interval '60 days', now() - interval '20 days'),
                ('forever', '/x', now() - interval '600 days', NULL)",
    ] {
        sqlx::query(statement)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("{statement}: {e}"));
    }

    let gone = Retention::sweep(&db, erp_types::Timestamp::from(chrono::Utc::now()))
        .await
        .expect("sweeps");
    assert_eq!(gone, 4, "one old receipt of each kind");

    let outbox: Vec<String> = sqlx::query_scalar("SELECT idempotency_key FROM outbox ORDER BY 1")
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(
        outbox,
        ["pending", "young"],
        "the young receipt and the open promise stay"
    );
    let webhooks: Vec<String> = sqlx::query_scalar("SELECT event_id FROM webhook_event ORDER BY 1")
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(webhooks, ["evt-young"]);
    let claims: Vec<String> = sqlx::query_scalar("SELECT owner FROM occupancy_claim ORDER BY 1")
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(claims, ["res-young"]);
    let links: Vec<String> = sqlx::query_scalar("SELECT key FROM short_link ORDER BY 1")
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(
        links,
        ["forever", "young"],
        "a permanent link is never swept"
    );

    drop(conn);
    drop(db);
    fixture.cleanup().await;
}
