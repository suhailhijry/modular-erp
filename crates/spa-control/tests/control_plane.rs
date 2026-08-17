//! Control-plane behaviour, against a real Postgres.
//!
//! The test that matters most is [`entering_one_tenant_cannot_reach_another`]:
//! it is the executable form of the claim that database-per-tenant makes
//! cross-tenant access structurally impossible.

// `clippy.toml`'s test allowances only reach `#[cfg(test)]` modules; an
// integration test is an ordinary crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{
    Actor, ClusterRegistry, ControlPlane, Lane, PoolConfig, Scope, TenantPools, TenantStatus,
};
use spa_testkit::{Schema, Template};
use spa_types::{IdentityId, ModuleId, TenantId};

/// The control plane's own schema, built from the shipped migrations — so these
/// tests exercise the migrations rather than a hand-written approximation.
static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);

/// A stand-in for a tenant database. Phase 2 replaces this with the real
/// event-log schema; for now it just needs to be distinguishable per tenant.
static TENANT: Schema = Schema::sql(
    "tenant-stub",
    &["CREATE TABLE marker (whose TEXT PRIMARY KEY)"],
);

struct Fixture {
    control: ControlPlane,
    _db: spa_testkit::TestDb,
    tenant_databases: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_config(PoolConfig::default()).await
    }

    async fn with_config(config: PoolConfig) -> Self {
        let db = Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("the test database URL parses");

        let control = ControlPlane::new(db.pool().clone(), TenantPools::new(clusters, config));
        // Tenants are now foreign-keyed to a cluster, so one has to exist.
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
            _db: db,
            tenant_databases: Vec::new(),
        }
    }

    /// Registers a tenant, creates its database, and activates it — the shape
    /// the provisioning workflow will have in Phase 4.
    async fn provision(&mut self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("tenant registers");

        spa_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        self.tenant_databases.push(tenant.database_name.clone());

        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");

        tenant.id
    }

    async fn member_of(&self, tenant: TenantId) -> IdentityId {
        let identity = self
            .control
            .create_identity(Actor::system())
            .await
            .expect("identity is created");
        self.control
            .grant_membership(identity.id, Scope::Tenant(tenant), "owner", Actor::system())
            .await
            .expect("membership is granted");
        identity.id
    }

    async fn cleanup(self) {
        for name in &self.tenant_databases {
            let _ = spa_testkit::drop_named_database(name).await;
        }
    }
}

/// The property everything else rests on.
#[tokio::test]
async fn entering_one_tenant_cannot_reach_another() {
    let mut fixture = Fixture::new().await;

    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;

    let acme_user = fixture.member_of(acme).await;
    let globex_user = fixture.member_of(globex).await;

    // Each writes into what it believes is its own database.
    let acme_db = fixture
        .control
        .enter(acme_user, acme, Lane::Interactive)
        .await
        .expect("enters");
    sqlx::query("INSERT INTO marker (whose) VALUES ('acme')")
        .execute(&mut *acme_db.acquire().await.expect("within budget"))
        .await
        .expect("writes");

    let globex_db = fixture
        .control
        .enter(globex_user, globex, Lane::Interactive)
        .await
        .expect("enters");
    sqlx::query("INSERT INTO marker (whose) VALUES ('globex')")
        .execute(&mut *globex_db.acquire().await.expect("within budget"))
        .await
        .expect("writes");

    // Neither sees the other. Not filtered out — absent.
    let in_acme: Vec<String> = sqlx::query_scalar("SELECT whose FROM marker")
        .fetch_all(&mut *acme_db.acquire().await.expect("within budget"))
        .await
        .expect("reads");
    let in_globex: Vec<String> = sqlx::query_scalar("SELECT whose FROM marker")
        .fetch_all(&mut *globex_db.acquire().await.expect("within budget"))
        .await
        .expect("reads");

    assert_eq!(in_acme, vec!["acme".to_owned()]);
    assert_eq!(in_globex, vec!["globex".to_owned()]);

    // And a handle knows which tenant it belongs to.
    assert_eq!(acme_db.tenant(), acme);
    assert_eq!(globex_db.tenant(), globex);

    drop(acme_db);
    drop(globex_db);
    fixture.cleanup().await;
}

