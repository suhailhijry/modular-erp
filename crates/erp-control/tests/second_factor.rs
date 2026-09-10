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
        self.enrolling(now, None).await
    }

    /// The same, **replacing** a factor that is already there — which costs a
    /// code from the old one, or one of its recovery codes. A stolen phone is
    /// what the paper is for.
    async fn re_enrolled(&self, now: Timestamp, previous: &str) -> (Vec<u8>, Vec<String>) {
        self.enrolling(now, Some(previous)).await
    }

    async fn enrolling(&self, now: Timestamp, previous: Option<&str>) -> (Vec<u8>, Vec<String>) {
        let enrolment = self
            .control
            .begin_second_factor(self.identity, "Bassat", HANDLE, &self.sealing)
            .await
            .expect("enrolment begins");
        let secret = totp::unbase32(&enrolment.secret).expect("the secret is base32");
        let code = totp::code_at(&secret, seconds(now), totp::DIGITS).expect("a code");
        let confirmed = self
            .control
            .confirm_second_factor(self.identity, &code, previous, now, &self.sealing)
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
        .confirm_second_factor(fixture.identity, "000000", None, now, &fixture.sealing)
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
///
/// **And it now costs one of those recovery codes**, because replacing a factor
/// destroys it and all ten — so it asks for proof of what it destroys. The
/// stolen phone is exactly why the paper exists, which is why requiring it
/// strands nobody.
#[tokio::test]
async fn re_enrolling_retires_the_old_secret_and_the_old_recovery_codes() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (old_secret, old_recovery) = fixture.enrolled(now).await;

    let later = at(1_700_000_600);
    let paper = old_recovery.first().expect("a code").clone();
    let (new_secret, new_recovery) = fixture.re_enrolled(later, &paper).await;
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

    let old_paper = old_recovery.get(1).expect("a second code");
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
    let (_secret, recovery) = fixture.enrolled(now).await;

    // Turning it off now costs a code — see
    // `turning_it_off_needs_the_factor_being_turned_off`.
    fixture
        .control
        .disable_second_factor(fixture.identity, Some(&recovery[0]), now, &fixture.sealing)
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

/// Turning it **off** must not need the *tenant requirement* switched off
/// first, or the requirement cannot be undone by the person it locked out.
///
/// It does now cost a code, since a stolen session used to be enough — and the
/// phone being gone is what the recovery codes are for. That adds no stranding:
/// somebody with neither could not have reached this route, because they could
/// not have logged in.
#[tokio::test]
async fn the_requirement_can_always_be_switched_off() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let tenant = fixture.tenant("acme").await;
    fixture.join(tenant).await;

    let (_secret, recovery) = fixture.enrolled(now).await;
    fixture
        .control
        .set_second_factor_requirement(tenant, fixture.identity, true)
        .await
        .expect("switches on");

    fixture
        .control
        .disable_second_factor(fixture.identity, Some(&recovery[0]), now, &fixture.sealing)
        .await
        .expect("the phone is gone, and the paper is what that is for");

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

// ---------------------------------------------------------------------------
// No session without both factors
// ---------------------------------------------------------------------------

/// **The takeover this closed, run end to end.**
///
/// An attacker holding the password and the mailbox — the exact pair a second
/// factor exists to survive — used to be able to walk around it: signing up a
/// throwaway company under the victim's address calls `authenticate`, which is
/// the credential half with no factor in it, and confirming the link called
/// `start_session` directly. That handed back a full session as the victim,
/// and `DELETE /v1/sessions/second-factor` needs nothing but a session, so the
/// enrolment and all ten recovery codes went with it.
///
/// **And it is refused before anything is built.** Reaching `start_session`
/// with the tenant already provisioned would answer `500` and leave an orphan
/// database behind, with the confirmation link unclaimed and the slug taken
/// forever — a worse bug than the one being fixed.
#[tokio::test]
async fn signing_up_a_second_company_cannot_walk_around_a_second_factor() {
    let fixture = Fixture::new().await;
    fixture.enrolled(at(1_700_000_000)).await;

    let refused = fixture
        .control
        .sign_up(
            HANDLE.to_owned(),
            PASSWORD.to_owned(),
            "throwaway".to_owned(),
            "Throwaway".to_owned(),
            Vec::new(),
        )
        .await
        .expect_err("a password alone must not walk past a second factor");

    assert!(
        matches!(
            refused,
            erp_control::AccessError::Auth(erp_control::AuthError::SecondFactorRequired)
        ),
        "{refused:?}"
    );

    // Nothing was built on the way to being refused.
    assert!(
        fixture
            .control
            .tenant_by_slug("throwaway")
            .await
            .expect("the lookup works")
            .is_none(),
        "a refused signup left a tenant behind"
    );

    fixture.cleanup().await;
}

/// **A phone code is one factor.**
///
/// `verify_code` minted a session for whatever identity the number belonged to,
/// having asked for nothing else. An identity that signs in by phone *and* has
/// enrolled a second factor was therefore one SMS away from a session that
/// skipped it.
#[tokio::test]
async fn a_phone_code_cannot_walk_around_a_second_factor() {
    let fixture = Fixture::new().await;
    fixture.enrolled(at(1_700_000_000)).await;

    let refused = fixture
        .control
        .start_session(fixture.identity)
        .await
        .expect_err("an enrolled identity needs its second factor");

    assert!(
        matches!(refused, erp_control::AuthError::SecondFactorRequired),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **The law, checked in the source rather than promised in a comment.**
///
/// `log_in` used to carry the promise — *"a path which never heard of a second
/// factor cannot issue a session that skipped one"* — while three other callers
/// of `start_session` minted sessions without ever asking: the phone code, and
/// both signup paths for an address that already has an account. An enrolled
/// identity was takeable by anybody holding the password and the mailbox, which
/// is the exact pair a second factor exists to survive.
///
/// The gate is inside `start_session` now, and `issue_session` is the only way
/// round it. This counts the ways round: **two, both in `auth.rs`, both having
/// just checked.** A third is either a mistake or a decision somebody has to
/// come here and write down.
#[test]
fn only_two_paths_may_issue_a_session_without_checking_the_second_factor() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut callers: Vec<String> = Vec::new();

    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("src is readable") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable");
            for (n, line) in text.lines().enumerate() {
                // The definition is not a call.
                if line.contains("async fn issue_session") {
                    continue;
                }
                if line.contains("issue_session(") {
                    let file = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    callers.push(format!("{file}:{}", n + 1));
                }
            }
        }
    }

    assert_eq!(
        callers.len(),
        2,
        "the ways past the second-factor gate are: {callers:?}. \
         Two are sanctioned, both in auth.rs and both after a check. \
         A third needs an argument written beside it and this number changed."
    );
    assert!(
        callers.iter().all(|c| c.starts_with("auth.rs")),
        "something outside auth.rs issues a session without the gate: {callers:?}"
    );
}

