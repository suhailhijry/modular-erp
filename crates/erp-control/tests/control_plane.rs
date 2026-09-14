//! Control-plane behaviour, against a real Postgres.
//!
//! The test that matters most is [`entering_one_tenant_cannot_reach_another`]:
//! it is the executable form of the claim that database-per-tenant makes
//! cross-tenant access structurally impossible.

// `clippy.toml`'s test allowances only reach `#[cfg(test)]` modules; an
// integration test is an ordinary crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_control::{
    AccessError, Actor, ClusterRegistry, ControlPlane, Lane, PoolConfig, Scope, TenantPools,
    TenantStatus,
};
use erp_testkit::{Schema, Template};
use erp_types::{IdentityId, ModuleId, TenantId};

/// The control plane's own schema, built from the shipped migrations — so these
/// tests exercise the migrations rather than a hand-written approximation.
static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);

/// A stand-in for a tenant database. Phase 2 replaces this with the real
/// event-log schema; for now it just needs to be distinguishable per tenant.
static TENANT: Schema = Schema::sql(
    "tenant-stub",
    &["CREATE TABLE marker (whose TEXT PRIMARY KEY)"],
);

struct Fixture {
    control: ControlPlane,
    db: erp_testkit::TestDb,
    tenant_databases: Vec<String>,
    prover: Arc<FakeProver>,
}

/// TXT records the tests choose to publish, standing in for the world's DNS.
#[derive(Debug, Default)]
struct FakeProver {
    records: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl FakeProver {
    fn publish(&self, name: &str, value: &str) {
        self.records
            .lock()
            .expect("not poisoned")
            .entry(name.to_owned())
            .or_default()
            .push(value.to_owned());
    }
}

impl erp_control::DomainProver for FakeProver {
    fn txt_records<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<String>, erp_control::ProofError>>
                + Send
                + 'a,
        >,
    > {
        let found = self
            .records
            .lock()
            .expect("not poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(found) })
    }
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
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");

        let prover = Arc::new(FakeProver::default());
        let control = ControlPlane::new(db.pool().clone(), TenantPools::new(clusters, config))
            .with_prover(Arc::clone(&prover) as Arc<dyn erp_control::DomainProver>);
        // Tenants are now foreign-keyed to a cluster, so one has to exist.
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

        Self {
            control,
            prover,
            db,
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

        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
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
            let _ = erp_testkit::drop_named_database(name).await;
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
        matches!(refused, Err(erp_control::AccessError::NotAMember)),
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
        Err(erp_control::AccessError::NotAMember)
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
        Err(erp_control::AccessError::IdentitySuspended)
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
            Err(erp_control::AccessError::TenantNotActive {
                status: TenantStatus::Provisioning
            })
        ),
        "got {refused:?}"
    );

    fixture.cleanup().await;
}

/// Enrols a second factor the way a person does, so a door that asks for one
/// finds it.
async fn enrol(control: &ControlPlane, identity: IdentityId) {
    let sealing = erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes");
    let enrolment = control
        .begin_second_factor(identity, "ERP", "staff", &sealing, None)
        .await
        .expect("enrolment begins");
    let secret = erp_control::totp::unbase32(&enrolment.secret).expect("base32");
    let now = chrono::Utc::now();
    let seconds = u64::try_from(now.timestamp()).expect("after 1970");
    let code =
        erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS).expect("a code");
    control
        .confirm_second_factor(identity, &code, None, now, &sealing, None, None)
        .await
        .expect("enrolment confirms");
}

