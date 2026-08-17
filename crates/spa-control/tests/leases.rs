//! Which worker gets which tenant, and when a tenant is next worth a look.
//!
//! These are the deterministic counterpart to `spa-worker`'s shutdown tests: a
//! worker test can only observe that no event was double-applied, which two
//! different mechanisms could be responsible for. These pin down the lease on
//! its own.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use spa_control::{
    Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools, TenantStatus, WorkSchedule,
};
use spa_testkit::{Schema, TestDb};
use spa_types::TenantId;

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);

/// No jitter: a test that has to guess how long a revisit takes is a flaky test.
fn schedule() -> WorkSchedule {
    WorkSchedule {
        max_idle_interval: WorkSchedule::default().max_idle_interval,
        lease: Duration::from_secs(30),
        idle_interval: Duration::from_secs(30),
        jitter: Duration::ZERO,
    }
}

struct Fixture {
    control: ControlPlane,
    db: TestDb,
}

impl Fixture {
    async fn new() -> Self {
        let db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("the test database URL parses");

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

    /// Registers and activates a tenant. No database is created — nothing here
    /// opens one.
    async fn tenant(&self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("tenant registers");
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");
        tenant.id
    }

    async fn lease_owner(&self, tenant: TenantId) -> Option<String> {
        sqlx::query_scalar("SELECT worker_lease_owner FROM tenant WHERE id = $1")
            .bind(tenant.as_uuid())
            .fetch_one(self.db.pool())
            .await
            .expect("reads")
    }

    /// Winds a tenant's lease back so it reads as lapsed.
    async fn expire_lease(&self, tenant: TenantId) {
        sqlx::query(
            "UPDATE tenant SET worker_lease_until = now() - INTERVAL '1 second' WHERE id = $1",
        )
        .bind(tenant.as_uuid())
        .execute(self.db.pool())
        .await
        .expect("expires");
    }
}

#[tokio::test]
async fn a_claimed_tenant_is_not_claimable_by_another_worker() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    let first = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].tenant.id, tenant);
    assert_eq!(
        fixture.lease_owner(tenant).await.as_deref(),
        Some("worker-a")
    );

    let second = fixture
        .control
        .claim_tenants("worker-b", 10, schedule())
        .await
        .expect("claims");
    assert!(
        second.is_empty(),
        "a leased tenant must not be handed to a second worker"
    );
}

#[tokio::test]
async fn re_claiming_your_own_tenant_renews_it() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");

    // Renewing and claiming are the same call, so long work needs no second
    // code path to keep hold of its tenant.
    let again = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].tenant.id, tenant);
}

#[tokio::test]
async fn a_lapsed_lease_returns_the_tenant_to_the_fleet() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .claim_tenants("crashed-worker", 10, schedule())
        .await
        .expect("claims");

    // Exactly what a worker that died mid-visit leaves behind.
    fixture.expire_lease(tenant).await;

    let recovered = fixture
        .control
        .claim_tenants("replacement", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(
        recovered.len(),
        1,
        "expiry, not release, is what makes a crashed worker recoverable"
    );
    assert_eq!(
        fixture.lease_owner(tenant).await.as_deref(),
        Some("replacement")
    );
}

#[tokio::test]
async fn a_tenant_that_had_nothing_to_do_is_not_visited_again_at_once() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    fixture
        .control
        .schedule_next_visit(tenant, Duration::from_secs(30), false)
        .await
        .expect("defers");

    let immediate = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    assert!(
        immediate.is_empty(),
        "an idle tenant revisited immediately is a connection spent to learn nothing"
    );
    assert_eq!(
        fixture.lease_owner(tenant).await,
        None,
        "and the lease is dropped, not held across the wait"
    );
}

#[tokio::test]
async fn a_tenant_that_did_work_is_due_again_immediately() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    fixture
        .control
        .schedule_next_visit(tenant, Duration::ZERO, true)
        .await
        .expect("reschedules");

    let again = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(again.len(), 1, "there was more to do, so look again now");
}

