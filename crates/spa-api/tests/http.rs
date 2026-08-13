//! The HTTP surface, driven through the real router.
//!
//! No mocked state and no handler called directly: the requests go through
//! `Router::oneshot`, so the extractors, the rejections and the status mapping
//! are all under test. A handler tested by calling it has skipped the part most
//! likely to be wrong.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use spa_api::{AppState, router};
use spa_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, Scope, TenantPools};
use spa_testkit::{Schema, TestDb};
use spa_types::{IdentityId, TenantId};
use tower::ServiceExt;

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);

struct Fixture {
    app: Router,
    control: Arc<ControlPlane>,
    _db: TestDb,
    tenant_databases: Vec<String>,
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

        let control = Arc::new(ControlPlane::new(
            db.pool().clone(),
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
            .expect("cluster registers");

        Self {
            app: router(AppState::new(Arc::clone(&control))),
            control,
            _db: db,
            tenant_databases: Vec::new(),
        }
    }

    /// An identity with a password, and no memberships.
    async fn user(&self, handle: &str, password: &str) -> IdentityId {
        let identity = self
            .control
            .create_identity(Actor::system())
            .await
            .expect("identity is created");
        self.control
            .set_password(identity.id, handle.to_owned(), password.to_owned())
            .await
            .expect("password is set");
        identity.id
    }

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

    async fn join(&self, identity: IdentityId, tenant: TenantId) {
        self.control
            .grant_membership(identity, Scope::Tenant(tenant), "owner", Actor::system())
            .await
            .expect("membership is granted");
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value, Vec<u8>) {
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router responds");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json, content_type)
    }

    /// Logs in and returns the bearer token.
    async fn token(&self, handle: &str, password: &str) -> String {
        let (status, body, _) = self
            .send(
                Request::post("/v1/sessions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "handle": handle, "password": password }).to_string(),
                    ))
                    .expect("request builds"),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["token"].as_str().expect("a token").to_owned()
    }

    /// Installs the ledger module's read models, as `enable_module` will.
    async fn enable_ledger(&self, tenant: TenantId) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        ledger::install(&mut conn).await.expect("module schema");
        spa_projection::ensure_group_schema::<ledger::Ledger>(&mut conn)
            .await
            .expect("group checkpoint");
    }

    /// Drives the ledger projections, standing in for the worker.
    async fn project_ledger(&self, tenant: TenantId) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let owned = ledger::projections();
        let refs: Vec<&dyn spa_projection::Projection<Group = ledger::Ledger>> =
            owned.iter().map(AsRef::as_ref).collect();

        loop {
            let mut tx = db.begin().await.expect("transaction");
            let progress = spa_projection::run_once_in::<ledger::Ledger>(
                &mut tx,
                &refs,
                ledger::upcasters(),
                200,
            )
            .await
            .expect("projects");
            if matches!(progress, spa_projection::Progress::Advanced { .. }) {
                tx.commit().await.expect("commits");
            } else {
                tx.rollback().await.expect("rolls back");
                break;
            }
        }
    }

    async fn cleanup(self) {
        for name in &self.tenant_databases {
            let _ = spa_testkit::drop_named_database(name).await;
        }
    }
}

