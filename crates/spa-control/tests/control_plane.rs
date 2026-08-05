//! Control-plane behaviour, against a real Postgres.
//!
//! The test that matters most is [`entering_one_tenant_cannot_reach_another`]:
//! it is the executable form of the claim that database-per-tenant makes
//! cross-tenant access structurally impossible.

// `clippy.toml`'s test allowances only reach `#[cfg(test)]` modules; an
// integration test is an ordinary crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_control::{
    Actor, ClusterRegistry, ControlPlane, PoolConfig, Scope, TenantPools, TenantStatus,
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

        Self {
            control: ControlPlane::new(db.pool().clone(), TenantPools::new(clusters, config)),
            _db: db,
            tenant_databases: Vec::new(),
        }
    }

    /// Registers a tenant, creates its database, and activates it — the shape
    /// the provisioning workflow will have in Phase 4.
    async fn provision(&mut self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant(slug, slug, "primary", Actor::system())
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
        .enter(acme_user, acme)
        .await
        .expect("enters");
    sqlx::query("INSERT INTO marker (whose) VALUES ('acme')")
        .execute(acme_db.pool())
        .await
        .expect("writes");

    let globex_db = fixture
        .control
        .enter(globex_user, globex)
        .await
        .expect("enters");
    sqlx::query("INSERT INTO marker (whose) VALUES ('globex')")
        .execute(globex_db.pool())
        .await
        .expect("writes");

    // Neither sees the other. Not filtered out — absent.
    let in_acme: Vec<String> = sqlx::query_scalar("SELECT whose FROM marker")
        .fetch_all(acme_db.pool())
        .await
        .expect("reads");
    let in_globex: Vec<String> = sqlx::query_scalar("SELECT whose FROM marker")
        .fetch_all(globex_db.pool())
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
        .enter(acme_user, acme)
        .await
        .expect("their own tenant opens");

    let refused = fixture.control.enter(acme_user, globex).await;
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

    fixture.control.enter(user, tenant).await.expect("opens");

    fixture
        .control
        .revoke_membership(user, Scope::Tenant(tenant), Actor::system())
        .await
        .expect("revokes");

    assert!(matches!(
        fixture.control.enter(user, tenant).await,
        Err(spa_control::AccessError::NotAMember)
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_suspended_identity_cannot_enter_anywhere() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    fixture.control.enter(user, tenant).await.expect("opens");

    fixture
        .control
        .suspend_identity(user, "policy violation", Actor::system())
        .await
        .expect("suspends");

    assert!(matches!(
        fixture.control.enter(user, tenant).await,
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
        .register_tenant("acme", "Acme", "primary", Actor::system())
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

    let refused = fixture.control.enter(identity.id, tenant.id).await;
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

    let db = fixture.control.enter(user, tenant).await.expect("opens");
    assert!(db.has_module(&ledger));
    assert!(db.has_module(&invoicing));
    assert_eq!(db.modules().len(), 2);
    drop(db);

    fixture
        .control
        .disable_module(tenant, &invoicing, Actor::system())
        .await
        .expect("disables");

    let db = fixture.control.enter(user, tenant).await.expect("opens");
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

/// The connection budget is what makes database-per-tenant survivable, so its
/// behaviour under exhaustion is asserted rather than assumed.
#[tokio::test]
async fn exhausting_the_connection_budget_refuses_rather_than_queues() {
    let mut fixture = Fixture::with_config(PoolConfig {
        max_concurrent_operations: 1,
        ..PoolConfig::default()
    })
    .await;

    let tenant = fixture.provision("acme").await;
    let user = fixture.member_of(tenant).await;

    let held = fixture.control.enter(user, tenant).await.expect("opens");
    assert_eq!(fixture.control.tenants().available_budget(), 0);

    let refused = fixture.control.enter(user, tenant).await;
    assert!(
        matches!(
            refused,
            Err(spa_control::AccessError::Pool(
                spa_control::PoolError::Overloaded
            ))
        ),
        "over budget must fail fast, got {refused:?}"
    );

    // Dropping the handle returns the permit — the budget tracks concurrent
    // work, not tenant count.
    drop(held);
    assert_eq!(fixture.control.tenants().available_budget(), 1);
    let reacquired = fixture
        .control
        .enter(user, tenant)
        .await
        .expect("capacity was returned");
    assert_eq!(fixture.control.tenants().available_budget(), 0);

    drop(reacquired);
    fixture.cleanup().await;
}

/// Two tenants must never be pointed at one database. The schema refuses.
#[tokio::test]
async fn two_tenants_cannot_share_a_database() {
    let fixture = Fixture::new().await;
    let first = fixture
        .control
        .register_tenant("acme", "Acme", "primary", Actor::system())
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