/// Platform staff get in through the audited path, not by a privilege flag —
/// and only staff whose role may, holding a second factor.
///
/// **Billing is refused.** Billing suspends tenants; it never reads their
/// books, and before the platform had roles any live platform membership was
/// enough.
#[tokio::test]
async fn support_access_needs_the_power_and_a_second_factor_and_is_audited() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let staff_member = |role: &'static str| {
        let control = &fixture.control;
        async move {
            let who = control
                .create_identity(Actor::system())
                .await
                .expect("creates");
            control
                .grant_membership(who.id, Scope::Platform, role, Actor::system())
                .await
                .expect("grants");
            who.id
        }
    };

    let outsider = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates");
    let billing = staff_member("billing").await;
    enrol(&fixture.control, billing).await;
    for (who, what) in [(outsider.id, "an outsider"), (billing, "billing")] {
        assert!(
            matches!(
                fixture
                    .control
                    .enter_for_support(who, tenant, "curiosity")
                    .await,
                Err(AccessError::StaffOnly(
                    erp_control::PlatformPower::EnterForSupport
                ))
            ),
            "{what} entered a tenant's books for support"
        );
    }

    let staff = staff_member("support").await;
    assert!(
        matches!(
            fixture
                .control
                .enter_for_support(staff, tenant, "ticket #42")
                .await,
            Err(AccessError::StaffSecondFactorRequired)
        ),
        "support without a second factor entered a tenant's books"
    );

    enrol(&fixture.control, staff).await;
    fixture
        .control
        .enter_for_support(staff, tenant, "ticket #42")
        .await
        .expect("support with a second factor may enter");

    // A stored platform role this build does not know is corrupt data, and
    // refused — never read as "no role", and never as a role that may.
    let odd = staff_member("root").await;
    enrol(&fixture.control, odd).await;
    assert!(
        matches!(
            fixture.control.enter_for_support(odd, tenant, "?").await,
            Err(AccessError::Corrupt(_))
        ),
        "an unknown platform role was not refused as corrupt"
    );

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

/// **A suspension closes every door on this node at once, and reinstating
/// opens them again at once.** No `clear_caches`: the first `enter` puts the
/// tenant in the entry cache, and a suspension that did not forget it would be
/// answered from there for five more seconds.
#[tokio::test]
async fn a_suspended_tenant_is_refused_at_every_door_and_reinstated_at_once() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.member_of(tenant).await;
    let support = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates")
        .id;
    fixture
        .control
        .grant_membership(support, Scope::Platform, "support", Actor::system())
        .await
        .expect("grants");
    enrol(&fixture.control, support).await;

    let control = &fixture.control;
    let enter = || async {
        control
            .enter(owner, tenant, Lane::Interactive)
            .await
            .map(drop)
    };
    let public = || async { control.enter_for_the_public(tenant).await.map(drop) };
    enter().await.expect("opens, and caches the tenant");
    public().await.expect("the booking page opens");

    control
        .suspend_tenant(tenant, "unpaid", Actor::identity(support))
        .await
        .expect("suspends");

    // **Both halves shut the doors.** `suspending` is the drain — the worker
    // still signs and reports the tenant's documents — and nobody gets in
    // during it any more than after.
    for (door, answer) in [("a member", enter().await), ("the public", public().await)] {
        assert!(
            matches!(
                answer,
                Err(AccessError::TenantNotActive {
                    status: TenantStatus::Suspending
                })
            ),
            "{door} got into a tenant being suspended: {answer:?}"
        );
    }
    assert!(
        control.finish_suspension(tenant).await.expect("finishes"),
        "the drain finished a tenant that was suspending"
    );
    for (door, answer) in [("a member", enter().await), ("the public", public().await)] {
        assert!(
            matches!(
                answer,
                Err(AccessError::TenantNotActive {
                    status: TenantStatus::Suspended
                })
            ),
            "{door} got into a suspended tenant: {answer:?}"
        );
    }
    // Finding out why is a normal reason for support to need to get in.
    control
        .enter_for_support(support, tenant, "why was acme suspended")
        .await
        .expect("support still opens a suspended tenant");

    control
        .reinstate_tenant(tenant, Actor::identity(support))
        .await
        .expect("reinstates");
    enter().await.expect("a reinstated tenant opens at once");
    public().await.expect("and so does its booking page");

    fixture.cleanup().await;
}