fn get(path: &str) -> axum::http::request::Builder {
    Request::get(path)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_needs_no_credential_and_no_database() {
    let fixture = Fixture::new().await;
    let (status, body, _) = fixture
        .send(get("/v1/health").body(Body::empty()).unwrap())
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    fixture.cleanup().await;
}

#[tokio::test]
async fn logging_in_returns_a_token_that_works() {
    let fixture = Fixture::new().await;
    fixture
        .user("owner@acme.test", "correct horse battery staple")
        .await;

    let token = fixture
        .token("owner@acme.test", "correct horse battery staple")
        .await;
    assert_eq!(token.len(), 64, "32 random bytes, hex encoded");

    // And it authenticates.
    let (status, _, _) = fixture
        .send(
            Request::delete("/v1/sessions/current")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    fixture.cleanup().await;
}

/// A logged-out token stops working **immediately**, not after a cache TTL.
#[tokio::test]
async fn logging_out_ends_the_session_at_once() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let authorized = |t: &str| {
        Request::get("/v1/tenants/acme")
            .header(header::AUTHORIZATION, format!("Bearer {t}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, _, _) = fixture.send(authorized(&token)).await;
    assert_eq!(status, StatusCode::OK);

    fixture
        .send(
            Request::delete("/v1/sessions/current")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    let (status, _, _) = fixture.send(authorized(&token)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked session must not survive in a cache"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_handle_are_indistinguishable() {
    let fixture = Fixture::new().await;
    fixture.user("owner@acme.test", "hunter2hunter2").await;

    let attempt = |handle: &str, password: &str| {
        Request::post("/v1/sessions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "handle": handle, "password": password }).to_string(),
            ))
            .unwrap()
    };

    let (wrong_password, a, _) = fixture.send(attempt("owner@acme.test", "nope")).await;
    let (unknown_handle, b, _) = fixture.send(attempt("nobody@acme.test", "nope")).await;

    assert_eq!(wrong_password, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_handle, StatusCode::UNAUTHORIZED);
    assert_eq!(
        a, b,
        "the two responses must be byte-identical, or the API is an \
         account-enumeration oracle"
    );

    fixture.cleanup().await;
}

/// **The isolation property, over HTTP.**
#[tokio::test]
async fn a_member_of_one_tenant_cannot_read_another() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let _globex = fixture.provision("globex").await;
    fixture.join(user, acme).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let read = |slug: &str| {
        Request::get(format!("/v1/tenants/{slug}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (mine, _, _) = fixture.send(read("acme")).await;
    assert_eq!(mine, StatusCode::OK);

    let (theirs, theirs_body, _) = fixture.send(read("globex")).await;
    let (missing, missing_body, _) = fixture.send(read("does-not-exist")).await;

    assert_eq!(theirs, StatusCode::NOT_FOUND);
    assert_eq!(
        theirs_body, missing_body,
        "a tenant that exists but is not yours must be indistinguishable from \
         one that does not exist, or the API enumerates our customers"
    );
    assert_eq!(missing, StatusCode::NOT_FOUND);

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_absent_or_malformed_credential_is_a_401_not_a_500() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;

    for header_value in [
        None,
        Some(""),
        Some("Bearer"),
        Some("Bearer "),
        Some("Basic abc"),
        Some("Bearer not-a-token"),
    ] {
        let mut request = Request::get("/v1/tenants/acme");
        if let Some(value) = header_value {
            request = request.header(header::AUTHORIZATION, value);
        }
        let (status, body, content_type) = fixture.send(request.body(Body::empty()).unwrap()).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "for {header_value:?}: {body}"
        );
        assert_eq!(content_type, b"application/problem+json");
        assert_eq!(body["code"], "auth.session_expired");
    }

    fixture.cleanup().await;
}

/// **The reason errors are codes and not sentences.**
#[tokio::test]
async fn errors_are_localized_but_their_codes_are_not() {
    let fixture = Fixture::new().await;

    let attempt = |language: &str| {
        Request::post("/v1/sessions")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT_LANGUAGE, language)
            .body(Body::from(
                serde_json::json!({ "handle": "nobody", "password": "nope" }).to_string(),
            ))
            .unwrap()
    };

    let (_, english, content_type) = fixture.send(attempt("en")).await;
    let (_, arabic, _) = fixture.send(attempt("ar-SA,ar;q=0.9,en;q=0.5")).await;

    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(
        english["code"], arabic["code"],
        "the code is what a client branches on, so it must not move with the \
         language"
    );
    assert_ne!(
        english["detail"], arabic["detail"],
        "and the prose must, or Accept-Language is decorative"
    );
    assert!(
        arabic["detail"]
            .as_str()
            .expect("a detail")
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "the Arabic response should actually be in Arabic: {}",
        arabic["detail"]
    );

    fixture.cleanup().await;
}

/// An unparseable body is a 400, and does not reveal anything about the route.
#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let fixture = Fixture::new().await;
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_suspended_identity_cannot_log_in() {
    let fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;

    fixture
        .control
        .suspend_identity(user, "offboarded", Actor::system())
        .await
        .expect("suspends");

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "owner@acme.test",
                        "password": "hunter2hunter2"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body["code"], "auth.invalid_credentials",
        "a suspended account must not be distinguishable from a wrong password"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The ledger, over HTTP
// ---------------------------------------------------------------------------

/// **The whole path, once.**
///
/// Sign in, enter a tenant, open two accounts, post an entry, and read a trial
/// balance that agrees with itself — through the real router, against a real
/// tenant database. Everything before this tested one layer.
#[tokio::test]
async fn a_signed_in_user_can_keep_books() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let post = |path: &str, body: serde_json::Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    for (code, kind) in [("1000", "asset"), ("4000", "revenue")] {
        let (status, body, _) = fixture
            .send(post(
                "/v1/tenants/acme/ledger/accounts",
                serde_json::json!({
                    "code": code, "name": code, "kind": kind, "currency": "SAR"
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body, _) = fixture
        .send(post(
            "/v1/tenants/acme/ledger/entries",
            serde_json::json!({
                "id": "inv-1",
                "occurred_on": "2026-01-15T00:00:00Z",
                "memo": "Invoice 1",
                "lines": [
                    { "account": "1000", "amount": { "minor": 15000, "currency": "SAR" } },
                    { "account": "4000", "amount": { "minor": -15000, "currency": "SAR" } }
                ]
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lines"], 2);
    assert!(
        body["position"].as_i64().is_some(),
        "the write reports where it landed"
    );

    // Read models are driven by the worker, so a read straight after a write
    // sees nothing yet. That is the design, not a bug — the API exposes it as
    // `?consistent_after=`, which is why the write returned a position.
    fixture.project_ledger(tenant).await;

    let read = |path: &str| {
        Request::get(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, accounts, _) = fixture.send(read("/v1/tenants/acme/ledger/accounts")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accounts[0]["code"], "1000");
    assert_eq!(accounts[0]["balance"], 15000);
    assert_eq!(accounts[1]["balance"], -15000);

    let (status, trial, _) = fixture
        .send(read("/v1/tenants/acme/ledger/trial-balance"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trial[0]["currency"], "SAR");
    assert_eq!(trial[0]["difference"], 0);
    assert_eq!(trial[0]["debits"], trial[0]["credits"]);
    assert_eq!(trial[0]["balances"], true);

    fixture.cleanup().await;
}

/// An unbalanced entry is a 400 that says by how much — in the caller's
/// language.
#[tokio::test]
async fn an_unbalanced_entry_is_refused_with_the_difference() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/entries")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({
                        "id": "inv-1",
                        "occurred_on": "2026-01-15T00:00:00Z",
                        "lines": [
                            { "account": "1000", "amount": { "minor": 15000, "currency": "SAR" } },
                            { "account": "4000", "amount": { "minor": -14900, "currency": "SAR" } }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "ledger.does_not_balance");
    assert!(
        body["detail"].as_str().unwrap().contains("1.00 SAR"),
        "the message must say by how much: {}",
        body["detail"]
    );
    // And nothing was written — the type refused before the command ran.
    fixture.cleanup().await;
}

/// Posting into an account that does not exist is a 422, not a 500.
#[tokio::test]
async fn posting_to_an_unknown_account_is_unprocessable() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/entries")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "id": "inv-1",
                        "occurred_on": "2026-01-15T00:00:00Z",
                        "lines": [
                            { "account": "9998", "amount": { "minor": 100, "currency": "SAR" } },
                            { "account": "9999", "amount": { "minor": -100, "currency": "SAR" } }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "ledger.no_such_account");
    fixture.cleanup().await;
}

/// A member of another tenant cannot read this one's books.
#[tokio::test]
async fn the_ledger_is_behind_the_same_tenant_check_as_everything_else() {
    let mut fixture = Fixture::new().await;
    let outsider = fixture.user("nosy@globex.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    fixture.join(outsider, globex).await;
    fixture.enable_ledger(acme).await;

    let token = fixture.token("nosy@globex.test", "hunter2hunter2").await;
    let (status, _, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/ledger/trial-balance")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "module routes get the tenant check for free, because they take the \
         same extractor"
    );
    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Signup
// ---------------------------------------------------------------------------

/// **The self-provisioning requirement, over HTTP.**
///
/// One request, and the person who made it has a working system they are
/// already logged into.
#[tokio::test]
async fn signing_up_gives_you_a_working_system() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme",
                        "company": "Acme Trading",
                        "email": "owner@acme.test",
                        "password": "correct horse battery staple",
                        "modules": ["ledger"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "acme");
    assert_eq!(body["modules"][0], "ledger");
    let token = body["token"].as_str().expect("a token").to_owned();

    // The token works immediately — signing up logs you in.
    let (status, tenant, _) = fixture
        .send(
            Request::get("/v1/tenants/acme")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tenant}");
    assert_eq!(tenant["modules"][0], "ledger");

    // And the ledger is installed and usable, with no further setup.
    let (status, accounts, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accounts}");
    assert_eq!(accounts.as_array().expect("a list").len(), 0);

    let (status, _, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "code": "1000", "name": "Cash", "kind": "asset", "currency": "SAR"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a brand-new tenant can keep books straight away"
    );

    let _ = spa_testkit::drop_named_database(
        body["tenant"]
            .as_str()
            .map_or("spa_tenant_acme", |_| "spa_tenant_acme"),
    )
    .await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn a_short_password_is_refused_before_anything_is_built() {
    let fixture = Fixture::new().await;

    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme", "company": "Acme",
                        "email": "owner@acme.test", "password": "short"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "request.password_too_short");
    // The plural form is selected, not left as a placeholder.
    assert!(
        !body["detail"].as_str().unwrap().contains("{n}"),
        "{}",
        body["detail"]
    );

    fixture.cleanup().await;
}

/// A typo in a module name is refused, not silently ignored.
#[tokio::test]
async fn an_unknown_module_is_refused() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme", "company": "Acme",
                        "email": "owner@acme.test",
                        "password": "correct horse battery staple",
                        "modules": ["ledgre"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "request.unknown_module");
    assert!(
        body["detail"].as_str().unwrap().contains("ledgre"),
        "the message should name the typo: {}",
        body["detail"]
    );

    fixture.cleanup().await;
}

/// A taken name is a 409, and does not disturb the tenant that has it.
#[tokio::test]
async fn a_taken_name_is_a_conflict() {
    let fixture = Fixture::new().await;

    let signup = |email: &str| {
        Request::post("/v1/signups")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "slug": "acme", "company": "Acme",
                    "email": email, "password": "correct horse battery staple"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (first, _, _) = fixture.send(signup("a@acme.test")).await;
    assert_eq!(first, StatusCode::CREATED);

    let (second, body, _) = fixture.send(signup("b@acme.test")).await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(body["code"], "provisioning.slug_taken");

    let _ = spa_testkit::drop_named_database("spa_tenant_acme").await;
    fixture.cleanup().await;
}

/// The catalogue is readable without an account — a signup form has to show it.
#[tokio::test]
async fn the_chart_catalogue_needs_no_credential_and_speaks_arabic() {
    let fixture = Fixture::new().await;

    let (status, charts, _) = fixture
        .send(get("/v1/ledger/charts").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!charts.as_array().expect("a list").is_empty());
    assert_eq!(charts[0]["id"], "services");
    assert!(charts[0]["accounts"].as_u64().expect("a count") > 10);
    // The preview is what "modify before installing" needs: you can see every
    // account before committing to it.
    assert_eq!(
        u64::try_from(charts[0]["preview"].as_array().expect("a preview").len()).unwrap(),
        charts[0]["accounts"].as_u64().expect("a count")
    );

    let (_, arabic, _) = fixture
        .send(
            get("/v1/ledger/charts")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let name = arabic[0]["preview"][0]["name"].as_str().expect("a name");
    assert!(
        name.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "the catalogue must be readable in Arabic: {name}"
    );
    // The id does not move with the language — it is what gets sent back.
    assert_eq!(arabic[0]["id"], "services");

    fixture.cleanup().await;
}

/// **Signup to a working chart of accounts, in three requests.**
#[tokio::test]
async fn a_new_tenant_can_start_from_a_template() {
    let fixture = Fixture::new().await;

    let (_, signed_up, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme", "company": "Acme Trading",
                        "email": "owner@acme.test",
                        "password": "correct horse battery staple",
                        "modules": ["ledger"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    let token = signed_up["token"].as_str().expect("a token").to_owned();

    let (status, installed, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/chart")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({ "template": "services", "currency": "SAR" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert!(installed["opened"].as_u64().expect("a count") > 10);
    assert_eq!(installed["skipped"], 0);

    // Projections have to run before the chart is readable — the API exposes
    // that gap, it does not hide it.
    fixture
        .project_ledger(
            signed_up["tenant"]
                .as_str()
                .expect("a tenant id")
                .parse()
                .expect("a uuid"),
        )
        .await;

    let (status, accounts, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let accounts = accounts.as_array().expect("a list");
    assert!(accounts.len() > 10);

    // Installed in Arabic, because that is what the request asked for.
    let name = accounts[0]["name"].as_str().expect("a name");
    assert!(
        name.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "installed in Arabic: {name}"
    );

    // And it is immediately usable: VAT accounts are there, so a first invoice
    // does not need a Saudi business to fix the chart first.
    let codes: Vec<&str> = accounts
        .iter()
        .map(|a| a["code"].as_str().expect("a code"))
        .collect();
    assert!(codes.contains(&"2100"), "output VAT: {codes:?}");
    assert!(codes.contains(&"2300"), "Zakat: {codes:?}");

    let _ = spa_testkit::drop_named_database("spa_tenant_acme").await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn an_unknown_chart_is_refused() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/chart")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "template": "retial", "currency": "SAR" }).to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "request.unknown_chart");
    fixture.cleanup().await;
}