/// A member of one tenant is not a member of another, and gets no handle.
#[tokio::test]
async fn membership_does_not_carry_across_tenants() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let acme_user = fixture.member_of(acme).await;

    fixture
        .control
        .enter(acme_user, acme, Lane::Interactive)
        .await
        .expect("their own tenant opens");

    let refused = fixture
        .control
        .enter(acme_user, globex, Lane::Interactive)
        .await;
    assert!(
        matches!(refused, Err(spa_control::AccessError::NotAMember)),
        "a member of one tenant must not enter another, got {refused:?}"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_revoked_membership_stops_working_immediately() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");

    fixture
        .control
        .revoke_membership(user, Scope::Tenant(tenant), Actor::system())
        .await
        .expect("revokes");

    assert!(matches!(
        fixture.control.enter(user, tenant, Lane::Interactive).await,
        Err(spa_control::AccessError::NotAMember)
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_suspended_identity_cannot_enter_anywhere() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");

    fixture
        .control
        .suspend_identity(user, "policy violation", Actor::system())
        .await
        .expect("suspends");

    assert!(matches!(
        fixture.control.enter(user, tenant, Lane::Interactive).await,
        Err(spa_control::AccessError::IdentitySuspended)
    ));

    fixture.cleanup().await;
}

/// A tenant is registered before its database exists. Entry must fail during
/// that window — otherwise a request lands on a database with no schema.
#[tokio::test]
async fn a_tenant_still_provisioning_cannot_be_entered() {
    let fixture = Fixture::new().await;

    let tenant = fixture
        .control
        .register_tenant_on("acme", "Acme", "primary", Actor::system())
        .await
        .expect("registers");
    assert_eq!(tenant.status, TenantStatus::Provisioning);

    let identity = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates");
    fixture
        .control
        .grant_membership(
            identity.id,
            Scope::Tenant(tenant.id),
            "owner",
            Actor::system(),
        )
        .await
        .expect("grants");

    let refused = fixture
        .control
        .enter(identity.id, tenant.id, Lane::Interactive)
        .await;
    assert!(
        matches!(
            refused,
            Err(spa_control::AccessError::TenantNotActive {
                status: TenantStatus::Provisioning
            })
        ),
        "got {refused:?}"
    );

    fixture.cleanup().await;
}

/// Platform staff get in through the audited path, not by a privilege flag.
#[tokio::test]
async fn support_access_requires_platform_membership_and_is_audited() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;

    let outsider = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates");
    assert!(
        matches!(
            fixture
                .control
                .enter_for_support(outsider.id, tenant, "curiosity")
                .await,
            Err(spa_control::AccessError::NotAMember)
        ),
        "support access must require a platform membership"
    );

    let staff = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates");
    fixture
        .control
        .grant_membership(staff.id, Scope::Platform, "support", Actor::system())
        .await
        .expect("grants");

    fixture
        .control
        .enter_for_support(staff.id, tenant, "ticket #42")
        .await
        .expect("staff may enter for support");

    // The audit trail must name who, what, and why — otherwise support access
    // is indistinguishable from the tenant acting for themselves.
    let (action, detail): (String, serde_json::Value) = sqlx::query_as(
        "SELECT action, detail FROM audit_entry
          WHERE subject_type = 'tenant' AND action = 'tenant.support_access'",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("an audit entry was written");

    assert_eq!(action, "tenant.support_access");
    assert_eq!(detail["reason"], serde_json::json!("ticket #42"));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_audit_trail_cannot_be_rewritten() {
    let fixture = Fixture::new().await;
    fixture
        .control
        .record(
            Actor::system(),
            "test.action",
            "thing",
            "1",
            serde_json::json!({}),
        )
        .await
        .expect("records");

    // Append-only is enforced by the database, not by discipline.
    assert!(
        sqlx::query("UPDATE audit_entry SET action = 'tampered'")
            .execute(fixture.control.pool())
            .await
            .is_err(),
        "audit entries must not be updatable"
    );
    assert!(
        sqlx::query("DELETE FROM audit_entry")
            .execute(fixture.control.pool())
            .await
            .is_err(),
        "audit entries must not be deletable"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn modules_toggle_and_the_handle_reports_them() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;
    let ledger = ModuleId::new("ledger").unwrap();
    let invoicing = ModuleId::new("invoicing").unwrap();

    fixture
        .control
        .enable_module(tenant, &ledger, Actor::system())
        .await
        .expect("enables");
    fixture
        .control
        .enable_module(tenant, &invoicing, Actor::system())
        .await
        .expect("enables");

    // Enabling twice is a no-op, because the caller is usually a retryable
    // workflow.
    fixture
        .control
        .enable_module(tenant, &ledger, Actor::system())
        .await
        .expect("enabling twice is idempotent");

    let db = fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");
    assert!(db.has_module(&ledger));
    assert!(db.has_module(&invoicing));
    assert_eq!(db.modules().len(), 2);
    drop(db);

    fixture
        .control
        .disable_module(tenant, &invoicing, Actor::system())
        .await
        .expect("disables");

    let db = fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");
    assert!(db.has_module(&ledger));
    assert!(
        !db.has_module(&invoicing),
        "a disabled module must not be reported as live"
    );
    drop(db);

    fixture.cleanup().await;
}

#[tokio::test]
async fn tenants_for_identity_lists_only_live_memberships() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;

    let user = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates");
    for tenant in [acme, globex] {
        fixture
            .control
            .grant_membership(user.id, Scope::Tenant(tenant), "member", Actor::system())
            .await
            .expect("grants");
    }

    let listed = fixture
        .control
        .tenants_for_identity(user.id)
        .await
        .expect("lists");
    assert_eq!(listed.len(), 2);

    fixture
        .control
        .revoke_membership(user.id, Scope::Tenant(globex), Actor::system())
        .await
        .expect("revokes");

    let listed = fixture
        .control
        .tenants_for_identity(user.id)
        .await
        .expect("lists");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].slug, "acme");

    fixture.cleanup().await;
}

/// Holding a handle must cost nothing.
///
/// This is the property that makes the design scale to client-facing load: with
/// permits scoped to the request, 10,000 req/s would need ~400 connections; with
/// permits scoped to the query, ~120. If this test ever fails, that arithmetic
/// has silently reverted.
#[tokio::test]
async fn holding_a_handle_costs_no_budget() {
    let mut fixture = Fixture::with_config(PoolConfig {
        interactive_operations: 2,
        ..PoolConfig::default()
    })
    .await;

    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    let mut handles = Vec::new();
    for _ in 0..20 {
        handles.push(
            fixture
                .control
                .enter(user, tenant, Lane::Interactive)
                .await
                .expect("entering is free"),
        );
    }
    assert_eq!(
        fixture.control.tenants().available(Lane::Interactive),
        2,
        "twenty open handles must not have spent any of a budget of two"
    );

    fixture.cleanup().await;
}

/// Budget is spent per operation, and released when the operation ends.
#[tokio::test]
async fn a_query_spends_budget_only_while_it_runs() {
    let mut fixture = Fixture::with_config(PoolConfig {
        interactive_operations: 1,
        ..PoolConfig::default()
    })
    .await;

    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;
    let db = fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");

    let conn = db.acquire().await.expect("within budget");
    assert_eq!(fixture.control.tenants().available(Lane::Interactive), 0);

    let refused = db.acquire().await;
    assert!(
        matches!(refused, Err(spa_control::PoolError::Overloaded { .. })),
        "over budget must fail fast rather than queue, got {refused:?}"
    );

    drop(conn);
    assert_eq!(
        fixture.control.tenants().available(Lane::Interactive),
        1,
        "finishing an operation must return its permit"
    );
    db.acquire().await.expect("capacity was returned");

    fixture.cleanup().await;
}

/// Bulkheads: a tenant's customers flooding the booking endpoint must not stop
/// the employee at the counter from working.
#[tokio::test]
async fn client_traffic_cannot_starve_the_counter() {
    let mut fixture = Fixture::with_config(PoolConfig {
        client_operations: 2,
        interactive_operations: 2,
        ..PoolConfig::default()
    })
    .await;

    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    let client_db = fixture
        .control
        .enter(user, tenant, Lane::Client)
        .await
        .expect("opens");
    let counter_db = fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");

    // Saturate the client lane.
    let mut flood = Vec::new();
    for _ in 0..2 {
        flood.push(client_db.acquire().await.expect("within budget"));
    }
    assert!(
        client_db.acquire().await.is_err(),
        "the client lane should now be exhausted"
    );

    // The counter is unaffected.
    let _serving = counter_db
        .acquire()
        .await
        .expect("client saturation must not starve interactive work");
    assert_eq!(fixture.control.tenants().available(Lane::Interactive), 1);

    fixture.cleanup().await;
}

/// Transactions hold their permit until they finish, not until the handle drops.
#[tokio::test]
async fn a_transaction_holds_its_permit_until_it_commits() {
    let mut fixture = Fixture::with_config(PoolConfig {
        interactive_operations: 1,
        ..PoolConfig::default()
    })
    .await;

    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;
    let db = fixture
        .control
        .enter(user, tenant, Lane::Interactive)
        .await
        .expect("opens");

    let mut tx = db.begin().await.expect("within budget");
    assert_eq!(fixture.control.tenants().available(Lane::Interactive), 0);

    sqlx::query("INSERT INTO marker (whose) VALUES ('in-transaction')")
        .execute(&mut *tx)
        .await
        .expect("writes");
    tx.commit().await.expect("commits");

    assert_eq!(
        fixture.control.tenants().available(Lane::Interactive),
        1,
        "committing must return the permit"
    );

    // And the write landed.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM marker")
        .fetch_one(&mut *db.acquire().await.expect("within budget"))
        .await
        .expect("reads");
    assert_eq!(count, 1);

    fixture.cleanup().await;
}

/// Without a replica, reads go to the primary — so `read()` is always callable
/// and adding replicas later is configuration, not a code change.
#[tokio::test]
async fn reads_fall_back_to_the_primary_when_no_replica_is_configured() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;
    let db = fixture
        .control
        .enter(user, tenant, Lane::Client)
        .await
        .expect("opens");

    assert!(!db.has_replica());
    let mut conn = db.read().await.expect("read path works without a replica");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM marker")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(count, 0);

    fixture.cleanup().await;
}

