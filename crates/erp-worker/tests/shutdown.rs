//! Stopping a worker without losing work.
//!
//! The claim under test is not "the worker exits". It is that **a projection
//! interrupted by SIGTERM ends up in exactly the state it would have reached had
//! nobody interrupted it** — and the only way to check that without trusting the
//! thing under test is to rebuild the projection from the log and diff.
//!
//! [`sigterm_mid_batch_loses_nothing_and_duplicates_nothing`] does that. The
//! projection it uses increments a running total with no dedup guard, so a
//! double-applied event shows up as a wrong number rather than as a silent
//! overwrite.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools, WorkSchedule};
use erp_eventlog::{
    DomainEvent, Envelope, Metadata, NewEvent, Upcasters, append, integrity, read_since,
};
use erp_projection::{
    Projection, ProjectionCtx, ProjectionError, ProjectionGroup, checkpoint, ensure_group_schema,
    replay_shadow,
};
use erp_testkit::{Schema, TestDb};
use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId, TenantId,
};
use erp_worker::{Activity, Job, ProjectionJob, Worker, WorkerConfig};
use sqlx::PgConnection;
use tokio_util::sync::CancellationToken;

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

// ---------------------------------------------------------------------------
// A group whose totals reveal a double-apply
// ---------------------------------------------------------------------------

struct Ledger;
impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

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

fn upcasters() -> Arc<Upcasters> {
    Arc::new(Upcasters::new().declare(&posted_name(), SchemaVersion::new(1).unwrap()))
}

/// Sums per account, with no idempotency guard.
///
/// The missing guard is the point: L4 promises exactly-once, so if a shutdown
/// ever replayed an event the total would be wrong and this test would say so. A
/// projection written defensively would hide the bug it exists to catch.
struct Balances {
    /// How slow one event is to apply. Long enough that a batch is reliably
    /// in flight when the cancellation arrives.
    latency: Duration,
    applied: Arc<AtomicUsize>,
}

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

        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }

        sqlx::query(
            "INSERT INTO balance (account, total, events) VALUES ($1, $2, 1)
             ON CONFLICT (account) DO UPDATE
                SET total = balance.total + EXCLUDED.total,
                    events = balance.events + 1",
        )
        .bind(&event.account)
        .bind(event.amount)
        .execute(conn)
        .await?;

        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture: a control plane with one real tenant database
// ---------------------------------------------------------------------------

struct Fixture {
    control: Arc<ControlPlane>,
    tenant: TenantId,
    control_db: TestDb,
    tenant_database: String,
}