// ---------------------------------------------------------------------------
// Getting back in
// ---------------------------------------------------------------------------

const LINK_BASE: &str = "https://erp.test/reset/";

/// **The whole point: somebody who has forgotten their password gets back in.**
///
/// And every session they had ends, because a reset means the old password is
/// not trusted and a session minted under it is that password still working.
#[tokio::test]
async fn a_reset_link_sets_a_new_password_and_ends_every_session() {
    let fixture = Fixture::new().await;
    let (old_token, _) = fixture
        .control
        .log_in(HANDLE, PASSWORD)
        .await
        .expect("the old password works");

    let link = fixture
        .control
        .request_password_reset(HANDLE, erp_i18n::Locale::English, LINK_BASE)
        .await
        .expect("a known address gets a link")
        .expect("and it is a link");

    fixture
        .control
        .reset_password(
            link.expose(),
            "correcthorsebattery",
            None,
            at(1_700_000_000),
            &fixture.sealing,
        )
        .await
        .expect("the new password is set");

    fixture
        .control
        .log_in(HANDLE, "correcthorsebattery")
        .await
        .expect("the new password works");
    assert!(
        fixture.control.log_in(HANDLE, PASSWORD).await.is_err(),
        "the old password still works"
    );
    assert!(
        fixture.control.session(old_token.expose()).await.is_err(),
        "a session minted under the old password survived the reset"
    );

    fixture.cleanup().await;
}