/// The seam the push path attaches to.
#[tokio::test]
async fn requesting_a_visit_makes_a_deferred_tenant_due() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .schedule_next_visit(tenant, Duration::from_hours(1), false)
        .await
        .expect("defers");
    assert!(
        fixture
            .control
            .claim_tenants("worker-a", 10, schedule())
            .await
            .expect("claims")
            .is_empty()
    );

    fixture
        .control
        .request_visit(tenant)
        .await
        .expect("requests");

    let claimed = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(
        claimed.len(),
        1,
        "a write should not have to wait out an idle interval to be projected"
    );
}

#[tokio::test]
async fn releasing_leases_hands_tenants_over_without_making_them_due() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("acme").await;

    fixture
        .control
        .claim_tenants("leaving", 10, schedule())
        .await
        .expect("claims");
    fixture
        .control
        .schedule_next_visit(tenant, Duration::from_secs(30), false)
        .await
        .expect("defers");
    // Claim again so there is a lease to release.
    fixture
        .control
        .request_visit(tenant)
        .await
        .expect("requests");
    fixture
        .control
        .claim_tenants("leaving", 10, schedule())
        .await
        .expect("claims");

    let released = fixture
        .control
        .release_leases("leaving")
        .await
        .expect("releases");
    assert_eq!(released, 1);
    assert_eq!(fixture.lease_owner(tenant).await, None);

    // Still due, so a replacement starts on it at once rather than waiting out
    // the expiry.
    let claimed = fixture
        .control
        .claim_tenants("replacement", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(claimed.len(), 1);
}

#[tokio::test]
async fn only_active_tenants_are_visited() {
    let fixture = Fixture::new().await;

    // Registered but never activated: its database may not exist yet.
    let provisioning = fixture
        .control
        .register_tenant_on("pending", "Pending", "primary", Actor::system())
        .await
        .expect("registers");
    let active = fixture.tenant("acme").await;

    let claimed = fixture
        .control
        .claim_tenants("worker-a", 10, schedule())
        .await
        .expect("claims");

    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].tenant.id, active);
    assert_ne!(
        claimed[0].tenant.id, provisioning.id,
        "a tenant whose database is not built yet must not be opened"
    );
}

#[tokio::test]
async fn a_claim_is_bounded_by_its_limit() {
    let fixture = Fixture::new().await;
    for slug in ["a1", "b2", "c3", "d4", "e5"] {
        fixture.tenant(slug).await;
    }

    let claimed = fixture
        .control
        .claim_tenants("worker-a", 2, schedule())
        .await
        .expect("claims");
    assert_eq!(
        claimed.len(),
        2,
        "the limit is what keeps a worker's connection demand bounded"
    );

    // The rest are still there for another worker, or for the next round.
    let rest = fixture
        .control
        .claim_tenants("worker-b", 10, schedule())
        .await
        .expect("claims");
    assert_eq!(rest.len(), 3);
}

#[tokio::test]
async fn two_workers_claiming_at_once_get_disjoint_sets() {
    let fixture = Fixture::new().await;
    for slug in ["a1", "b2", "c3", "d4", "e5", "f6", "g7", "h8"] {
        fixture.tenant(slug).await;
    }

    let (left, right) = tokio::join!(
        fixture.control.claim_tenants("worker-a", 4, schedule()),
        fixture.control.claim_tenants("worker-b", 4, schedule()),
    );
    let left = left.expect("claims");
    let right = right.expect("claims");

    let mut all: Vec<TenantId> = left
        .iter()
        .chain(right.iter())
        .map(|c| c.tenant.id)
        .collect();
    let total = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(
        all.len(),
        total,
        "SKIP LOCKED must give two simultaneous claimers disjoint sets"
    );
}

