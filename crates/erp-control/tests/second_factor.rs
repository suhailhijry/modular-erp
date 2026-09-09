//! A second factor, against a real control database.
//!
//! `totp.rs`'s own tests prove the arithmetic against RFC 6238's published
//! vectors. These prove the parts that only exist once there is a database: an
//! enrolment that is not live until it is confirmed, a login that refuses to
//! issue a session without the factor, a code that cannot be used twice, and a
//! recovery code that is spent when it is used.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use erp_control::{
    AccessError, Actor, AuthError, ClusterRegistry, ControlPlane, Lane, PoolConfig, Scope,
    TenantPools, totp,
};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::{IdentityId, TenantId, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

const HANDLE: &str = "sara@bassat.test";
const PASSWORD: &str = "hunter2hunter2";

struct Fixture {
    control: ControlPlane,
    identity: IdentityId,
    sealing: erp_eventlog::SealingKey,
    tenant_databases: std::sync::Mutex<Vec<String>>,
    _db: TestDb,
}

impl Fixture {
    async fn new() -> Self {
        let db = Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");
        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");
        let control = ControlPlane::new(
            db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        );
        // Tenants are foreign-keyed to a cluster, so one has to exist before
        // any of the tenant tests can register.
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

        let identity = control
            .create_identity(Actor::system())
            .await
            .expect("identity is created")
            .id;
        control
            .register_login(identity, HANDLE.to_owned(), PASSWORD.to_owned())
            .await
            .expect("password is registered");

        Self {
            control,
            identity,
            sealing: erp_eventlog::SealingKey::parse(&format!("test:{}", "ab".repeat(32)))
                .expect("a sealing key"),
            tenant_databases: std::sync::Mutex::new(Vec::new()),
            _db: db,
        }
    }

    /// A registered, activated tenant with a real database behind it.
    async fn tenant(&self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        self.tenant_databases
            .lock()
            .expect("not poisoned")
            .push(tenant.database_name.clone());
        self.control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");
        tenant.id
    }

    /// Puts the fixture's own identity in a tenant as its owner.
    async fn join(&self, tenant: TenantId) {
        self.control
            .grant_membership(
                self.identity,
                Scope::Tenant(tenant),
                "owner",
                Actor::system(),
            )
            .await
            .expect("membership is granted");
    }

    /// Somebody else in the same tenant, with no second factor of their own.
    async fn colleague(&self, tenant: TenantId) -> IdentityId {
        let other = self
            .control
            .create_identity(Actor::system())
            .await
            .expect("identity is created")
            .id;
        self.control
            .grant_membership(other, Scope::Tenant(tenant), "viewer", Actor::system())
            .await
            .expect("membership is granted");
        other
    }

    async fn cleanup(&self) {
        let names = self.tenant_databases.lock().expect("not poisoned").clone();
        for name in names {
            let _ = erp_testkit::drop_named_database(&name).await;
        }
    }

    /// Enrols and confirms, returning the secret so a test can compute codes,
    /// and the recovery codes.
    async fn enrolled(&self, now: Timestamp) -> (Vec<u8>, Vec<String>) {
        let enrolment = self
            .control
            .begin_second_factor(self.identity, "Bassat", HANDLE, &self.sealing)
            .await
            .expect("enrolment begins");
        let secret = totp::unbase32(&enrolment.secret).expect("the secret is base32");
        let code = totp::code_at(&secret, seconds(now), totp::DIGITS).expect("a code");
        let confirmed = self
            .control
            .confirm_second_factor(self.identity, &code, now, &self.sealing)
            .await
            .expect("enrolment confirms");
        (secret, confirmed.recovery_codes)
    }
}

fn seconds(at: Timestamp) -> u64 {
    u64::try_from(at.timestamp()).expect("after 1970")
}

fn at(unix: i64) -> Timestamp {
    chrono::DateTime::from_timestamp(unix, 0).expect("a timestamp")
}

/// **The property the whole feature is for.**
#[tokio::test]
async fn a_password_alone_stops_working_once_a_factor_is_enrolled() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);

    // Before enrolling, the password is the whole login.
    fixture
        .control
        .log_in(HANDLE, PASSWORD)
        .await
        .expect("logs in with a password alone");

    let (secret, _) = fixture.enrolled(now).await;

    let refused = fixture.control.log_in(HANDLE, PASSWORD).await;
    assert!(
        matches!(refused, Err(AuthError::SecondFactorRequired)),
        "a password alone must not issue a session, got {refused:?}"
    );

    let code = totp::code_at(&secret, seconds(now), totp::DIGITS).unwrap();
    fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &code, now, &fixture.sealing)
        .await
        .expect("both factors together sign in");
}

/// A pending enrolment is not a second factor. Somebody who scanned the QR and
/// wandered off must not be locked out.
#[tokio::test]
async fn an_unconfirmed_enrolment_does_not_gate_a_login() {
    let fixture = Fixture::new().await;

    fixture
        .control
        .begin_second_factor(fixture.identity, "Bassat", HANDLE, &fixture.sealing)
        .await
        .expect("enrolment begins");

    assert!(
        !fixture
            .control
            .has_second_factor(fixture.identity)
            .await
            .unwrap(),
        "an enrolment nobody proved is not a factor"
    );
    fixture
        .control
        .log_in(HANDLE, PASSWORD)
        .await
        .expect("still logs in");
}