/// The entry path must not query the control database on every request — at
/// 10,000 req/s that would be 40,000 queries/second against a single database.
#[tokio::test]
async fn entering_repeatedly_does_not_hammer_the_control_database() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    // Warm the cache, then count what a further 200 entries cost.
    fixture
        .control
        .enter(user, tenant, Lane::Client)
        .await
        .expect("opens");

    let (_, misses_before) = fixture.control.entry_cache_stats();
    for _ in 0..200 {
        fixture
            .control
            .enter(user, tenant, Lane::Client)
            .await
            .expect("opens");
    }
    let (_, misses_after) = fixture.control.entry_cache_stats();

    assert_eq!(
        misses_after - misses_before,
        0,
        "200 warm entries caused {} control-database round trips; the entry path \
         must be served from cache or the control plane becomes the bottleneck",
        misses_after - misses_before
    );

    fixture.cleanup().await;
}

/// A cold entry costs exactly four lookups — identity, tenant, membership,
/// entitlements. If that number grows, the arithmetic in `cache`'s docs is
/// wrong and the TTL needs revisiting.
#[tokio::test]
async fn a_cold_entry_costs_four_lookups() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    fixture.control.clear_caches();
    let (_, before) = fixture.control.entry_cache_stats();
    fixture
        .control
        .enter(user, tenant, Lane::Client)
        .await
        .expect("opens");
    let (_, after) = fixture.control.entry_cache_stats();

    assert_eq!(after - before, 4, "cold entry should cost four lookups");

    fixture.cleanup().await;
}