/// **A tenant moves only from the status the move starts from**, and a move
/// asked of any other is refused naming the status it is in — never answered
/// `Ok` as a no-op, which is what `activate_tenant` used to do, auditing a
/// `tenant.activated` that had activated nothing.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "every status move, from every status, in the one place their order can be read"
)]
async fn a_tenant_moves_only_from_the_status_it_is_in() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let control = &fixture.control;
    let wrong = |answer: Result<(), AccessError>, status, expected| {
        assert!(
            matches!(
                answer,
                Err(AccessError::WrongTenantStatus { status: s, expected: e })
                    if s == status && e == expected
            ),
            "expected {status:?} refused for needing {expected:?}, got {answer:?}"
        );
    };
    let status = |id| async move {
        control
            .tenant(id)
            .await
            .expect("reads")
            .expect("exists")
            .status
    };

    // Still provisioning: there is nothing to suspend yet.
    let half_built = control
        .register_tenant_on("halfbuilt", "Half Built", "primary", Actor::system())
        .await
        .expect("registers");
    wrong(
        control
            .suspend_tenant(half_built.id, "unpaid", Actor::system())
            .await,
        TenantStatus::Provisioning,
        TenantStatus::Active,
    );
    assert_eq!(status(half_built.id).await, TenantStatus::Provisioning);
    wrong(
        control
            .reinstate_tenant(half_built.id, Actor::system())
            .await,
        TenantStatus::Provisioning,
        TenantStatus::Suspended,
    );

    wrong(
        control.reinstate_tenant(tenant, Actor::system()).await,
        TenantStatus::Active,
        TenantStatus::Suspended,
    );
    control
        .suspend_tenant(tenant, "unpaid", Actor::system())
        .await
        .expect("suspends");
    wrong(
        control
            .suspend_tenant(tenant, "again", Actor::system())
            .await,
        TenantStatus::Suspending,
        TenantStatus::Active,
    );
    // Activating is not reinstating.
    wrong(
        control.activate_tenant(tenant, Actor::system()).await,
        TenantStatus::Suspending,
        TenantStatus::Provisioning,
    );
    assert_eq!(status(tenant).await, TenantStatus::Suspending);

    // **The drain's move, and only from `suspending`.** Not an error from any
    // other status: the worker that asks has simply nothing to finish.
    assert!(control.finish_suspension(tenant).await.expect("finishes"));
    assert_eq!(status(tenant).await, TenantStatus::Suspended);
    assert!(
        !control.finish_suspension(tenant).await.expect("answers"),
        "a finished suspension was finished again"
    );
    wrong(
        control
            .suspend_tenant(tenant, "again", Actor::system())
            .await,
        TenantStatus::Suspended,
        TenantStatus::Active,
    );

    // Reinstating works from either half: back to active, suspend again, and
    // a tenant still draining is reinstated as readily.
    control
        .reinstate_tenant(tenant, Actor::system())
        .await
        .expect("reinstates from suspended");
    control
        .suspend_tenant(tenant, "unpaid again", Actor::system())
        .await
        .expect("suspends");
    assert_eq!(status(tenant).await, TenantStatus::Suspending);
    control
        .reinstate_tenant(tenant, Actor::system())
        .await
        .expect("reinstates from suspending");
    assert_eq!(status(tenant).await, TenantStatus::Active);
    assert!(
        !control.finish_suspension(tenant).await.expect("answers"),
        "an active tenant was moved to suspended"
    );

    let nobody = TenantId::new();
    for answer in [
        control
            .suspend_tenant(nobody, "unpaid", Actor::system())
            .await,
        control.reinstate_tenant(nobody, Actor::system()).await,
        control.activate_tenant(nobody, Actor::system()).await,
    ] {
        assert!(
            matches!(answer, Err(AccessError::NoSuchTenant)),
            "{answer:?}"
        );
    }

    // Only the moves that happened are on the record.
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_entry
          WHERE subject_type = 'tenant' AND subject_id = $1 AND action <> 'tenant.registered'
          ORDER BY at, id",
    )
    .bind(tenant.to_string())
    .fetch_all(control.pool())
    .await
    .expect("reads");
    assert_eq!(
        actions,
        [
            "tenant.activated",
            "tenant.suspended",
            "tenant.suspension_complete",
            "tenant.reinstated",
            "tenant.suspended",
            "tenant.reinstated",
        ]
    );

    fixture.cleanup().await;
}