/// The wrong code must not confirm, or enrolment proves nothing.
#[tokio::test]
async fn a_wrong_code_does_not_confirm_an_enrolment() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);

    fixture
        .control
        .begin_second_factor(fixture.identity, "Bassat", HANDLE, &fixture.sealing)
        .await
        .expect("enrolment begins");

    let refused = fixture
        .control
        .confirm_second_factor(fixture.identity, "000000", now, &fixture.sealing)
        .await;
    assert!(matches!(refused, Err(AuthError::InvalidCredentials)));
    assert!(
        !fixture
            .control
            .has_second_factor(fixture.identity)
            .await
            .unwrap()
    );
}

/// **A code seen over a shoulder is good for thirty seconds.** Recording the
/// last accepted one is what closes that window.
#[tokio::test]
async fn a_code_cannot_be_used_twice() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (secret, _) = fixture.enrolled(now).await;

    let code = totp::code_at(&secret, seconds(now), totp::DIGITS).unwrap();
    fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &code, now, &fixture.sealing)
        .await
        .expect("the first use works");

    let replayed = fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &code, now, &fixture.sealing)
        .await;
    assert!(
        matches!(replayed, Err(AuthError::InvalidCredentials)),
        "the same code inside its own window must not work twice, got {replayed:?}"
    );

    // The next window's code is a different code and is accepted.
    let later = at(1_700_000_000 + 60);
    let next = totp::code_at(&secret, seconds(later), totp::DIGITS).unwrap();
    fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &next, later, &fixture.sealing)
        .await
        .expect("a new code works");
}

/// The phone is gone. The paper is the way back in, and each line works once.
#[tokio::test]
async fn a_recovery_code_signs_in_and_is_spent() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (_, recovery) = fixture.enrolled(now).await;

    assert_eq!(recovery.len(), erp_control::RECOVERY_CODES);
    assert_eq!(
        fixture
            .control
            .recovery_codes_left(fixture.identity)
            .await
            .unwrap(),
        i64::try_from(erp_control::RECOVERY_CODES).unwrap()
    );

    let code = recovery.first().expect("a code").clone();
    fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &code, now, &fixture.sealing)
        .await
        .expect("a recovery code signs in");

    assert_eq!(
        fixture
            .control
            .recovery_codes_left(fixture.identity)
            .await
            .unwrap(),
        i64::try_from(erp_control::RECOVERY_CODES - 1).unwrap(),
        "using one spends it"
    );

    let again = fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &code, now, &fixture.sealing)
        .await;
    assert!(
        matches!(again, Err(AuthError::InvalidCredentials)),
        "a spent recovery code is not a credential"
    );
}

/// Re-enrolling replaces the old phone and invalidates the old paper. Somebody
/// whose phone was stolen has to be able to make everything they had useless.
#[tokio::test]
async fn re_enrolling_retires_the_old_secret_and_the_old_recovery_codes() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (old_secret, old_recovery) = fixture.enrolled(now).await;

    let later = at(1_700_000_600);
    let (new_secret, new_recovery) = fixture.enrolled(later).await;
    assert_ne!(old_secret, new_secret);

    let stale = totp::code_at(&old_secret, seconds(later), totp::DIGITS).unwrap();
    let refused = fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &stale, later, &fixture.sealing)
        .await;
    assert!(
        matches!(refused, Err(AuthError::InvalidCredentials)),
        "the stolen phone must stop working"
    );

    let old_paper = old_recovery.first().expect("a code");
    let refused = fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, old_paper, later, &fixture.sealing)
        .await;
    assert!(
        matches!(refused, Err(AuthError::InvalidCredentials)),
        "the old paper must stop working too"
    );

    let fresh = totp::code_at(&new_secret, seconds(later), totp::DIGITS).unwrap();
    fixture
        .control
        .log_in_with_second_factor(HANDLE, PASSWORD, &fresh, later, &fixture.sealing)
        .await
        .expect("the new phone works");
    assert_eq!(new_recovery.len(), erp_control::RECOVERY_CODES);
}

#[tokio::test]
async fn turning_it_off_gives_the_password_back() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    fixture.enrolled(now).await;

    fixture
        .control
        .disable_second_factor(fixture.identity)
        .await
        .expect("disables");

    assert!(
        !fixture
            .control
            .has_second_factor(fixture.identity)
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .control
            .recovery_codes_left(fixture.identity)
            .await
            .unwrap(),
        0,
        "the paper goes with it"
    );
    fixture
        .control
        .log_in(HANDLE, PASSWORD)
        .await
        .expect("the password is the whole login again");
}