/// Revoking access takes effect immediately on the node that performed it,
/// rather than waiting out the cache TTL.
#[tokio::test]
async fn revocation_is_not_delayed_by_the_cache() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    fixture
        .control
        .enter(user, tenant, Lane::Client)
        .await
        .expect("opens");

    fixture
        .control
        .revoke_membership(user, Scope::Tenant(tenant), Actor::system())
        .await
        .expect("revokes");

    assert!(
        matches!(
            fixture.control.enter(user, tenant, Lane::Client).await,
            Err(spa_control::AccessError::NotAMember)
        ),
        "a revocation on this node must not be masked by its own cache"
    );

    fixture.cleanup().await;
}

/// Two tenants must never be pointed at one database. The schema refuses.
#[tokio::test]
async fn two_tenants_cannot_share_a_database() {
    let fixture = Fixture::new().await;
    let first = fixture
        .control
        .register_tenant_on("acme", "Acme", "primary", Actor::system())
        .await
        .expect("registers");

    let clash = sqlx::query(
        "INSERT INTO tenant (id, slug, display_name, cluster, database_name)
         VALUES ($1, 'globex', 'Globex', 'primary', $2)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(&first.database_name)
    .execute(fixture.control.pool())
    .await;

    assert!(
        clash.is_err(),
        "the database must refuse two tenants sharing one database"
    );

    fixture.cleanup().await;
}

/// **A person can be erased, and the trail of what they did survives.**
///
/// The right this implements is Saudi Arabia's PDPL right to destruction. What
/// it must not do is destroy the audit trail with them: the entries stay,
/// saying what was done and when, attributed to nobody — which is the shape an
/// entry has always had for a system-initiated action.
#[tokio::test]
async fn erasing_a_person_keeps_what_the_platform_did() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    // Something they did, so the trail has their name on it.
    fixture
        .control
        .suspend_identity(user, "policy violation", Actor::identity(user))
        .await
        .expect("suspends");

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_entry")
        .fetch_one(fixture.control.pool())
        .await
        .expect("reads");
    let theirs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_entry WHERE actor_identity_id = $1")
            .bind(user.as_uuid())
            .fetch_one(fixture.control.pool())
            .await
            .expect("reads");
    assert!(
        theirs > 0,
        "the trail should name them before they are erased"
    );

    fixture
        .control
        .erase_identity(user, Actor::system())
        .await
        .expect("erases");

    // **They are gone.**
    assert!(
        fixture
            .control
            .identity(user)
            .await
            .expect("reads")
            .is_none()
    );
    assert!(matches!(
        fixture.control.enter(user, tenant, Lane::Interactive).await,
        Err(spa_control::AccessError::NoSuchIdentity)
    ));

    // **And the trail is not.** One entry more than before — the erasure
    // itself — and none of them still names them.
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_entry")
        .fetch_one(fixture.control.pool())
        .await
        .expect("reads");
    assert_eq!(after, before + 1, "an audit entry was destroyed");

    let still_named: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_entry WHERE actor_identity_id = $1")
            .bind(user.as_uuid())
            .fetch_one(fixture.control.pool())
            .await
            .expect("reads");
    assert_eq!(still_named, 0, "the person is still named in the trail");

    // The erasure is visible as having happened.
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_entry WHERE action = 'identity.erased'")
            .fetch_one(fixture.control.pool())
            .await
            .expect("reads");
    assert_eq!(recorded, 1);

    fixture.cleanup().await;
}

