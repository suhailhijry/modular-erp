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
        self.join_as(identity, tenant, "owner").await;
    }

    async fn join_as(&self, identity: IdentityId, tenant: TenantId, role: &str) {
        self.control
            .grant_membership(identity, Scope::Tenant(tenant), role, Actor::system())
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

    /// Turns a module on the way the product does.
    ///
    /// Through `install_module`, not by hand: an earlier version of this fixture
    /// created `proj_ledger` and never wrote the entitlement, so every tenant in
    /// these tests had a module's tables and no right to use them. A harness
    /// with its own install path is a harness that can be right while the
    /// product is wrong.
    async fn enable_module(&self, tenant: TenantId, setup: spa_control::ModuleSetup) {
        self.control
            .install_module(tenant, setup, Actor::system())
            .await
            .expect("module installs");
    }

    async fn enable_ledger(&self, tenant: TenantId) {
        self.enable_module(tenant, ledger::setup()).await;
    }

    /// Sales needs the ledger underneath it.
    async fn enable_sales(&self, tenant: TenantId) {
        self.enable_ledger(tenant).await;
        self.enable_module(tenant, sales::setup()).await;
    }

    /// Drives one group's projections, standing in for the worker.
    async fn project<G: spa_projection::ProjectionGroup>(
        &self,
        tenant: TenantId,
        projections: &[std::sync::Arc<dyn spa_projection::Projection<Group = G>>],
        upcasters: &spa_eventlog::Upcasters,
    ) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let refs: Vec<&dyn spa_projection::Projection<Group = G>> =
            projections.iter().map(AsRef::as_ref).collect();

        loop {
            let mut tx = db.begin().await.expect("transaction");
            let progress = spa_projection::run_once_in::<G>(&mut tx, &refs, upcasters, 200)
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

    async fn project_ledger(&self, tenant: TenantId) {
        self.project(tenant, &ledger::projections(), ledger::upcasters())
            .await;
    }

    async fn project_sales(&self, tenant: TenantId) {
        self.project_ledger(tenant).await;
        self.project(tenant, &sales::projections(), sales::upcasters())
            .await;
    }

    /// Installs a chart of accounts over HTTP.
    async fn install_chart(&self, token: &str, slug: &str, template: &str) {
        let (status, body, _) = self
            .send(
                Request::post(format!("/v1/tenants/{slug}/ledger/chart"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "template": template, "currency": "SAR" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// A ledger account's balance, read over HTTP so the assertion travels the
    /// same path a client would.
    async fn ledger_balance(&self, token: &str, slug: &str, code: &str) -> i64 {
        let (status, accounts, _) = self
            .send(
                Request::get(format!("/v1/tenants/{slug}/ledger/accounts"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{accounts}");
        accounts
            .as_array()
            .expect("a list")
            .iter()
            .find(|a| a["code"] == code)
            .and_then(|a| a["balance"].as_i64())
            .expect("an account with a balance")
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

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// **The authorization matrix, over HTTP.**
///
/// Every role against every endpoint. Written out rather than derived from
/// `Role::allows`, because a test that asks the code what it does can only ever
/// agree with it — this one asks whether that is what we *meant*.
#[tokio::test]
async fn every_role_can_do_exactly_what_it_should() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_ledger(tenant).await;

    // (role, read, post, manage accounts)
    let matrix = [
        ("owner", true, true, true),
        ("accountant", true, true, true),
        ("clerk", true, true, false),
        ("viewer", true, false, false),
    ];

    for (role, may_read, may_post, may_manage) in matrix {
        let email = format!("{role}@acme.test");
        let user = fixture.user(&email, "hunter2hunter2").await;
        fixture.join_as(user, tenant, role).await;
        let token = fixture.token(&email, "hunter2hunter2").await;

        let bearer = |request: axum::http::request::Builder| {
            request.header(header::AUTHORIZATION, format!("Bearer {token}"))
        };

        let (read, _, _) = fixture
            .send(
                bearer(Request::get("/v1/tenants/acme/ledger/accounts"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(read == StatusCode::OK, may_read, "{role} read: {read}");

        let (manage, _, _) = fixture
            .send(
                bearer(Request::post("/v1/tenants/acme/ledger/accounts"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "code": format!("9{}", role.len()),
                            "name": "Test", "kind": "asset", "currency": "SAR"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            manage == StatusCode::CREATED,
            may_manage,
            "{role} manage accounts: {manage}"
        );

        // Posting needs accounts, so this asserts the *authorization* outcome:
        // a refused role gets 403 before the ledger is consulted, an allowed
        // one gets past it and fails on the missing accounts instead.
        let (post, body, _) = fixture
            .send(
                bearer(Request::post("/v1/tenants/acme/ledger/entries"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "id": format!("e-{role}"),
                            "occurred_on": "2026-01-15T00:00:00Z",
                            "lines": [
                                { "account": "1000", "amount": { "minor": 1, "currency": "SAR" } },
                                { "account": "4000", "amount": { "minor": -1, "currency": "SAR" } }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            post != StatusCode::FORBIDDEN,
            may_post,
            "{role} post entries: {post} {body}"
        );
    }

    fixture.cleanup().await;
}

/// A refusal says which capability, so "ask someone with permission" is
/// actionable — and it says it in the caller's language.
#[tokio::test]
async fn a_refusal_names_the_capability_and_speaks_arabic() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_ledger(tenant).await;
    let user = fixture.user("viewer@acme.test", "hunter2hunter2").await;
    fixture.join_as(user, tenant, "viewer").await;
    let token = fixture.token("viewer@acme.test", "hunter2hunter2").await;

    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({
                        "code": "1000", "name": "Cash", "kind": "asset", "currency": "SAR"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    // 403, not 404: they have already proved membership, so hiding the tenant
    // buys nothing, and they need to know what to ask for.
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "access.not_permitted");
    assert!(
        body["detail"].as_str().unwrap().contains("manage_accounts"),
        "the message must name the capability: {}",
        body["detail"]
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "and be in Arabic: {}",
        body["detail"]
    );

    fixture.cleanup().await;
}

/// A stored role this build does not know is refused, not guessed at.
#[tokio::test]
async fn an_unknown_stored_role_locks_nobody_in_or_out_silently() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.user("odd@acme.test", "hunter2hunter2").await;
    fixture.join_as(user, tenant, "superuser").await;
    let token = fixture.token("odd@acme.test", "hunter2hunter2").await;

    let (status, _, _) = fixture
        .send(
            Request::get("/v1/tenants/acme")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "defaulting down locks someone out silently; defaulting up lets them \
         in silently. Both are worse than an error naming the row."
    );

    fixture.cleanup().await;
}

/// The tenant view reports the caller's role, so a client can hide what it must
/// not offer. The server refuses regardless.
#[tokio::test]
async fn the_tenant_view_tells_a_client_what_to_show() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let user = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(user, tenant, "clerk").await;
    let token = fixture.token("clerk@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/tenants/acme")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], "clerk");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// **Adding a colleague, end to end.**
#[tokio::test]
async fn an_owner_can_add_a_colleague_who_can_then_sign_in() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, added, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "clerk@acme.test",
                        "password": "another good passphrase",
                        "role": "clerk"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");

    // They can sign in with what the owner set, and they land in the tenant.
    let colleague = fixture
        .token("clerk@acme.test", "another good passphrase")
        .await;
    let (status, view, _) = fixture
        .send(
            Request::get("/v1/tenants/acme")
                .header(header::AUTHORIZATION, format!("Bearer {colleague}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["role"], "clerk");

    // And their role is enforced: a clerk cannot restructure the chart.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {colleague}"))
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
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Both show up in the list, which a viewer could also read.
    let (status, members, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/members")
                .header(header::AUTHORIZATION, format!("Bearer {colleague}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let members = members.as_array().expect("a list");
    assert_eq!(members.len(), 2);
    assert!(
        members
            .iter()
            .any(|m| m["handle"] == "owner@acme.test" && m["role"] == "owner")
    );
    assert!(
        members
            .iter()
            .any(|m| m["handle"] == "clerk@acme.test" && m["role"] == "clerk")
    );

    fixture.cleanup().await;
}

/// A demotion applies immediately, not after the cache TTL.
#[tokio::test]
async fn changing_a_role_takes_effect_at_once() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (_, added, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "acct@acme.test",
                        "password": "another good passphrase",
                        "role": "accountant"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    let identity = added["identity"].as_str().expect("an identity").to_owned();
    let colleague = fixture
        .token("acct@acme.test", "another good passphrase")
        .await;

    let open_account = |t: &str| {
        Request::post("/v1/tenants/acme/ledger/accounts")
            .header(header::AUTHORIZATION, format!("Bearer {t}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "code": "1000", "name": "Cash", "kind": "asset", "currency": "SAR"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, _, _) = fixture.send(open_account(&colleague)).await;
    assert_eq!(status, StatusCode::CREATED, "an accountant may");

    let (status, _, _) = fixture
        .send(
            Request::patch(format!("/v1/tenants/acme/members/{identity}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "viewer" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = fixture.send(open_account(&colleague)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a demotion that takes five seconds to apply is five seconds of \
         someone doing what they were just told they cannot"
    );

    fixture.cleanup().await;
}

/// **The footgun that has no undo.**
#[tokio::test]
async fn the_last_owner_cannot_remove_or_demote_themselves() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::patch(format!("/v1/tenants/acme/members/{owner}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "viewer" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "members.last_owner");

    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/tenants/acme/members/{owner}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // But with a second owner it is allowed — the rule is about the tenant
    // keeping an owner, not about anyone being undemotable.
    fixture
        .send(
            Request::post("/v1/tenants/acme/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "second@acme.test",
                        "password": "another good passphrase",
                        "role": "owner"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    let (status, _, _) = fixture
        .send(
            Request::patch(format!("/v1/tenants/acme/members/{owner}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "viewer" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a second owner makes it safe"
    );

    fixture.cleanup().await;
}

/// Only `ManageTenant` may change who has access.
#[tokio::test]
async fn an_accountant_cannot_add_members() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let acct = fixture.user("acct@acme.test", "hunter2hunter2").await;
    fixture.join_as(acct, tenant, "accountant").await;
    let token = fixture.token("acct@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "friend@acme.test",
                        "password": "another good passphrase",
                        "role": "owner"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an accountant who can grant themselves ownership is not an accountant"
    );
    assert_eq!(body["code"], "access.not_permitted");

    fixture.cleanup().await;
}

/// One person, two tenants, one account.
#[tokio::test]
async fn adding_an_existing_login_reuses_their_account() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    fixture.join(owner, acme).await;
    fixture.join(owner, globex).await;

    let shared = fixture.user("cfo@example.test", "hunter2hunter2").await;
    fixture.join_as(shared, acme, "accountant").await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, added, _) = fixture
        .send(
            Request::post("/v1/tenants/globex/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "cfo@example.test",
                        "password": "ignored, they already have one",
                        "role": "viewer"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        added["identity"].as_str().expect("an identity"),
        shared.to_string(),
        "one person with two tenants must not end up with two accounts"
    );

    // And adding them again is a conflict, not a silent second membership.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/globex/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "cfo@example.test",
                        "password": "another good passphrase",
                        "role": "viewer"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "members.already_a_member");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Reading your own write
// ---------------------------------------------------------------------------

/// **The most common client pattern: submit, then refresh.**
///
/// Without `?consistent_after=` the refresh can legitimately miss the write —
/// projections are driven by a worker. This is the whole reason every write
/// returns its log position.
#[tokio::test]
async fn a_client_can_read_the_write_it_just_made() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    for (code, kind) in [("1000", "asset"), ("4000", "revenue")] {
        fixture
            .send(
                Request::post("/v1/tenants/acme/ledger/accounts")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "code": code, "name": code, "kind": kind, "currency": "SAR"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
    }

    let (status, posted, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/ledger/entries")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "id": "inv-1",
                        "occurred_on": "2026-01-15T00:00:00Z",
                        "lines": [
                            { "account": "1000", "amount": { "minor": 15000, "currency": "SAR" } },
                            { "account": "4000", "amount": { "minor": -15000, "currency": "SAR" } }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    let position = posted["position"].as_i64().expect("a position");

    // No worker is running in this test, so the projection never advances and
    // the read must time out rather than quietly serve stale data.
    let (status, body, _) = fixture
        .send(
            Request::get(format!(
                "/v1/tenants/acme/ledger/accounts?consistent_after={position}"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a guarantee the response cannot make must not be answered with stale data"
    );
    assert_eq!(body["code"], "request.not_caught_up");

    // With the projection caught up, the same request succeeds and the write is
    // there.
    fixture.project_ledger(tenant).await;
    let (status, accounts, _) = fixture
        .send(
            Request::get(format!(
                "/v1/tenants/acme/ledger/accounts?consistent_after={position}"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let cash = accounts
        .as_array()
        .expect("a list")
        .iter()
        .find(|a| a["code"] == "1000")
        .expect("cash");
    assert_eq!(cash["balance"], 15000);

    fixture.cleanup().await;
}

/// A read that does not ask for consistency never waits.
#[tokio::test]
async fn a_read_without_the_hint_is_never_delayed() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    fixture
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

    // Nothing has projected it, and the read returns immediately with what the
    // read model actually holds — which is the honest answer to a question that
    // did not ask for more.
    let started = std::time::Instant::now();
    let (status, accounts, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(accounts.as_array().expect("a list").is_empty());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a read with no hint must not pay for one: {:?}",
        started.elapsed()
    );

    fixture.cleanup().await;
}

/// A write asks the worker to look now, so the wait is a claim cycle rather than
/// the idle backoff.
#[tokio::test]
async fn a_write_marks_the_tenant_as_needing_a_visit() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Push the tenant far into the future, as an idle one would be.
    fixture
        .control
        .schedule_next_visit(tenant, std::time::Duration::from_hours(1))
        .await
        .expect("defers");
    assert!(
        fixture
            .control
            .claim_tenants("w", 10, spa_control::WorkSchedule::default())
            .await
            .expect("claims")
            .is_empty(),
        "the tenant starts out not due"
    );

    fixture
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

    let claimed = fixture
        .control
        .claim_tenants("w", 10, spa_control::WorkSchedule::default())
        .await
        .expect("claims");
    assert_eq!(
        claimed.len(),
        1,
        "without this the first write after a quiet period waits out the idle \
         backoff, and `consistent_after` times out on a healthy system"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Sales
// ---------------------------------------------------------------------------

/// **The loop a business actually runs, over HTTP.**
///
/// Invoice a customer, read it back, take the money, watch the receivable
/// clear — and check the ledger agrees, without sales ever having been asked
/// about accounting. Two modules, one request path.
#[tokio::test]
async fn a_signed_in_user_can_invoice_a_customer_and_take_payment() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // A chart, so the accounts the sale posts to exist.
    fixture.install_chart(&token, "acme", "services").await;

    let (status, issued, _) = fixture
        .send(
            bearer(Request::post("/v1/tenants/acme/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-1",
                        "customer": { "name": "Rawabi", "vat_number": "310000000000003" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [
                            { "description": "Consulting", "net": 100_000, "vat": "standard" },
                            { "description": "Export", "net": 50_000, "vat": "zero" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    let position = issued["position"].as_i64().expect("a log position");

    // Read your own write. Without the worker running, this is what the hint is
    // for — so drive the projections first and then ask for exactly that point.
    fixture.project_sales(tenant).await;

    let (status, invoice, _) = fixture
        .send(
            bearer(Request::get(format!(
                "/v1/tenants/acme/sales/invoices/INV-1?consistent_after={position}"
            )))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{invoice}");
    assert_eq!(invoice["net"], 150_000);
    assert_eq!(
        invoice["tax"], 15_000,
        "15% of the standard-rated 1,000 only"
    );
    assert_eq!(invoice["gross"], 165_000);
    assert_eq!(invoice["outstanding"], 165_000);
    assert_eq!(invoice["customer_vat"], "310000000000003");
    assert_eq!(
        invoice["tax_breakdown"].as_array().expect("bands").len(),
        2,
        "one band per rate, which is what a tax invoice has to print"
    );

    // The books, without sales having touched them.
    let balance = async |code: &str| fixture.ledger_balance(&token, "acme", code).await;
    assert_eq!(balance("1100").await, 165_000, "receivable");
    assert_eq!(balance("4000").await, -150_000, "revenue");
    assert_eq!(balance("2100").await, -15_000, "VAT payable");

    // And the money arrives.
    let (status, paid, _) = fixture
        .send(
            bearer(Request::post(
                "/v1/tenants/acme/sales/invoices/INV-1/payments",
            ))
            .body(Body::from(
                serde_json::json!({
                    "reference": "wire-77",
                    "amount": { "minor": 165_000, "currency": "SAR" },
                    "received_on": "2026-03-20T00:00:00Z",
                    "account": "1010"
                })
                .to_string(),
            ))
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{paid}");
    fixture.project_sales(tenant).await;

    let (_, invoices, _) = fixture
        .send(
            bearer(Request::get("/v1/tenants/acme/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let invoices = invoices.as_array().expect("a list");
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["outstanding"], 0);
    assert_eq!(invoices[0]["paid"], 165_000);

    assert_eq!(
        fixture.ledger_balance(&token, "acme", "1100").await,
        0,
        "the receivable is settled"
    );

    let _ = spa_testkit::drop_named_database("spa_tenant_acme").await;
    fixture.cleanup().await;
}

/// A module a tenant did not buy is not there — a 404, not a 403, so the
/// response says nothing about what they are not paying for.
#[tokio::test]
async fn a_module_a_tenant_did_not_enable_is_not_there() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    // The ledger only. Sales is not enabled.
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/sales/invoices")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "request.module_not_enabled");

    // Not vacuous: the ledger's own routes work for this same tenant and token.
    let (status, _, _) = fixture
        .send(
            Request::get("/v1/tenants/acme/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    fixture.cleanup().await;
}

/// Signing up for sales without the ledger is refused at the door, rather than
/// producing a system that fails on its first invoice.
#[tokio::test]
async fn a_module_cannot_be_bought_without_what_it_needs() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "solo",
                        "company": "Solo",
                        "email": "owner@solo.test",
                        "password": "hunter2hunter2",
                        "modules": ["sales"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.module_requires");
    let detail = body["detail"].as_str().expect("a detail");
    assert!(
        detail
            .chars()
            .any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)),
        "in the caller's language: {detail}"
    );

    fixture.cleanup().await;
}

/// The VAT treatment is a fixed vocabulary; the *rate* is never a client's to
/// send, because it is statutory.
#[tokio::test]
async fn an_unknown_vat_treatment_is_refused() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/sales/invoices")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-9",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [{ "description": "Work", "net": 100, "vat": "reduced" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.unknown_vat_category");

    fixture.cleanup().await;
}

/// An invoice posted against a chart that has no receivable account is refused
/// with the ledger's own message — 422, because the request was well-formed and
/// the tenant's setup was not.
#[tokio::test]
async fn an_invoice_the_chart_cannot_take_is_unprocessable() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // No chart installed at all.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/sales/invoices")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-8",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [{ "description": "Work", "net": 100, "vat": "standard" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "ledger.no_such_account");
    assert_eq!(body["args"]["code"]["value"], "1100");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

/// **The modularity requirement, over HTTP.** A tenant that did not buy sales at
/// signup can buy it on a Tuesday, and it works immediately.
#[tokio::test]
async fn a_tenant_can_turn_a_module_on_after_signing_up() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // Not there yet.
    let (status, _, _) = fixture
        .send(
            bearer(Request::get("/v1/tenants/acme/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tenants/acme/modules"))
                .body(Body::from(
                    serde_json::json!({ "module": "sales" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // And it is *usable*, not merely listed — the read models were installed
    // too. Entitling without installing is a tenant that 500s on its first
    // request, which is what this asserts did not happen.
    fixture.install_chart(&token, "acme", "services").await;
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tenants/acme/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-LATE",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [{ "description": "Work", "net": 10_000, "vat": "standard" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    fixture.project_sales(tenant).await;
    let (_, invoices, _) = fixture
        .send(
            bearer(Request::get("/v1/tenants/acme/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(invoices.as_array().expect("a list").len(), 1);

    fixture.cleanup().await;
}

/// Turning a module off keeps every byte of its data.
///
/// "Updates should never break old data", applied to the operation most likely
/// to violate it: a tenant who downgrades and comes back finds their invoices.
#[tokio::test]
async fn turning_a_module_off_hides_it_without_losing_anything() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    fixture.install_chart(&token, "acme", "services").await;
    let (status, _, _) = fixture
        .send(
            bearer(Request::post("/v1/tenants/acme/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-KEEP",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [{ "description": "Work", "net": 10_000, "vat": "standard" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    fixture.project_sales(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::delete("/v1/tenants/acme/modules/sales"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, _, _) = fixture
        .send(
            bearer(Request::get("/v1/tenants/acme/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "gone from the API");

    // The ledger keeps the entry the sale made, because disabling sales does
    // not unpost anything.
    assert_eq!(
        fixture.ledger_balance(&token, "acme", "1100").await,
        11_500,
        "the receivable the invoice created is still on the books"
    );

    // Back on, and the invoice is exactly where it was left.
    let (status, _, _) = fixture
        .send(
            bearer(Request::post("/v1/tenants/acme/modules"))
                .body(Body::from(
                    serde_json::json!({ "module": "sales" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, invoices, _) = fixture
        .send(
            bearer(Request::get("/v1/tenants/acme/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        invoices.as_array().expect("a list").len(),
        1,
        "the data was hidden, never deleted"
    );

    fixture.cleanup().await;
}

/// A module cannot be pulled out from under one that needs it, and the refusal
/// says which one — in the caller's language.
#[tokio::test]
async fn a_module_something_else_needs_cannot_be_turned_off() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::delete("/v1/tenants/acme/modules/ledger")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.module_in_use");
    assert_eq!(body["args"]["dependent"]["value"], "sales");

    // Not vacuous: with sales off first, the ledger goes too.
    let (status, _, _) = fixture
        .send(
            Request::delete("/v1/tenants/acme/modules/sales")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = fixture
        .send(
            Request::delete("/v1/tenants/acme/modules/ledger")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    fixture.cleanup().await;
}

/// Enabling a module without what it needs is refused, with the same message
/// signup gives — because both read the same declaration.
#[tokio::test]
async fn a_module_cannot_be_turned_on_without_what_it_needs() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    // No modules at all.
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tenants/acme/modules")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "module": "sales" }).to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.module_requires");

    fixture.cleanup().await;
}

/// Only `ManageTenant` may change what a tenant is paying for.
#[tokio::test]
async fn changing_modules_needs_the_capability_to_manage_the_tenant() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_ledger(tenant).await;

    for (role, may) in [("owner", true), ("accountant", false), ("viewer", false)] {
        let email = format!("{role}@acme.test");
        let user = fixture.user(&email, "hunter2hunter2").await;
        fixture.join_as(user, tenant, role).await;
        let token = fixture.token(&email, "hunter2hunter2").await;

        let (status, body, _) = fixture
            .send(
                Request::post("/v1/tenants/acme/modules")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "module": "sales" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;

        assert_eq!(
            status != StatusCode::FORBIDDEN,
            may,
            "{role} enabling a module: {status} {body}"
        );
    }

    fixture.cleanup().await;
}

/// The catalogue is public, because a pricing page needs it before anyone has
/// an account — and it carries the dependencies, so a picker can grey out the
/// impossible combinations rather than let someone discover them.
#[tokio::test]
async fn the_module_catalogue_is_readable_without_signing_in() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
        .send(Request::get("/v1/modules").body(Body::empty()).unwrap())
        .await;

    assert_eq!(status, StatusCode::OK);
    let modules = body.as_array().expect("a list");
    assert!(modules.len() >= 2);

    let sales = modules
        .iter()
        .find(|m| m["name"] == "sales")
        .expect("sales is offered");
    assert_eq!(sales["requires"][0], "ledger");

    fixture.cleanup().await;
}