/// **The wrong password is refused before the code is even looked at.** A code
/// is not a password, and neither is a password on its own.
#[tokio::test]
async fn a_right_code_does_not_excuse_a_wrong_password() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (secret, _) = fixture.enrolled(now).await;
    let code = totp::code_at(&secret, seconds(now), totp::DIGITS).unwrap();

    let refused = fixture
        .control
        .log_in_with_second_factor(HANDLE, "not-the-password", &code, now, &fixture.sealing)
        .await;
    assert!(matches!(refused, Err(AuthError::InvalidCredentials)));
}

/// A secret is sealed against the identity that owns it, so a row copied onto
/// another account does not open.
#[tokio::test]
async fn a_secret_sealed_for_one_identity_does_not_open_for_another() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (secret, _) = fixture.enrolled(now).await;

    let other = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("a second identity")
        .id;
    sqlx::query("UPDATE authenticator SET identity_id = $1 WHERE kind = 'totp'")
        .bind(other.as_uuid())
        .execute(fixture.control.pool())
        .await
        .expect("moves the row");

    let code = totp::code_at(&secret, seconds(now), totp::DIGITS).unwrap();
    let refused = fixture
        .control
        .verify_second_factor(other, &code, now, &fixture.sealing)
        .await;
    assert!(
        refused.is_err(),
        "a transplanted secret must not open, got {refused:?}"
    );
}

// ---------------------------------------------------------------------------
// A tenant that requires one
// ---------------------------------------------------------------------------

/// **The whole feature, and the reason it refuses at entry rather than at
/// login.** A person may belong to two tenants; only one of them asked for
/// this, and the other must be unaffected.
#[tokio::test]
async fn a_tenant_that_requires_a_second_factor_refuses_a_member_without_one() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let strict = fixture.tenant("strict").await;
    let relaxed = fixture.tenant("relaxed").await;
    fixture.join(strict).await;
    fixture.join(relaxed).await;

    // An owner cannot switch it on from an unprotected account.
    let refused = fixture
        .control
        .set_second_factor_requirement(strict, fixture.identity, true)
        .await;
    assert!(
        matches!(refused, Err(AccessError::SecondFactorRequired)),
        "switching it on without one is a lockout waiting to happen, got {refused:?}"
    );

    fixture.enrolled(now).await;
    fixture
        .control
        .set_second_factor_requirement(strict, fixture.identity, true)
        .await
        .expect("an enrolled owner may switch it on");

    // The enrolled owner still gets in.
    fixture
        .control
        .enter(fixture.identity, strict, Lane::Interactive)
        .await
        .expect("an enrolled member enters");

    // A colleague who has not enrolled does not — here, and only here.
    let colleague = fixture.colleague(strict).await;
    let refused = fixture
        .control
        .enter(colleague, strict, Lane::Interactive)
        .await;
    assert!(
        matches!(refused, Err(AccessError::SecondFactorRequired)),
        "an unprotected member must not enter a tenant that requires one, got {refused:?}"
    );

    let elsewhere = fixture.colleague(relaxed).await;
    fixture
        .control
        .enter(elsewhere, relaxed, Lane::Interactive)
        .await
        .expect("a tenant that did not ask for this is unaffected");

    fixture.cleanup().await;
}

/// Turning it **off** must not need one, or the requirement cannot be undone by
/// the person it locked out.
#[tokio::test]
async fn the_requirement_can_always_be_switched_off() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let tenant = fixture.tenant("acme").await;
    fixture.join(tenant).await;

    fixture.enrolled(now).await;
    fixture
        .control
        .set_second_factor_requirement(tenant, fixture.identity, true)
        .await
        .expect("switches on");

    fixture
        .control
        .disable_second_factor(fixture.identity)
        .await
        .expect("the phone is gone");

    // Now unprotected, and locked out — but still able to undo it.
    assert!(matches!(
        fixture
            .control
            .enter(fixture.identity, tenant, Lane::Interactive)
            .await,
        Err(AccessError::SecondFactorRequired)
    ));
    fixture
        .control
        .set_second_factor_requirement(tenant, fixture.identity, false)
        .await
        .expect("switching it off needs no second factor");
    fixture
        .control
        .enter(fixture.identity, tenant, Lane::Interactive)
        .await
        .expect("and they are back in");

    fixture.cleanup().await;
}

/// The requirement is read from a cached tenant row. A cache nobody clears is a
/// setting nobody enforces.
#[tokio::test]
async fn turning_it_on_takes_effect_without_waiting_for_a_cache() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let tenant = fixture.tenant("acme").await;
    fixture.join(tenant).await;
    let colleague = fixture.colleague(tenant).await;

    // Warm the cache by entering first.
    fixture
        .control
        .enter(colleague, tenant, Lane::Interactive)
        .await
        .expect("enters while nothing is required");

    fixture.enrolled(now).await;
    fixture
        .control
        .set_second_factor_requirement(tenant, fixture.identity, true)
        .await
        .expect("switches on");

    let refused = fixture
        .control
        .enter(colleague, tenant, Lane::Interactive)
        .await;
    assert!(
        matches!(refused, Err(AccessError::SecondFactorRequired)),
        "the cached tenant must have been forgotten, got {refused:?}"
    );

    fixture.cleanup().await;
}