impl Fixture {
    async fn new() -> Self {
        let control_db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");

        let control = ControlPlane::new(
            control_db.pool().clone(),
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

        let tenant = control
            .register_tenant_on("acme", "Acme", "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");

        let fixture = Self {
            control: Arc::new(control),
            tenant: tenant.id,
            control_db,
            tenant_database: tenant.database_name,
        };

        // The group's schema and tables, as `enable_module` will do in Phase 4.
        let db = fixture
            .control
            .enter_for_maintenance(fixture.tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        ensure_group_schema::<Ledger>(&mut conn)
            .await
            .expect("group schema");
        sqlx::raw_sql(
            "CREATE TABLE proj_ledger.balance (
                 account TEXT PRIMARY KEY,
                 total   BIGINT NOT NULL,
                 events  BIGINT NOT NULL
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("group tables");
        drop(conn);
        drop(db);

        fixture
    }

    /// Appends `count` events to the tenant's log.
    async fn post(&self, count: i64) {
        let db = self
            .control
            .enter_for_maintenance(self.tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");

        for i in 0..count {
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
                    serde_json::json!({ "account": "cash", "amount": 10 }),
                )],
                &Metadata::default(),
            )
            .await
            .expect("appends");
        }
    }

    async fn balance(&self) -> Option<(i64, i64)> {
        let db = self
            .control
            .enter_for_maintenance(self.tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        sqlx::query_as("SELECT total, events FROM proj_ledger.balance WHERE account = 'cash'")
            .fetch_optional(&mut *conn)
            .await
            .expect("reads")
    }

    async fn checkpoint(&self) -> LogPosition {
        let db = self
            .control
            .enter_for_maintenance(self.tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        checkpoint::<Ledger>(&mut conn).await.expect("reads")
    }

    fn worker(&self, name: &str, latency: Duration, applied: &Arc<AtomicUsize>) -> Worker {
        let projection = Arc::new(Balances {
            latency,
            applied: Arc::clone(applied),
        });
        let job = ProjectionJob::<Ledger>::new(vec![projection], upcasters(), 4);

        Worker::new(
            Arc::clone(&self.control),
            WorkerConfig {
                name: name.to_owned(),
                schedule: WorkSchedule {
                    max_idle_interval: WorkSchedule::default().max_idle_interval,
                    lease: Duration::from_secs(30),
                    // No jitter: a test that has to guess how long to wait for a
                    // revisit is a flaky test.
                    idle_interval: Duration::from_millis(20),
                    jitter: Duration::ZERO,
                },
                tenants_per_claim: 8,
                concurrency: 4,
                max_ticks_per_visit: 64,
                empty_claim_pause: Duration::from_millis(5),
                drain_timeout: Duration::from_secs(10),
            },
        )
        .with_job(Arc::new(job))
    }

    async fn cleanup(self) {
        let _ = erp_testkit::drop_named_database(&self.tenant_database).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_drives_a_tenants_projections_to_the_head_of_the_log() {
    let fixture = Fixture::new().await;
    fixture.post(12).await;

    let applied = Arc::new(AtomicUsize::new(0));
    let worker = fixture.worker("solo", Duration::ZERO, &applied);
    let cancel = CancellationToken::new();

    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    // Wait for the projection to catch up rather than for a duration.
    wait_until(Duration::from_secs(10), || async {
        fixture.checkpoint().await.get() == 12
    })
    .await;

    cancel.cancel();
    let shutdown = run.await.expect("joins");

    assert!(shutdown.drained, "the drain must complete");
    assert_eq!(shutdown.failed_visits, 0);
    assert_eq!(fixture.balance().await, Some((120, 12)));

    fixture.cleanup().await;
}

/// **The shutdown-safety test.**
///
/// A worker is cancelled while a batch is in flight. Afterwards, the projection
/// must be exactly what a clean run would have produced — which is checked by
/// rebuilding it from the log into a shadow schema and diffing, not by trusting
/// the numbers the interrupted run left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_mid_batch_loses_nothing_and_duplicates_nothing() {
    const EVENTS: i64 = 40;

    let fixture = Fixture::new().await;
    fixture.post(EVENTS).await;

    let applied = Arc::new(AtomicUsize::new(0));
    // Batches of four at ~15ms an event: a batch takes ~60ms, so cancelling once
    // a few events have landed reliably lands inside one.
    let worker = fixture.worker("interrupted", Duration::from_millis(15), &applied);
    let cancel = CancellationToken::new();

    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    // Cancel once work is genuinely under way but nowhere near finished.
    wait_until(Duration::from_secs(10), || async {
        applied.load(Ordering::SeqCst) >= 6
    })
    .await;
    let applied_at_cancel = applied.load(Ordering::SeqCst);
    assert!(
        applied_at_cancel < usize::try_from(EVENTS).unwrap(),
        "the test must interrupt work in progress, not work already finished"
    );

    cancel.cancel();
    let shutdown = run.await.expect("joins");
    assert!(shutdown.drained, "the drain must complete");

    // 1. The checkpoint and the tables agree. This is L4: whatever committed,
    //    committed together.
    let stopped_at = fixture.checkpoint().await;
    let (total, events) = fixture.balance().await.expect("some rows landed");
    assert_eq!(
        events,
        stopped_at.get(),
        "the projection applied exactly the events the checkpoint claims"
    );
    assert_eq!(total, events * 10);

    // 2. It stopped part-way. Without this the test could pass by finishing
    //    everything before the cancellation and proving nothing about shutdown.
    assert!(
        stopped_at.get() < EVENTS,
        "the worker was supposed to stop mid-log, but reached {stopped_at}"
    );
    assert!(
        stopped_at.get() > 0,
        "and it was supposed to have committed something first"
    );

    // 3. It stopped on a batch boundary, not inside one. A partially applied
    //    batch would leave a checkpoint that is not a multiple of the batch size.
    assert_eq!(
        stopped_at.get() % 4,
        0,
        "an interrupted batch must roll back whole; {stopped_at} is mid-batch"
    );

    // 4. Restarting finishes the job, and the result is identical to what an
    //    uninterrupted run would have produced.
    let applied_after = Arc::new(AtomicUsize::new(0));
    let restarted = fixture.worker("replacement", Duration::ZERO, &applied_after);
    let cancel = CancellationToken::new();
    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { restarted.run(cancel).await })
    };
    wait_until(Duration::from_secs(10), || async {
        fixture.checkpoint().await.get() == EVENTS
    })
    .await;
    cancel.cancel();
    run.await.expect("joins");

    assert_eq!(
        fixture.balance().await,
        Some((EVENTS * 10, EVENTS)),
        "every event applied exactly once across the interruption"
    );

    // 5. The proof that does not rely on any of the above: rebuild from the log
    //    and diff. If the interruption had lost or duplicated anything, the live
    //    tables would differ from a clean replay.
    let db = fixture
        .control
        .enter_for_maintenance(fixture.tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    assert!(
        integrity(&mut conn).await.expect("checks").is_contiguous(),
        "the log itself must still be contiguous"
    );
    let pending = read_since(&mut conn, LogPosition::new(EVENTS).unwrap(), 10)
        .await
        .expect("reads");
    assert!(pending.is_empty(), "nothing was left unprojected");
    drop(conn);

    let shadow_projection = Arc::new(Balances {
        latency: Duration::ZERO,
        applied: Arc::new(AtomicUsize::new(0)),
    });
    let shadow_ref: &dyn Projection<Group = Ledger> = shadow_projection.as_ref();
    let pool = tenant_pool(&fixture).await;
    let report = replay_shadow::<Ledger>(&pool, &[shadow_ref], &upcasters(), 100)
        .await
        .expect("replays");
    pool.close().await;
    assert!(
        report.is_reproducible(),
        "the interrupted run must be indistinguishable from a clean one; \
         differences: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// A worker releases its leases on the way out, so a replacement starts at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopping_worker_hands_its_tenants_over_immediately() {
    let fixture = Fixture::new().await;
    fixture.post(4).await;

    let applied = Arc::new(AtomicUsize::new(0));
    let worker = fixture.worker("leaving", Duration::from_millis(5), &applied);
    let cancel = CancellationToken::new();

    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };
    wait_until(Duration::from_secs(10), || async {
        applied.load(Ordering::SeqCst) >= 1
    })
    .await;
    cancel.cancel();
    let shutdown = run.await.expect("joins");

    assert!(shutdown.visits > 0);
    let held: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant WHERE worker_lease_owner IS NOT NULL")
            .fetch_one(fixture.control_db.pool())
            .await
            .expect("counts");
    assert_eq!(
        held, 0,
        "a lease left behind means the replacement waits out the expiry"
    );

    fixture.cleanup().await;
}

/// Two workers against one tenant do not both process it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_do_not_double_apply_a_tenants_events() {
    const EVENTS: i64 = 24;

    let fixture = Fixture::new().await;
    fixture.post(EVENTS).await;

    let left_applied = Arc::new(AtomicUsize::new(0));
    let right_applied = Arc::new(AtomicUsize::new(0));
    let left = fixture.worker("left", Duration::from_millis(2), &left_applied);
    let right = fixture.worker("right", Duration::from_millis(2), &right_applied);

    let cancel = CancellationToken::new();
    let runs = {
        let (a, b) = (cancel.clone(), cancel.clone());
        (
            tokio::spawn(async move { left.run(a).await }),
            tokio::spawn(async move { right.run(b).await }),
        )
    };

    wait_until(Duration::from_secs(15), || async {
        fixture.checkpoint().await.get() == EVENTS
    })
    .await;
    cancel.cancel();
    runs.0.await.expect("joins");
    runs.1.await.expect("joins");

    assert_eq!(
        fixture.balance().await,
        Some((EVENTS * 10, EVENTS)),
        "the tenant lease and the checkpoint lock between them must admit \
         exactly one application per event"
    );
    // Counted across both, because which one does the work is not the point and
    // asserting a split would be asserting a race. The lease usually gives the
    // whole tenant to whoever claims it first, which is the design working.
    // That the *lease itself* excludes is proved deterministically in
    // `erp-control/tests/leases.rs`.
    assert_eq!(
        left_applied.load(Ordering::SeqCst) + right_applied.load(Ordering::SeqCst),
        usize::try_from(EVENTS).unwrap(),
        "no event was applied twice by different workers"
    );

    fixture.cleanup().await;
}

/// A job that fails stops its tenant and leaves the worker running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_job_stalls_one_tenant_without_taking_the_worker_down() {
    struct AlwaysFails;

    #[async_trait::async_trait]
    impl Job for AlwaysFails {
        fn name(&self) -> &'static str {
            "always-fails"
        }
        async fn tick(
            &self,
            _db: &erp_control::TenantDb,
        ) -> Result<Activity, erp_worker::BoxError> {
            Err("upstream is on fire".into())
        }
    }