/// **A suspension says why, under whose name, and the schema refuses one that
/// does not.** The raw `UPDATE` is the point here, not a shortcut: it is the
/// hand edit an operator might make, and the database is what refuses it.
#[tokio::test]
async fn a_suspension_says_why_and_is_audited_and_the_schema_refuses_one_that_does_not() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let staff = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("creates")
        .id;
    let control = &fixture.control;
    let row = || async {
        sqlx::query_as::<_, (String, Option<String>, bool)>(
            "SELECT status, suspended_reason, suspended_at IS NOT NULL FROM tenant WHERE id = $1",
        )
        .bind(tenant.as_uuid())
        .fetch_one(control.pool())
        .await
        .expect("reads")
    };
    let entry = |action: &'static str| async move {
        sqlx::query_as::<_, (Option<uuid::Uuid>, serde_json::Value)>(
            "SELECT actor_identity_id, detail FROM audit_entry
              WHERE action = $1 AND subject_id = $2",
        )
        .bind(action)
        .bind(tenant.to_string())
        .fetch_one(control.pool())
        .await
        .expect("an audit entry was written")
    };

    // Blank, or longer than the owner should have to read: refused, by name.
    for reason in ["   ", &"x".repeat(501)] {
        assert!(
            matches!(
                control
                    .suspend_tenant(tenant, reason, Actor::identity(staff))
                    .await,
                Err(AccessError::SuspensionReason)
            ),
            "a {}-character reason was not refused",
            reason.len()
        );
    }
    assert_eq!(row().await, ("active".to_owned(), None, false));

    control
        .suspend_tenant(tenant, " unpaid since August ", Actor::identity(staff))
        .await
        .expect("suspends");
    assert_eq!(
        row().await,
        (
            "suspending".to_owned(),
            Some("unpaid since August".to_owned()),
            true
        )
    );
    assert_eq!(
        entry("tenant.suspended").await,
        (
            Some(staff.into_uuid()),
            serde_json::json!({ "reason": "unpaid since August" })
        )
    );
    // The reason and the instant survive the drain's move.
    control.finish_suspension(tenant).await.expect("finishes");
    assert_eq!(
        row().await,
        (
            "suspended".to_owned(),
            Some("unpaid since August".to_owned()),
            true
        )
    );

    control
        .reinstate_tenant(tenant, Actor::identity(staff))
        .await
        .expect("reinstates");
    assert_eq!(row().await, ("active".to_owned(), None, false));
    assert_eq!(entry("tenant.reinstated").await.0, Some(staff.into_uuid()));

    // By hand, with no reason: the database refuses what the method would not.
    let hand_edit = sqlx::query("UPDATE tenant SET status = 'suspended' WHERE id = $1")
        .bind(tenant.as_uuid())
        .execute(control.pool())
        .await
        .expect_err("a suspension with no reason was stored");
    assert_eq!(
        hand_edit
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("tenant_suspension_is_complete")
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_audit_trail_cannot_be_rewritten() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.control.pool().acquire().await.expect("connection");
    fixture
        .control
        .record(
            &mut conn,
            Actor::system(),
            None,
            "test.action",
            "thing",
            "1",
            serde_json::json!({}),
        )
        .await
        .expect("records");
    drop(conn);

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
        matches!(refused, Err(erp_control::PoolError::Overloaded { .. })),
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
            Err(erp_control::AccessError::NotAMember)
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
        Err(erp_control::AccessError::NoSuchIdentity)
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

    // Taking an entry out of its tenant's trail, or moving one into another's.
    // `tenant_id` came after the whitelist `0007` wrote, and `0019` pins it.
    assert!(
        sqlx::query("UPDATE audit_entry SET tenant_id = NULL WHERE action = 'tenant.registered'")
            .execute(pool)
            .await
            .is_err(),
        "an entry was taken out of its tenant's trail"
    );
    assert!(
        sqlx::query("UPDATE audit_entry SET tenant_id = $1 WHERE action = 'identity.suspended'")
            .bind(tenant.as_uuid())
            .execute(pool)
            .await
            .is_err(),
        "an entry was moved into a tenant's trail"
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

/// **An expired session is forgotten.** The row is an index entry that grows
/// with every sign-in, and the first version swept it never; expiry was only
/// ever checked on the way in.
#[tokio::test]
async fn expired_sessions_are_swept_and_live_ones_are_not() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let identity = fixture.member_of(tenant).await;

    let (live, _) = fixture
        .control
        .start_session(identity)
        .await
        .expect("a session starts");
    let (expired, _) = fixture
        .control
        .start_session(identity)
        .await
        .expect("another starts");
    sqlx::query("UPDATE session SET expires_at = now() - interval '1 hour' WHERE token_hash = $1")
        .bind(erp_control::SessionToken::digest(expired.expose()))
        .execute(fixture.db.pool())
        .await
        .expect("winds one back");

    assert_eq!(
        fixture.control.sweep_sessions().await.expect("sweeps"),
        1,
        "the expired one"
    );
    assert!(
        fixture.control.session(live.expose()).await.is_ok(),
        "the live session still signs in"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM session")
        .fetch_one(fixture.db.pool())
        .await
        .expect("counts");
    assert_eq!(rows, 1);

    fixture.cleanup().await;
}

/// **A domain is proved by DNS, not by asking.** `verify_domain` reads the
/// record the claim named; the wrong value or none is a refusal that says what
/// to publish, and the right one is a proof that then licenses every host under
/// the domain.
#[tokio::test]
async fn a_domain_is_proved_by_the_record_it_was_told_to_publish() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;

    let unclaimed = fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await;
    assert!(
        matches!(unclaimed, Err(AccessError::DomainNotClaimed(_))),
        "{unclaimed:?}"
    );

    let token = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");

    let unpublished = fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await;
    match unpublished {
        Err(AccessError::DomainNotProved {
            domain,
            record,
            expected,
        }) => {
            assert_eq!(domain, "salon.example");
            assert_eq!(record, "_erp-challenge.salon.example");
            assert_eq!(expected, format!("erp-verification={token}"));
        }
        other => panic!("an unpublished record proved a domain: {other:?}"),
    }
    assert!(
        fixture
            .control
            .tenant_by_host("api.salon.example")
            .await
            .expect("asks")
            .is_none(),
        "an unproved domain reaches nobody"
    );

    fixture.prover.publish(
        "_erp-challenge.salon.example",
        "erp-verification=not-this-one",
    );
    assert!(matches!(
        fixture
            .control
            .verify_domain(tenant, "salon.example", Actor::system())
            .await,
        Err(AccessError::DomainNotProved { .. })
    ));

    fixture.prover.publish(
        "_erp-challenge.salon.example",
        &format!("erp-verification={token}"),
    );
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("the published record proves it");

    for host in [
        "salon.example",
        "api.salon.example",
        "Book.Salon.Example:443",
    ] {
        let found = fixture
            .control
            .tenant_by_host(host)
            .await
            .expect("asks")
            .map(|t| t.id);
        assert_eq!(found, Some(tenant), "{host}");
    }
    for host in ["salon.example.attacker.test", "notsalon.example", "example"] {
        assert!(
            fixture
                .control
                .tenant_by_host(host)
                .await
                .expect("asks")
                .is_none(),
            "{host} reached a tenant"
        );
    }

    fixture.cleanup().await;
}