/// **A link works once.**
#[tokio::test]
async fn a_reset_link_cannot_be_spent_twice() {
    let fixture = Fixture::new().await;
    let link = fixture
        .control
        .request_password_reset(HANDLE, erp_i18n::Locale::English, LINK_BASE)
        .await
        .expect("a link")
        .expect("a link");

    fixture
        .control
        .reset_password(
            link.expose(),
            "correcthorsebattery",
            None,
            at(1_700_000_000),
            &fixture.sealing,
        )
        .await
        .expect("the first time");

    let refused = fixture
        .control
        .reset_password(
            link.expose(),
            "anotherpasswordentirely",
            None,
            at(1_700_000_000),
            &fixture.sealing,
        )
        .await
        .expect_err("the second time");
    assert!(
        matches!(refused, erp_control::PasswordError::NotValid),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **An address with no account is answered the same, and mailed nothing.**
///
/// Not `Err`, because the caller must not learn which addresses are registered;
/// and `None`, because writing and mailing for any address anybody types is an
/// unauthenticated mail cannon aimed at strangers — and the cost lands on the
/// sending domain that carries every tenant's signup and invitation mail.
#[tokio::test]
async fn an_address_with_no_account_is_answered_the_same_and_sent_nothing() {
    let fixture = Fixture::new().await;

    let nothing = fixture
        .control
        .request_password_reset("nobody@bassat.test", erp_i18n::Locale::English, LINK_BASE)
        .await
        .expect("answered, not refused");

    assert!(nothing.is_none(), "a stranger's address got a link");

    fixture.cleanup().await;
}

/// **A reset replaces the password. It does not replace the second factor.**
///
/// The mailbox is exactly what a second factor exists to survive: whoever holds
/// the password, or the laptop with a mail client signed in, holds the link. If
/// that were enough, enrolling would protect the login form and nothing else.
///
/// And **the link is left unspent** when no code came, because "this account
/// has a factor" is the next screen rather than a failure.
#[tokio::test]
async fn a_reset_cannot_walk_around_a_second_factor() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (_secret, recovery) = fixture.enrolled(now).await;

    let link = fixture
        .control
        .request_password_reset(HANDLE, erp_i18n::Locale::English, LINK_BASE)
        .await
        .expect("a link")
        .expect("a link");

    let refused = fixture
        .control
        .reset_password(
            link.expose(),
            "correcthorsebattery",
            None,
            now,
            &fixture.sealing,
        )
        .await
        .expect_err("no code, no reset");
    assert!(
        matches!(
            refused,
            erp_control::PasswordError::Auth(erp_control::AuthError::SecondFactorRequired)
        ),
        "{refused:?}"
    );
    assert!(
        fixture
            .control
            .log_in(HANDLE, "correcthorsebattery")
            .await
            .is_err(),
        "the refused reset wrote the password anyway"
    );

    // **A recovery code is the door**, and it is the door those codes exist for.
    fixture
        .control
        .reset_password(
            link.expose(),
            "correcthorsebattery",
            Some(&recovery[0]),
            now,
            &fixture.sealing,
        )
        .await
        .expect("the link survived the refusal and the code opens it");

    fixture.cleanup().await;
}

/// **A reset issues nothing.**
///
/// `disable_second_factor` takes a live session and nothing else, so a reset
/// that handed one back would be a two-call factor removal: open the link, get
/// a session, delete the enrolment. The signature is the guard — there is no
/// session in it to return — and this is the test that notices if one appears.
#[tokio::test]
async fn a_reset_leaves_the_second_factor_standing() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (_secret, recovery) = fixture.enrolled(now).await;

    let link = fixture
        .control
        .request_password_reset(HANDLE, erp_i18n::Locale::English, LINK_BASE)
        .await
        .expect("a link")
        .expect("a link");
    fixture
        .control
        .reset_password(
            link.expose(),
            "correcthorsebattery",
            Some(&recovery[0]),
            now,
            &fixture.sealing,
        )
        .await
        .expect("reset");

    // Still enrolled, and the new password alone still will not open it.
    assert!(
        fixture
            .control
            .has_second_factor(fixture.identity)
            .await
            .expect("asks"),
        "the reset took the second factor with it"
    );
    let refused = fixture
        .control
        .log_in(HANDLE, "correcthorsebattery")
        .await
        .expect_err("the factor still stands");
    assert!(
        matches!(refused, erp_control::AuthError::SecondFactorRequired),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **Changing a password needs the one being replaced.**
///
/// A session left open on a shared machine is not proof of anything.
#[tokio::test]
async fn changing_a_password_needs_the_current_one() {
    let fixture = Fixture::new().await;

    let refused = fixture
        .control
        .change_password(fixture.identity, "not the password", "correcthorsebattery")
        .await
        .expect_err("a guess is not the current password");
    assert!(
        matches!(
            refused,
            erp_control::PasswordError::Auth(erp_control::AuthError::InvalidCredentials)
        ),
        "{refused:?}"
    );

    fixture
        .control
        .change_password(fixture.identity, PASSWORD, "correcthorsebattery")
        .await
        .expect("the current password opens it");
    fixture
        .control
        .log_in(HANDLE, "correcthorsebattery")
        .await
        .expect("the new one works");

    fixture.cleanup().await;
}

/// **A stolen session cannot strip the control the theft was meant to run
/// into.**
///
/// Turning a second factor off used to need a live session and nothing else,
/// and a session is a bearer token left on shared machines and in browser
/// history. The factor is the one thing somebody who worked their way to a
/// session does not have — which is why it asks for that rather than the
/// password, the thing they probably do.
#[tokio::test]
async fn turning_it_off_needs_the_factor_being_turned_off() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (secret, _recovery) = fixture.enrolled(now).await;

    let refused = fixture
        .control
        .disable_second_factor(fixture.identity, None, now, &fixture.sealing)
        .await
        .expect_err("a session alone is not proof");
    assert!(
        matches!(refused, erp_control::AuthError::SecondFactorRequired),
        "{refused:?}"
    );

    let wrong = fixture
        .control
        .disable_second_factor(fixture.identity, Some("000000"), now, &fixture.sealing)
        .await
        .expect_err("a guess is not a code");
    assert!(
        matches!(wrong, erp_control::AuthError::InvalidCredentials),
        "{wrong:?}"
    );

    assert!(
        fixture
            .control
            .has_second_factor(fixture.identity)
            .await
            .expect("asks"),
        "a refused removal removed it anyway"
    );

    // The real code takes it off.
    let code = totp::code_at(&secret, seconds(now), totp::DIGITS).expect("a code");
    fixture
        .control
        .disable_second_factor(fixture.identity, Some(&code), now, &fixture.sealing)
        .await
        .expect("the factor turns off its own factor");

    fixture.cleanup().await;
}

/// **Replacing a factor is removing one, and it asks for what it destroys.**
///
/// This was worse than turning it off. `confirm_second_factor` deleted the live
/// enrolment *and all ten recovery codes* with no proof of either, so one
/// transient session pointed the account at the attacker's authenticator app
/// and took away the paper that would have let the owner back in — leaving them
/// *holding* a factor rather than merely dropping one.
#[tokio::test]
async fn replacing_a_factor_needs_the_one_being_replaced() {
    let fixture = Fixture::new().await;
    let now = at(1_700_000_000);
    let (secret, recovery) = fixture.enrolled(now).await;

    let enrolment = fixture
        .control
        .begin_second_factor(fixture.identity, "Bassat", HANDLE, &fixture.sealing)
        .await
        .expect("a second enrolment may be started");
    let waiting = totp::unbase32(&enrolment.secret).expect("base32");
    let fresh = totp::code_at(&waiting, seconds(now), totp::DIGITS).expect("a code");

    let refused = fixture
        .control
        .confirm_second_factor(fixture.identity, &fresh, None, now, &fixture.sealing)
        .await
        .expect_err("a session alone must not repoint the factor");
    assert!(
        matches!(refused, erp_control::AuthError::SecondFactorRequired),
        "{refused:?}"
    );

    // The old factor and its paper are untouched by the refusal.
    assert_eq!(
        fixture
            .control
            .recovery_codes_left(fixture.identity)
            .await
            .expect("counts"),
        i64::try_from(recovery.len()).expect("small"),
        "a refused replacement burned the recovery codes"
    );
    let old = totp::code_at(&secret, seconds(now), totp::DIGITS).expect("a code");
    fixture
        .control
        .verify_second_factor(fixture.identity, &old, now, &fixture.sealing)
        .await
        .expect("the old factor still verifies");

    fixture.cleanup().await;
}