/// The trigger permits **only** the update that erasure needs.
///
/// Widening it to "any update" would make the audit trail a table anybody can
/// rewrite, which is the thing it exists not to be.
#[tokio::test]
async fn the_audit_trail_is_still_append_only_for_everything_else() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;
    fixture
        .control
        .suspend_identity(user, "policy violation", Actor::identity(user))
        .await
        .expect("suspends");

    let pool = fixture.control.pool();

    // Rewriting what was done.
    assert!(
        sqlx::query(
            "UPDATE audit_entry SET action = 'something.else' WHERE action = 'identity.suspended'"
        )
        .execute(pool)
        .await
        .is_err(),
        "an audit entry's action was rewritten"
    );

    // Blaming somebody else — nulling is allowed, reassigning is not.
    let other = fixture.member_of(tenant).await;
    assert!(
        sqlx::query("UPDATE audit_entry SET actor_identity_id = $1 WHERE actor_identity_id = $2")
            .bind(other.as_uuid())
            .bind(user.as_uuid())
            .execute(pool)
            .await
            .is_err(),
        "one person's actions were attributed to another"
    );

    // Deleting one outright.
    assert!(
        sqlx::query("DELETE FROM audit_entry WHERE action = 'identity.suspended'")
            .execute(pool)
            .await
            .is_err(),
        "an audit entry was deleted"
    );

    fixture.cleanup().await;
}