    let fixture = Fixture::new().await;
    fixture.post(2).await;

    let worker = Worker::new(
        Arc::clone(&fixture.control),
        WorkerConfig {
            name: "stubborn".to_owned(),
            schedule: WorkSchedule {
                max_idle_interval: WorkSchedule::default().max_idle_interval,
                lease: Duration::from_secs(30),
                idle_interval: Duration::from_millis(10),
                jitter: Duration::ZERO,
            },
            empty_claim_pause: Duration::from_millis(5),
            ..WorkerConfig::default()
        },
    )
    .with_job(Arc::new(AlwaysFails));

    let cancel = CancellationToken::new();
    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(cancel).await })
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let shutdown = run.await.expect("joins");

    assert!(
        shutdown.failed_visits > 0,
        "the failure must be counted, not swallowed"
    );
    assert!(
        shutdown.drained,
        "and the worker must still shut down cleanly"
    );
    assert_eq!(
        fixture.checkpoint().await,
        LogPosition::ZERO,
        "a failing job commits nothing"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------

/// Polls until `condition` holds, or fails the test.
///
/// Waiting for a condition rather than a duration: a fixed sleep long enough to
/// be reliable is also long enough to make the suite slow, and one short enough
/// to be fast is flaky on a loaded machine.
async fn wait_until<F, Fut>(limit: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if condition().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition did not hold within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A pool straight at the tenant's database, for the shadow replay.
///
/// `replay_shadow` creates and drops schemas, which is neither a projection nor
/// a command — it is an operator action, and it is the one place a test reaches
/// past `TenantDb` deliberately.
async fn tenant_pool(fixture: &Fixture) -> sqlx::PgPool {
    let url = erp_testkit::database_url();
    let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
    sqlx::PgPool::connect(&format!("{base}/{}", fixture.tenant_database))
        .await
        .expect("connects to the tenant database")
}