#[tokio::test]
async fn maintenance_entry_refuses_a_tenant_with_no_database_yet() {
    let fixture = Fixture::new().await;
    let provisioning = fixture
        .control
        .register_tenant_on("pending", "Pending", "primary", Actor::system())
        .await
        .expect("registers");

    let result = fixture.control.enter_for_maintenance(provisioning.id).await;

    assert!(
        matches!(
            result,
            Err(spa_control::AccessError::TenantNotActive {
                status: TenantStatus::Provisioning
            })
        ),
        "opening a tenant before its schema exists would fail confusingly later"
    );
}

// ---------------------------------------------------------------------------
// Dormancy
// ---------------------------------------------------------------------------

/// **A tenant that has nothing to do stops being asked so often.**
///
/// `next_visit_at` throttled a quiet tenant to a fixed thirty seconds and never
/// backed off, so five thousand tenants cost about 167 visits a second for ever
/// whether any of them was doing anything or not. Each of those opens a
/// connection, runs every enabled module's projection query, and writes a row
/// back — the largest standing cost this platform has, spent entirely on
/// tenants doing nothing.
#[tokio::test]
async fn consecutive_idle_visits_push_the_next_one_further_out() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("quiet").await;
    let schedule = WorkSchedule {
        idle_interval: std::time::Duration::from_secs(30),
        jitter: std::time::Duration::ZERO,
        ..WorkSchedule::default()
    };

    // Four visits that find nothing, each rescheduling from the streak the
    // previous one left behind.
    let mut streaks = Vec::new();
    for _ in 0..4 {
        let claimed = fixture
            .control
            .claim_tenants("w", 10, schedule)
            .await
            .expect("claims");
        let claim = claimed
            .iter()
            .find(|c| c.tenant.id == tenant)
            .expect("the tenant is due");
        streaks.push(claim.idle_visits);

        let delay = schedule.next_idle_delay(tenant, claim.idle_visits);
        fixture
            .control
            .schedule_next_visit(tenant, std::time::Duration::ZERO, false)
            .await
            .expect("reschedules");
        assert!(delay >= schedule.idle_interval);
    }

    assert_eq!(
        streaks,
        vec![0, 1, 2, 3],
        "the idle streak did not accumulate, so the backoff can never happen"
    );

    // **A visit that worked resets it.** Without this a tenant that goes busy
    // after a quiet week would still be looked at every six hours.
    fixture
        .control
        .schedule_next_visit(tenant, std::time::Duration::ZERO, true)
        .await
        .expect("reschedules");
    let claimed = fixture
        .control
        .claim_tenants("w", 10, schedule)
        .await
        .expect("claims");
    assert_eq!(
        claimed
            .iter()
            .find(|c| c.tenant.id == tenant)
            .expect("due")
            .idle_visits,
        0,
        "a tenant that did work is still treated as dormant"
    );
}

/// **Waking is what makes the backoff invisible.**
///
/// A dormant tenant is due in hours. A write calls `request_visit`, which pulls
/// it back to now — so somebody who returns after a month waits a claim cycle,
/// not six hours. Without this the backoff would be a latency bug rather than a
/// saving.
#[tokio::test]
async fn a_request_wakes_a_dormant_tenant_immediately() {
    let fixture = Fixture::new().await;
    let tenant = fixture.tenant("dormant").await;

    // As six hours of silence would leave it.
    fixture
        .control
        .schedule_next_visit(tenant, std::time::Duration::from_hours(6), false)
        .await
        .expect("reschedules");

    let before = fixture
        .control
        .claim_tenants("w", 10, WorkSchedule::default())
        .await
        .expect("claims");
    assert!(
        !before.iter().any(|c| c.tenant.id == tenant),
        "a dormant tenant must not be claimed on its own"
    );

    fixture
        .control
        .request_visit(tenant)
        .await
        .expect("asks for a visit");

    let after = fixture
        .control
        .claim_tenants("w", 10, WorkSchedule::default())
        .await
        .expect("claims");
    assert!(
        after.iter().any(|c| c.tenant.id == tenant),
        "a dormant tenant that received a request was not woken"
    );
}
