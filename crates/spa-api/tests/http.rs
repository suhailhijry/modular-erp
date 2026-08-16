//! The HTTP surface, driven through the real router.
//!
//! No mocked state and no handler called directly: the requests go through
//! `Router::oneshot`, so the extractors, the rejections and the status mapping
//! are all under test. A handler tested by calling it has skipped the part most
//! likely to be wrong.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

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
            db,
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
            .register_login(identity.id, handle.to_owned(), password.to_owned())
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

    /// Sends a request, naming `acme` unless the test named someone else.
    ///
    /// The tenant is the subdomain now. Defaulting it here keeps a hundred
    /// call sites from repeating `acme.localhost`; a test that means a
    /// different tenant sets `Host` itself, and those are precisely the tests
    /// about reaching a tenant you are not a member of.
    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value, Vec<u8>) {
        let mut request = request;
        if !request.headers().contains_key(header::HOST) {
            request.headers_mut().insert(
                header::HOST,
                axum::http::HeaderValue::from_static("acme.localhost"),
            );
        }
        let request = request;

        let method = request.method().as_str().to_lowercase();
        let path = request.uri().path().to_owned();
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
        // Every response in this file is also a contract test. See `contract`.
        contract::check(&method, &path, status, &json);
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

    /// Sales and purchases together, which is what a whole VAT return needs.
    async fn enable_both_sides(&self, tenant: TenantId) {
        self.enable_sales(tenant).await;
        self.enable_module(tenant, purchases::setup()).await;
        self.enable_module(tenant, tax_sa::setup()).await;
    }

    /// The return without the input side, which is most small businesses.
    async fn enable_selling_only(&self, tenant: TenantId) {
        self.enable_sales(tenant).await;
        self.enable_module(tenant, tax_sa::setup()).await;
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

    async fn project_both_sides(&self, tenant: TenantId) {
        self.project_sales(tenant).await;
        self.project(tenant, &purchases::projections(), purchases::upcasters())
            .await;
    }

    /// Registers the tenant with ZATCA over HTTP, which every ZATCA test needs
    /// before it can have a document at all.
    async fn register_with_zatca(&self, token: &str) {
        let (status, body, _) = self
            .send(
                Request::put("/v1/tax_sa/registration")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "vat_number": "310122393500003",
                            "name": "أكمي للتجارة",
                            "scheme": "crn",
                            "identifier": "1010101010",
                            "address": {
                                "street": "طريق الملك فهد",
                                "building": "2322",
                                "district": "العليا",
                                "city": "الرياض",
                                "postal_code": "12211",
                                "country": "SA"
                            },
                            "effective_from": "2026-01-01T00:00:00Z"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The Saudi module's group, which is what builds the ZATCA documents.
    async fn project_tax(&self, tenant: TenantId) {
        self.project_sales(tenant).await;
        self.project(tenant, &tax_sa::projections(), tax_sa::upcasters())
            .await;
    }

    /// Installs a chart of accounts over HTTP.
    async fn install_chart(&self, token: &str, slug: &str, template: &str) {
        let (status, body, _) = self
            .send(
                Request::post("/v1/ledger/chart")
                    .header(header::HOST, format!("{slug}.localhost"))
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

    /// Gives somebody a different role in one module, or (with `None`) puts
    /// them back on their tenant-wide one.
    async fn module_role(
        &self,
        token: &str,
        identity: IdentityId,
        module: &str,
        role: Option<&str>,
    ) -> StatusCode {
        let uri = format!("/v1/members/{identity}/modules/{module}");
        let request = Request::builder()
            .method(if role.is_some() { "PUT" } else { "DELETE" })
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        let body = role.map_or_else(Body::empty, |role| {
            Body::from(serde_json::json!({ "role": role }).to_string())
        });

        let (status, body, _) = self.send(request.body(body).unwrap()).await;
        assert!(
            status.is_success(),
            "setting a module role: {status} {body}"
        );
        status
    }

    /// Issues an invoice as whoever holds `token`, returning only the status —
    /// which is what the authorization tests are asking about.
    async fn try_invoice(&self, token: &str, id: &str) -> StatusCode {
        let (status, _, _) = self
            .send(
                Request::post("/v1/sales/invoices")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "id": id,
                            "customer": { "name": "Rawabi" },
                            "issued_on": "2026-03-01T00:00:00Z",
                            "currency": "SAR",
                            "lines": [
                                { "description": "Work", "net": 10_000, "vat": "standard" }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        status
    }

    /// Opens a ledger account as whoever holds `token`.
    async fn try_open_account(&self, token: &str, code: &str) -> StatusCode {
        let (status, _, _) = self
            .send(
                Request::post("/v1/ledger/accounts")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "code": code, "name": "Test", "kind": "asset", "currency": "SAR"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        status
    }

    /// A ledger account's balance, read over HTTP so the assertion travels the
    /// same path a client would.
    async fn ledger_balance(&self, token: &str, slug: &str, code: &str) -> i64 {
        let (status, accounts, _) = self
            .send(
                Request::get("/v1/ledger/accounts")
                    .header(header::HOST, format!("{slug}.localhost"))
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

    /// Invites somebody and returns the link's token.
    async fn invite(&self, token: &str, slug: &str, handle: &str, role: &str) -> String {
        let (status, body, _) = self
            .send(
                Request::post("/v1/invitations")
                    .header(header::HOST, format!("{slug}.localhost"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "handle": handle, "role": role }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["token"].as_str().expect("a token").to_owned()
    }

    /// Drops every tenant database this test made, however it made it.
    ///
    /// Read from the control database rather than recorded as they are created:
    /// tenants born from `POST /v1/signups` were never on the recorded list, and
    /// the signup tests each tried to drop `spa_tenant_acme` — a name that has
    /// not been right since database names stopped being derived from the slug.
    /// Three databases leaked per green run, and nothing noticed. Asking the
    /// rows cannot drift the way remembering can.
    async fn cleanup(self) {
        let names: Vec<String> = sqlx::query_scalar("SELECT database_name FROM tenant")
            .fetch_all(self.db.pool())
            .await
            .expect("reads the tenants this test made");

        for name in &names {
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
        Request::get("/v1/tenant")
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
    // **The tenant is the host now**, so the same path reads a different
    // company depending only on which subdomain it arrives at. That is the
    // thing this test exists to try.
    let read = |slug: &str| {
        Request::get("/v1/tenant")
            .header(header::HOST, format!("{slug}.localhost"))
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
        let mut request = Request::get("/v1/tenant");
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
async fn a_malformed_body_is_a_400_in_the_shape_every_other_failure_has() {
    let fixture = Fixture::new().await;

    // axum's own `Json` rejection is `text/plain` with no `code`, so a client
    // that always parses `problem+json` broke on the most common mistake there
    // is. `wire::Json` is what makes this the same shape as everything else.
    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "request.malformed_body");
    assert!(
        body["args"]["reason"]["value"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "the parser's account of what it found is what makes this fixable: {body}"
    );

    // Valid JSON of the wrong shape is a different status and the same shape.
    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"handle": "someone@acme.test"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "request.malformed_body");

    // And a body with no content type at all.
    let (status, body, content_type) = fixture
        .send(
            Request::post("/v1/sessions")
                .body(Body::from(r#"{"handle": "a", "password": "b"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "request.unsupported_media_type");

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
                "/v1/ledger/accounts",
                serde_json::json!({
                    "code": code, "name": code, "kind": kind, "currency": "SAR"
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body, _) = fixture
        .send(post(
            "/v1/ledger/entries",
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

    let (status, accounts, _) = fixture.send(read("/v1/ledger/accounts")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accounts[0]["code"], "1000");
    assert_eq!(accounts[0]["balance"], 15000);
    assert_eq!(accounts[1]["balance"], -15000);

    let (status, trial, _) = fixture.send(read("/v1/ledger/trial-balance")).await;
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
            Request::post("/v1/ledger/entries")
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
            Request::post("/v1/ledger/entries")
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
            Request::get("/v1/ledger/trial-balance")
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
            Request::get("/v1/tenant")
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
            Request::get("/v1/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accounts}");
    assert_eq!(accounts.as_array().expect("a list").len(), 0);

    let (status, _, _) = fixture
        .send(
            Request::post("/v1/ledger/accounts")
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
            Request::post("/v1/ledger/chart")
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
            Request::get("/v1/ledger/accounts")
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
            Request::post("/v1/ledger/chart")
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

/// Every operation that is about a tenant, as
/// `(operationId, "METHOD /template", takes a body)`.
///
/// # Why this is not a path prefix any more
///
/// It used to be everything under `/v1/tenants/{slug}`, which was a prefix a
/// route either had or did not. The tenant is the **subdomain** now, so a
/// tenant-scoped path and an apex one look identical — `/v1/members` and
/// `/v1/catalogue` differ in what they need, not in how they read.
///
/// So the property is the real one: an operation is role-scoped unless it is
/// **public** (`security: []`) or one of the handful that need a session and no
/// tenant. That list is written out, because a route that quietly joined it
/// would be a route this matrix stopped checking.
fn role_scoped_operations() -> Vec<(String, String, bool)> {
    /// Authenticated, and about the caller rather than a company.
    const NO_TENANT: &[&str] = &["log_out"];

    let document = serde_json::to_value(spa_api::openapi()).expect("the document serializes");
    let mut found = Vec::new();

    for (path, item) in document["paths"].as_object().expect("there are paths") {
        for (method, operation) in item.as_object().expect("a path item") {
            let Some(id) = operation["operationId"].as_str() else {
                continue;
            };
            let public = operation["security"]
                .as_array()
                .is_some_and(std::vec::Vec::is_empty);
            if public || NO_TENANT.contains(&id) {
                continue;
            }
            found.push((
                id.to_owned(),
                format!("{} {path}", method.to_uppercase()),
                operation["requestBody"].is_object(),
            ));
        }
    }

    found
}

/// `(operationId, the roles that may)`. Everything else is refused.
const PERMISSIONS: &[(&str, &[&str])] = &[
    // Reading is what a viewer is for.
    ("tenant", ALL_ROLES),
    ("list_members", ALL_ROLES),
    ("list_modules", ALL_ROLES),
    ("list_accounts", ALL_ROLES),
    ("trial_balance", ALL_ROLES),
    ("list_invoices", ALL_ROLES),
    ("get_invoice", ALL_ROLES),
    ("posting_accounts", ALL_ROLES),
    ("books", ALL_ROLES),
    ("vat_rates", ALL_ROLES),
    ("vat_return", ALL_ROLES),
    ("filed_returns", ALL_ROLES),
    ("list_bills", ALL_ROLES),
    ("get_bill", ALL_ROLES),
    // Where the business stands with ZATCA, and the documents themselves.
    // Reading, so everyone — a clerk at a till needs to see that the receipt
    // they just handed over has been reported.
    ("registration", ALL_ROLES),
    ("zatca_standing", ALL_ROLES),
    ("zatca_documents", ALL_ROLES),
    ("zatca_document", ALL_ROLES),
    // Recording what happened. A clerk does this and nothing structural.
    ("post_entry", &["owner", "accountant", "clerk"]),
    ("reverse_entry", &["owner", "accountant", "clerk"]),
    ("issue_invoice", &["owner", "accountant", "clerk"]),
    ("record_payment", &["owner", "accountant", "clerk"]),
    ("credit_note", &["owner", "accountant", "clerk"]),
    ("record_bill", &["owner", "accountant", "clerk"]),
    ("pay_bill", &["owner", "accountant", "clerk"]),
    // Changing the shape of the books. Not a clerk's job — they post into
    // the chart, they do not restructure it.
    ("open_account", &["owner", "accountant"]),
    ("install_chart", &["owner", "accountant"]),
    ("set_posting_accounts", &["owner", "accountant"]),
    // Declaring the numbers final is the accountant's call, and not
    // something a clerk posting entries should be able to do to them.
    ("close_books", &["owner", "accountant"]),
    ("set_vat_rates", &["owner", "accountant"]),
    // Filing is a declaration to a tax authority, not a bookkeeping entry.
    ("file_return", &["owner", "accountant"]),
    // Changing the tenant: who has access, and what it pays for. The owner
    // alone, including against an accountant — somebody who keeps the books
    // should not be able to decide who else can see them.
    // The tenant's identity to a tax authority. Every invoice it ever issues
    // is stamped with this, so it sits with the owner beside membership.
    ("register", OWNER),
    ("add_member", OWNER),
    ("change_role", OWNER),
    ("remove_member", OWNER),
    ("set_module_role", OWNER),
    ("clear_module_role", OWNER),
    ("enable_module", OWNER),
    ("disable_module", OWNER),
    ("list_invitations", OWNER),
    ("invite", OWNER),
    ("revoke_invitation", OWNER),
];
const ALL_ROLES: &[&str] = &["owner", "accountant", "clerk", "viewer"];
const OWNER: &[&str] = &["owner"];

/// **The authorization matrix, over HTTP: every role against every endpoint.**
///
/// # Why the list comes from the document
///
/// The previous version of this test carried exactly this heading and checked
/// three ledger routes. It could not have done better: the endpoints were
/// written out here, so the test only ever grew when somebody remembered to
/// grow it, and a route added without that thought is a route nobody checked.
///
/// So the endpoints come from `spa_api::openapi()` — the same value the router
/// is built from. Every operation under `/v1/tenant` is role-scoped by
/// construction, and `PERMISSIONS` must name all of them: an operation missing
/// from the table **fails this test** rather than defaulting to untested.
/// Adding a route now forces the decision instead of allowing it.
///
/// # Why the table is written out
///
/// It is not derived from `Role::allows`. A test that asks the code what it does
/// can only ever agree with it; this one asks whether that is what we meant, and
/// a change in permissions has to be typed here, in a diff somebody reviews.
///
/// # Why a garbage body still answers the question
///
/// `Allowed<C>` is a `FromRequestParts` extractor and the first parameter of
/// all twenty-seven of these handlers, so authorization runs *before* the body
/// is parsed. `{}` gets a 403 when the role is refused and a 400 when it is not,
/// which is exactly the distinction being measured — and it keeps the test from
/// mutating the tenant out from under itself.
#[tokio::test]
async fn every_role_against_every_endpoint() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_sales(tenant).await;

    let endpoints = role_scoped_operations();

    // No operation named twice. A duplicate is harmless today — the lookup
    // takes the first — and it means two rows claiming different permissions
    // for one route would silently disagree, with whichever came first winning.
    let mut named: Vec<&str> = PERMISSIONS.iter().map(|(id, _)| *id).collect();
    named.sort_unstable();
    let unique = {
        let mut seen = named.clone();
        seen.dedup();
        seen
    };
    assert_eq!(named, unique, "an operation appears twice in the table");

    // The table and the router describe the same set, in both directions.
    let served: std::collections::BTreeSet<&str> =
        endpoints.iter().map(|(id, _, _)| id.as_str()).collect();
    let tabled: std::collections::BTreeSet<&str> = PERMISSIONS.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        served,
        tabled,
        "the table and the routes disagree. Served and untabled: {:?}. Tabled and unserved: {:?}.",
        served.difference(&tabled).collect::<Vec<_>>(),
        tabled.difference(&served).collect::<Vec<_>>(),
    );
    assert_eq!(
        served.len(),
        42,
        "expected forty-two role-scoped operations"
    );

    // A member, so `{identity}` names somebody real rather than testing the
    // uuid parser.
    let subject = fixture.user("subject@acme.test", "hunter2hunter2").await;
    fixture.join_as(subject, tenant, "viewer").await;

    for role in ALL_ROLES {
        let email = format!("{role}@acme.test");
        let user = fixture.user(&email, "hunter2hunter2").await;
        fixture.join_as(user, tenant, role).await;
        let token = fixture.token(&email, "hunter2hunter2").await;

        for (id, route, takes_a_body) in &endpoints {
            let (method, template) = route.split_once(' ').expect("method and path");
            // `{module}` is deliberately not a module: `disable_module` takes no
            // body, so a real name would let the owner turn sales off half way
            // through the matrix. An unknown one is refused *after* the
            // capability check, which is the only part being measured.
            let path = template
                .replace("{identity}", &subject.to_string())
                .replace("{module}", "none")
                .replace("{entry}", "JE-1")
                .replace("{invoice}", "INV-1")
                .replace("{invitation}", "01a00000-0000-7000-8000-000000000000");

            let request = Request::builder()
                .method(method)
                .uri(&path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json");
            let body = if *takes_a_body {
                Body::from("{}")
            } else {
                Body::empty()
            };

            let (status, answer, _) = fixture.send(request.body(body).unwrap()).await;

            let may = PERMISSIONS
                .iter()
                .find(|(operation, _)| operation == id)
                .map(|(_, roles)| roles.contains(role))
                .expect("every operation is in the table; checked above");

            assert_eq!(
                status != StatusCode::FORBIDDEN,
                may,
                "{role} → {method} {path} ({id}) answered {status}, and the table says \
                 {}. Body: {answer}",
                if may { "allowed" } else { "refused" }
            );

            // A refusal says which capability, so the person reading it knows
            // what to ask for rather than just that they cannot.
            if !may {
                assert_eq!(answer["code"], "access.not_permitted", "{role} → {id}");
                assert!(
                    answer["args"]["capability"]["value"].is_string(),
                    "{role} → {id}: the 403 does not name the capability: {answer}"
                );
            }
        }
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
            Request::post("/v1/ledger/accounts")
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
            Request::get("/v1/tenant")
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
            Request::get("/v1/tenant")
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
            Request::post("/v1/members")
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
            Request::get("/v1/tenant")
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
            Request::post("/v1/ledger/accounts")
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
            Request::get("/v1/members")
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
            Request::post("/v1/members")
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
        Request::post("/v1/ledger/accounts")
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
            Request::patch(format!("/v1/members/{identity}"))
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
            Request::patch(format!("/v1/members/{owner}"))
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
            Request::delete(format!("/v1/members/{owner}"))
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
            Request::post("/v1/members")
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
            Request::patch(format!("/v1/members/{owner}"))
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
            Request::post("/v1/members")
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
            Request::post("/v1/members")
                // The *other* tenant: they are already an accountant at acme,
                // and this is the second company adding the same person.
                .header(header::HOST, "globex.localhost")
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
            Request::post("/v1/members")
                .header(header::HOST, "globex.localhost")
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
                Request::post("/v1/ledger/accounts")
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
            Request::post("/v1/ledger/entries")
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
            Request::get(format!("/v1/ledger/accounts?consistent_after={position}"))
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
            Request::get(format!("/v1/ledger/accounts?consistent_after={position}"))
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
            Request::post("/v1/ledger/accounts")
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
            Request::get("/v1/ledger/accounts")
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
            Request::post("/v1/ledger/accounts")
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
            bearer(Request::post("/v1/sales/invoices"))
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
    // The client chose the *key*; the statutory number is ours, and Saudi law
    // requires the series it comes from to have no holes in it.
    assert_eq!(issued["id"], "INV-1", "the key comes back as sent");
    assert_eq!(issued["number"], "INV-00001", "{issued}");
    let position = issued["position"].as_i64().expect("a log position");

    // Read your own write. Without the worker running, this is what the hint is
    // for — so drive the projections first and then ask for exactly that point.
    fixture.project_sales(tenant).await;

    let (status, invoice, _) = fixture
        .send(
            bearer(Request::get(format!(
                "/v1/sales/invoices/INV-1?consistent_after={position}"
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
            bearer(Request::post("/v1/sales/invoices/INV-1/payments"))
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
            bearer(Request::get("/v1/sales/invoices"))
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
            Request::get("/v1/sales/invoices")
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
            Request::get("/v1/ledger/accounts")
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
            Request::post("/v1/sales/invoices")
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
            Request::post("/v1/sales/invoices")
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
            bearer(Request::get("/v1/sales/invoices"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/modules"))
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
            bearer(Request::post("/v1/sales/invoices"))
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
            bearer(Request::get("/v1/sales/invoices"))
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
            bearer(Request::post("/v1/sales/invoices"))
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
            bearer(Request::delete("/v1/modules/sales"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, _, _) = fixture
        .send(
            bearer(Request::get("/v1/sales/invoices"))
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
            bearer(Request::post("/v1/modules"))
                .body(Body::from(
                    serde_json::json!({ "module": "sales" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, invoices, _) = fixture
        .send(
            bearer(Request::get("/v1/sales/invoices"))
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
            Request::delete("/v1/modules/ledger")
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
            Request::delete("/v1/modules/sales")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = fixture
        .send(
            Request::delete("/v1/modules/ledger")
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
            Request::post("/v1/modules")
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
                Request::post("/v1/modules")
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
        .send(Request::get("/v1/catalogue").body(Body::empty()).unwrap())
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

// ---------------------------------------------------------------------------
// Invitations
// ---------------------------------------------------------------------------

/// **The requirement, end to end.** A colleague gets access without the owner
/// ever choosing — or knowing — their password.
#[tokio::test]
async fn an_invited_colleague_sets_their_own_password_and_gets_to_work() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, invitation, _) = fixture
        .send(
            Request::post("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "handle": "Sara@Acme.test", "role": "accountant" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{invitation}");
    assert_eq!(
        invitation["handle"], "sara@acme.test",
        "the address is normalised, so the login they end up with is predictable"
    );
    let link = invitation["token"].as_str().expect("a token").to_owned();

    // What Sara sees before accepting: what she is joining, and as what.
    let (status, pending, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pending}");
    assert_eq!(pending["slug"], "acme");
    assert_eq!(pending["role"], "accountant");
    assert_eq!(pending["has_account"], false, "she is new here");

    let (status, accepted, _) = fixture
        .send(
            Request::post(format!("/v1/join/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "password": "sara's own password" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{accepted}");
    let sara = accepted["token"].as_str().expect("a session token");

    // Accepting signed her in, and the role took effect: an accountant may
    // manage accounts.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {sara}"))
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
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // ...but not the tenant itself, because she was invited as an accountant.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {sara}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "handle": "x@acme.test", "role": "owner" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And she can log in again later with the password she chose — which is the
    // whole point, and which the owner never saw.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "sara@acme.test", "password": "sara's own password"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    fixture.cleanup().await;
}

/// An invitation is single use, and a spent link is indistinguishable from one
/// that never existed.
#[tokio::test]
async fn an_invitation_works_once_and_then_says_nothing() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let link = fixture
        .invite(&token, "acme", "sara@acme.test", "clerk")
        .await;

    let accept = |link: String, password: &'static str| {
        Request::post(format!("/v1/join/{link}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "password": password }).to_string(),
            ))
            .unwrap()
    };

    let (status, _, _) = fixture.send(accept(link.clone(), "sara's password")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body, _) = fixture.send(accept(link.clone(), "sara's password")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "spent: {body}");
    assert_eq!(body["code"], "invitations.not_valid");

    // Byte-identical to a link that was never issued.
    let (fake, _, _) = fixture
        .send(
            Request::get(
                "/v1/join/0000000000000000000000000000000000000000000000000000000000000000",
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    let (spent, _, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        fake, spent,
        "a spent link tells you nothing a fake one does not"
    );

    fixture.cleanup().await;
}

/// **The guard that matters.** A link cannot become somebody else's account: an
/// address that already has one must prove it with its password.
#[tokio::test]
async fn accepting_for_an_existing_account_needs_that_accounts_password() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    // Sara already works somewhere else on this platform.
    fixture.user("sara@acme.test", "sara's real password").await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let link = fixture
        .invite(&token, "acme", "sara@acme.test", "clerk")
        .await;

    let (status, pending, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(pending["has_account"], true, "{status}");

    // Somebody who got hold of the link, guessing.
    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/join/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "password": "not sara's password" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "auth.invalid_credentials");

    // And the invitation is not burnt by the attempt — a typo must not turn
    // into a support ticket.
    let (status, _, _) = fixture
        .send(
            Request::post(format!("/v1/join/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "password": "sara's real password" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "the real Sara still gets in");

    fixture.cleanup().await;
}

/// Revoking actually revokes: the link stops working, and re-inviting does not
/// leave the old one alive alongside the new.
#[tokio::test]
async fn revoking_and_re_inviting_leave_exactly_one_live_link() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let first = fixture
        .invite(&token, "acme", "sara@acme.test", "clerk")
        .await;
    let second = fixture
        .invite(&token, "acme", "sara@acme.test", "accountant")
        .await;
    assert_ne!(first, second);

    let live = |link: &str| {
        Request::get(format!("/v1/join/{link}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, _, _) = fixture.send(live(&first)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "re-inviting replaces rather than accumulates"
    );
    let (status, pending, _) = fixture.send(live(&second)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending["role"], "accountant");

    // Only one outstanding, and revoking it leaves none.
    let (_, list, _) = fixture
        .send(
            Request::get("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let list = list.as_array().expect("a list");
    assert_eq!(list.len(), 1);

    let id = list[0]["id"].as_str().expect("an id");
    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/invitations/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = fixture.send(live(&second)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a revoked link is dead");

    fixture.cleanup().await;
}

/// One tenant's invitation id cannot be used to revoke another tenant's.
#[tokio::test]
async fn an_invitation_cannot_be_revoked_from_another_tenant() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    fixture.join(owner, acme).await;
    fixture.join(owner, globex).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let link = fixture
        .invite(&token, "acme", "sara@acme.test", "clerk")
        .await;
    let (_, list, _) = fixture
        .send(
            Request::get("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let id = list[0]["id"].as_str().expect("an id").to_owned();

    // Same owner, same id, **wrong host**. The invitation belongs to acme and
    // this asks globex to revoke it — which is the shape of every "I have a
    // valid id from somewhere else" attempt.
    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/invitations/{id}"))
                .header(header::HOST, "globex.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "revoking is idempotent");

    let (status, _, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "but it did not touch acme's invitation"
    );

    fixture.cleanup().await;
}

/// Somebody who is already in does not need an invitation.
#[tokio::test]
async fn inviting_an_existing_member_is_refused() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({ "handle": "owner@acme.test", "role": "viewer" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "members.already_a_member");

    fixture.cleanup().await;
}

/// A password chosen through an invitation gets the same rule as one chosen at
/// signup, and the rule is applied before the token is looked at.
#[tokio::test]
async fn an_invitation_will_not_accept_a_short_password() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let link = fixture
        .invite(&token, "acme", "sara@acme.test", "clerk")
        .await;

    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/join/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "password": "short" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.password_too_short");

    // The invitation survives it.
    let (status, _, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    fixture.cleanup().await;
}

/// **Regression: unauthenticated account takeover.**
///
/// `set_password` upserted on `(kind, handle)`, so signing up with somebody
/// else's address overwrote their password while leaving the row pointing at
/// their identity. The attacker then logged in as them — as an owner of their
/// tenant — and the victim could not log in at all. From a public endpoint,
/// with no credential.
///
/// The fix is that registering a login and changing one are different
/// operations: `register_login` refuses a taken handle, and signing up with an
/// address that already has an account must prove it with that account's
/// password.
#[tokio::test]
async fn signing_up_with_someone_elses_address_cannot_take_their_account() {
    let mut fixture = Fixture::new().await;
    let victim = fixture
        .user("victim@acme.test", "the victim's password")
        .await;
    let tenant = fixture.provision("acme").await;
    fixture.join(victim, tenant).await;

    let signup = |password: &'static str| {
        Request::post("/v1/signups")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "slug": "attacker",
                    "company": "Attacker",
                    "email": "victim@acme.test",
                    "password": password,
                    "modules": []
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture.send(signup("chosen by the attacker")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "auth.invalid_credentials");

    // The attacker's chosen password is not a way in.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "victim@acme.test", "password": "chosen by the attacker"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And the victim's own password still is.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "victim@acme.test", "password": "the victim's password"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the victim never lost anything"
    );

    fixture.cleanup().await;
}

/// The same address signing up for a second company is a real thing people do,
/// and it works — by logging in on the way through.
#[tokio::test]
async fn signing_up_again_with_your_own_address_gives_you_a_second_tenant() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let first = fixture.provision("acme").await;
    fixture.join(owner, first).await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "second",
                        "company": "Second Company",
                        "email": "owner@acme.test",
                        "password": "hunter2hunter2",
                        "modules": []
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // One account, two tenants — the session it returns reaches both.
    let token = body["token"].as_str().expect("a token");
    for slug in ["acme", "second"] {
        let (status, _, _) = fixture
            .send(
                Request::get("/v1/tenant")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{slug}");
    }

    fixture.cleanup().await;
}

/// **Regression: removing a member made them permanently un-addable.**
///
/// The unique constraint on `(identity_id, tenant_id)` covers revoked rows, and
/// `grant_membership` was a plain `INSERT` — so an employee who left and came
/// back, or anyone removed by mistake, hit a 500 that named nothing. Granting
/// now revives a revoked membership, and only a revoked one.
#[tokio::test]
async fn somebody_removed_can_be_added_again() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let add = |role: &'static str| {
        Request::post("/v1/members")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "email": "sara@acme.test", "password": "sara's own password", "role": role
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture.send(add("clerk")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let identity = body["identity"].as_str().expect("an identity").to_owned();

    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/members/{identity}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Back, with a different role, and as the same person rather than a second
    // account for the same address.
    let (status, body, _) = fixture.send(add("accountant")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["identity"], identity, "the same account, revived");

    let (_, members, _) = fixture
        .send(
            Request::get("/v1/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let sara = members
        .as_array()
        .expect("a list")
        .iter()
        .find(|m| m["identity"] == identity)
        .expect("is back");
    assert_eq!(
        sara["role"], "accountant",
        "with the role they were re-added as"
    );

    fixture.cleanup().await;
}

/// Managing a member of another tenant does nothing to them — and now says so
/// rather than answering `204` to a request that changed nothing.
#[tokio::test]
async fn managing_somebody_who_is_not_a_member_here_is_a_404() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let stranger = fixture.user("other@globex.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    fixture.join(owner, acme).await;
    fixture.join(stranger, globex).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::patch(format!("/v1/members/{stranger}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "role": "owner" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "members.not_a_member");

    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/members/{stranger}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And the isolation held throughout: their membership elsewhere is exactly
    // as it was. This is the assertion that would have caught a leak, and it
    // passed even before the status was corrected.
    let members = fixture.control.members(globex).await.expect("reads");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].role, spa_control::Role::Owner);

    fixture.cleanup().await;
}

/// **Configuration, over HTTP.** A tenant chooses where sales post, and the
/// next invoice goes there.
#[tokio::test]
async fn a_tenant_can_configure_where_sales_post() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // Untouched: the shipped defaults, and honest about being defaults.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/sales/posting-accounts"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["receivable"], "1100");
    assert_eq!(body["configured"], false);

    // An account this tenant does not have is refused here rather than by every
    // future invoice.
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/sales/posting-accounts"))
                .body(Body::from(
                    serde_json::json!({
                        "receivable": "9999", "revenue": "4000", "output_vat": "2100"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "ledger.no_such_account");

    // The services chart has 4900 "Discounts given" — a real account to move to.
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/sales/posting-accounts"))
                .body(Body::from(
                    serde_json::json!({
                        "receivable": "1100", "revenue": "4900", "output_vat": "2100"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (_, body, _) = fixture
        .send(
            bearer(Request::get("/v1/sales/posting-accounts"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(body["revenue"], "4900");
    assert_eq!(body["configured"], true);

    // And the next invoice goes there.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-CONFIGURED",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-03-01T00:00:00Z",
                        "currency": "SAR",
                        "lines": [{ "description": "Work", "net": 100_000, "vat": "zero" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    fixture.project_sales(tenant).await;

    assert_eq!(
        fixture.ledger_balance(&token, "acme", "4900").await,
        -100_000,
        "revenue landed where the tenant said"
    );
    assert_eq!(
        fixture.ledger_balance(&token, "acme", "4000").await,
        0,
        "and not where it ships"
    );

    fixture.cleanup().await;
}

/// Choosing the accounts is a chart decision, so it needs the capability that
/// maintains the chart.
#[tokio::test]
async fn configuring_posting_accounts_needs_manage_accounts() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_sales(tenant).await;

    for (role, may) in [("accountant", true), ("clerk", false), ("viewer", false)] {
        let email = format!("{role}@acme.test");
        let user = fixture.user(&email, "hunter2hunter2").await;
        fixture.join_as(user, tenant, role).await;
        let token = fixture.token(&email, "hunter2hunter2").await;

        let (status, body, _) = fixture
            .send(
                Request::put("/v1/sales/posting-accounts")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "receivable": "1100", "revenue": "4000", "output_vat": "2100"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;

        assert_eq!(
            status != StatusCode::FORBIDDEN,
            may,
            "{role} setting posting accounts: {status} {body}"
        );
    }

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Per-module roles
// ---------------------------------------------------------------------------

/// **The arrangement two modules made possible.** Sara does the invoicing,
/// Khalid does the books, and neither can do the other's job.
#[tokio::test]
async fn one_person_can_have_a_different_role_in_a_different_module() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_sales(tenant).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture
        .install_chart(&owner_token, "acme", "services")
        .await;

    // Sara: a viewer everywhere, an accountant in sales.
    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, tenant, "viewer").await;
    fixture
        .module_role(&owner_token, sara, "sales", Some("accountant"))
        .await;
    let sara_token = fixture.token("sara@acme.test", "hunter2hunter2").await;

    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-SARA").await,
        StatusCode::CREATED,
        "invoicing is her job"
    );
    assert_eq!(
        fixture.try_open_account(&sara_token, "1234").await,
        StatusCode::FORBIDDEN,
        "the books are not"
    );

    // And the tenant itself is nobody's module: being an accountant in sales
    // does not make her able to decide who else has access.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/invitations")
                .header(header::AUTHORIZATION, format!("Bearer {sara_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "handle": "x@acme.test", "role": "owner" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Khalid: an accountant everywhere, a viewer in sales. The other direction,
    // and the one easier to get wrong.
    let khalid = fixture.user("khalid@acme.test", "hunter2hunter2").await;
    fixture.join_as(khalid, tenant, "accountant").await;
    fixture
        .module_role(&owner_token, khalid, "sales", Some("viewer"))
        .await;
    let khalid_token = fixture.token("khalid@acme.test", "hunter2hunter2").await;

    assert_eq!(
        fixture.try_open_account(&khalid_token, "1234").await,
        StatusCode::CREATED,
        "the books are his job"
    );
    assert_eq!(
        fixture.try_invoice(&khalid_token, "INV-KHALID").await,
        StatusCode::FORBIDDEN,
        "invoicing is not"
    );

    fixture.cleanup().await;
}

/// Clearing an override is not the same as setting `viewer`: it puts somebody
/// back on their tenant-wide role, so a later promotion reaches that module too.
#[tokio::test]
async fn clearing_a_module_role_restores_the_tenant_wide_one() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_sales(tenant).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture
        .install_chart(&owner_token, "acme", "services")
        .await;

    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, tenant, "accountant").await;
    let sara_token = fixture.token("sara@acme.test", "hunter2hunter2").await;

    fixture
        .module_role(&owner_token, sara, "sales", Some("viewer"))
        .await;
    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-1").await,
        StatusCode::FORBIDDEN
    );

    fixture.module_role(&owner_token, sara, "sales", None).await;
    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-2").await,
        StatusCode::CREATED,
        "her accountant role reaches sales again"
    );

    // And the members list stops mentioning it.
    let (_, members, _) = fixture
        .send(
            Request::get("/v1/members")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let entry = members
        .as_array()
        .expect("a list")
        .iter()
        .find(|m| m["identity"] == sara.to_string())
        .expect("is a member");
    assert_eq!(entry["role"], "accountant");
    assert!(
        entry["module_roles"].as_array().expect("a list").is_empty(),
        "{entry}"
    );

    fixture.cleanup().await;
}

/// Removing somebody takes away everything about their access, exceptions
/// included — so re-adding them later starts from their new role rather than a
/// rule nobody remembers setting.
#[tokio::test]
async fn removing_somebody_clears_their_module_exceptions() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, tenant, "owner").await;
    fixture
        .module_role(&token, sara, "sales", Some("viewer"))
        .await;

    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/members/{sara}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Back as an accountant, with no lingering exception.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/members")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "sara@acme.test",
                        "password": "hunter2hunter2",
                        "role": "accountant"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let sara_token = fixture.token("sara@acme.test", "hunter2hunter2").await;
    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-BACK").await,
        StatusCode::CREATED,
        "no ghost exception"
    );

    fixture.cleanup().await;
}

/// A demotion in one module takes effect at once, not after the cache expires.
#[tokio::test]
async fn a_module_demotion_takes_effect_immediately() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, tenant, "accountant").await;
    let sara_token = fixture.token("sara@acme.test", "hunter2hunter2").await;

    // Warms the membership cache with her accountant role.
    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-BEFORE").await,
        StatusCode::CREATED
    );

    fixture
        .module_role(&token, sara, "sales", Some("viewer"))
        .await;

    assert_eq!(
        fixture.try_invoice(&sara_token, "INV-AFTER").await,
        StatusCode::FORBIDDEN,
        "seconds of doing what you were just told you cannot is not acceptable"
    );

    fixture.cleanup().await;
}

/// **A mistake can be corrected, over HTTP.** A `POST`, not a `DELETE`: the
/// books end up showing both the entry and its correction.
#[tokio::test]
async fn an_entry_posted_in_error_can_be_reversed_over_http() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/ledger/entries"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "E-OOPS",
                        "occurred_on": "2026-03-01T00:00:00Z",
                        "memo": "wrong amount",
                        "lines": [
                            { "account": "1000", "amount": { "minor": 50_000, "currency": "SAR" } },
                            { "account": "4000", "amount": { "minor": -50_000, "currency": "SAR" } }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    fixture.project_ledger(tenant).await;
    assert_eq!(fixture.ledger_balance(&token, "acme", "1000").await, 50_000);

    let reverse = |id: &'static str| {
        bearer(Request::post("/v1/ledger/entries/E-OOPS/reversal"))
            .body(Body::from(
                serde_json::json!({
                    "id": id,
                    "occurred_on": "2026-03-05T00:00:00Z",
                    "memo": "correcting E-OOPS"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture.send(reverse("E-OOPS-R")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    fixture.project_ledger(tenant).await;
    assert_eq!(
        fixture.ledger_balance(&token, "acme", "1000").await,
        0,
        "undone"
    );

    // A second, different reversal is refused rather than swinging the balance
    // the other way.
    let (status, body, _) = fixture.send(reverse("E-OOPS-R2")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ledger.already_reversed");
    assert_eq!(body["args"]["by"]["value"], "E-OOPS-R");

    // Reversing something that was never posted is about the tenant's state,
    // not the request's shape.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/ledger/entries/NOPE/reversal"))
                .body(Body::from(
                    serde_json::json!({ "id": "NOPE-R", "occurred_on": "2026-03-05T00:00:00Z" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "ledger.no_such_entry");

    fixture.cleanup().await;
}

/// **An invoice issued in error can be credited, over HTTP** — and the module
/// refresh that made the read model able to say so is what got it there.
#[tokio::test]
async fn an_invoice_can_be_credited_and_stops_being_owed() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    assert_eq!(
        fixture.try_invoice(&token, "INV-OOPS").await,
        StatusCode::CREATED
    );
    fixture.project_sales(tenant).await;
    assert_eq!(fixture.ledger_balance(&token, "acme", "1100").await, 11_500);

    let credit = |id: &'static str| {
        Request::post("/v1/sales/invoices/INV-OOPS/credit-note")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "id": id, "reason": "wrong customer", "on": "2026-03-05T00:00:00Z"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture.send(credit("CN-1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // `CN-1` was the client's key; the credit note's statutory number is ours,
    // and it comes from a series of its own.
    assert_eq!(body["number"], "CN-00001", "{body}");

    fixture.project_sales(tenant).await;
    assert_eq!(
        fixture.ledger_balance(&token, "acme", "1100").await,
        0,
        "the receivable is reversed"
    );

    let (_, invoices, _) = fixture
        .send(
            Request::get("/v1/sales/invoices")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let invoice = &invoices.as_array().expect("a list")[0];
    assert_eq!(invoice["gross"], 11_500, "the document is still there");
    assert_eq!(
        invoice["outstanding"], 0,
        "but nobody owes it, so nobody chases it"
    );
    assert_eq!(invoice["credit_note"], "CN-00001");

    // A second, different credit note is refused.
    let (status, body, _) = fixture.send(credit("CN-2")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "sales.already_cancelled");

    fixture.cleanup().await;
}

/// **The VAT return, over HTTP** — what a Saudi business files, by rate.
#[tokio::test]
async fn a_tenant_can_read_the_vat_it_has_charged() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "INV-VAT",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-02-10T00:00:00Z",
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
    assert_eq!(status, StatusCode::CREATED, "{body}");
    fixture.project_sales(tenant).await;

    let period = "from=2026-01-01T00:00:00Z&until=2026-04-01T00:00:00Z&currency=SAR";
    let (status, filed, _) = fixture
        .send(
            bearer(Request::get(format!("/v1/tax_sa/vat-return?{period}")))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{filed}");
    assert_eq!(
        filed["output"]["tax"], 15_000,
        "15% of the standard-rated 1,000 only"
    );
    assert_eq!(filed["output"]["net"], 150_000);
    assert_eq!(
        filed["output"]["bands"].as_array().expect("bands").len(),
        2,
        "standard and zero-rated are reported apart"
    );
    assert_eq!(
        filed["payable"], 15_000,
        "nothing was bought, so the whole of it is payable"
    );

    // A period that ends before it starts is a mistake worth naming rather than
    // an empty return.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get(
                "/v1/tax_sa/vat-return\
                 ?from=2026-04-01T00:00:00Z&until=2026-01-01T00:00:00Z&currency=SAR",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.empty_period");

    // The query string is the other place a parser answers before any handler
    // does, and axum's own rejection there is `text/plain` with no code. A
    // missing `from` is the same shape as every other failure.
    let (status, body, content_type) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/vat-return?currency=SAR"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(content_type, b"application/problem+json");
    assert_eq!(body["code"], "request.invalid_query");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------

/// **Every response in this file, checked against the published document.**
///
/// # Why this exists
///
/// `utoipa-axum` makes the *paths* structural — a route is registered from its
/// `#[utoipa::path]` attribute, so the document cannot miss one. It does not
/// make the **responses** structural: `(status = OK, body = AccountView)` is
/// hand-written, and nothing in the compiler notices when a handler starts
/// answering with something else, or with a status nobody documented.
///
/// So every response the tests above receive is validated against the schema the
/// document publishes for that path, method and status. Three thousand lines of
/// existing coverage become contract coverage for the cost of one call in
/// [`Fixture::send`], and the failure mode this catches — a document that is
/// believed and wrong — is the expensive one.
///
/// ponytail: a hand-written subset of JSON Schema rather than the `jsonschema`
/// crate — `$ref`, `allOf`, `oneOf`/`anyOf`, `required`, `properties`,
/// `additionalProperties`, `items`, and `type` (including `["T", "null"]`), which
/// is everything utoipa emits here. `every_schema_keyword_is_understood` fails
/// when that stops being true, and the upgrade path is one dev-dependency.
mod contract {
    use std::collections::BTreeSet;
    use std::sync::LazyLock;

    use axum::http::StatusCode;
    use serde_json::Value;

    static DOCUMENT: LazyLock<Value> = LazyLock::new(|| {
        serde_json::to_value(spa_api::openapi()).expect("the document serializes")
    });

    /// Fails when a response does not match what the document promises.
    pub(super) fn check(method: &str, path: &str, status: StatusCode, body: &Value) {
        let doc = &*DOCUMENT;
        let Some(template) = template_for(doc, path) else {
            // Not a route this API serves — a test proving a 404, or one of
            // axum's own rejections. Nothing to check it against.
            return;
        };

        let operation = &doc["paths"][&template][method];
        if operation.is_null() {
            assert!(
                status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
                "{method} {template} answered {status} and is not in the document"
            );
            return;
        }

        let responses = &operation["responses"];
        let response = &responses[status.as_str()];
        assert!(
            !response.is_null(),
            "{method} {template} answered {status}, which the document does not \
             declare. It declares {:?}.",
            responses
                .as_object()
                .map(|r| r.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );

        let Some(schema) = response["content"]["application/json"]["schema"]
            .as_object()
            .or_else(|| response["content"]["application/problem+json"]["schema"].as_object())
        else {
            // Content declared with no schema is a body nothing can describe —
            // `/v1/openapi.json` answers with an arbitrary OpenAPI document.
            // No content at all is a promise the body carries nothing.
            if response["content"].is_object() {
                return;
            }
            assert!(
                body.is_null(),
                "{method} {template} → {status} declares no body and sent {body}"
            );
            return;
        };

        let schema = Value::Object(schema.clone());
        if let Err(why) = validate(doc, &schema, body, "$", Closed::Yes) {
            panic!(
                "{method} {template} → {status} does not match the document: {why}\nbody: {body}"
            );
        }
    }

    /// The templated path this concrete one was served by.
    ///
    /// Prefers the candidate with the most literal segments, so
    /// `/v1/sessions/current` is not read as `/v1/tenant`-shaped noise.
    fn template_for(doc: &Value, path: &str) -> Option<String> {
        let actual: Vec<&str> = path.split('/').collect();
        let mut best: Option<(usize, String)> = None;

        for template in doc["paths"].as_object()?.keys() {
            let parts: Vec<&str> = template.split('/').collect();
            if parts.len() != actual.len() {
                continue;
            }
            let mut literals = 0;
            let matches = parts.iter().zip(&actual).all(|(want, got)| {
                if want.starts_with('{') && want.ends_with('}') {
                    !got.is_empty()
                } else {
                    literals += 1;
                    want == got
                }
            });
            if matches && best.as_ref().is_none_or(|(score, _)| literals > *score) {
                best = Some((literals, template.clone()));
            }
        }
        best.map(|(_, template)| template)
    }

    fn resolve<'a>(doc: &'a Value, schema: &'a Value) -> &'a Value {
        match schema["$ref"].as_str() {
            Some(reference) => {
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or_else(|| panic!("{reference} is not a local reference"));
                let target = &doc["components"]["schemas"][name];
                assert!(!target.is_null(), "{reference} does not resolve");
                target
            }
            None => schema,
        }
    }

    /// Whether a stray property is an error at this level.
    ///
    /// `No` inside an `allOf` branch, where the fields it does not declare
    /// belong to a sibling — the union is checked once, at the `allOf`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Closed {
        Yes,
        No,
    }

    fn validate(
        doc: &Value,
        schema: &Value,
        value: &Value,
        at: &str,
        closed: Closed,
    ) -> Result<(), String> {
        let schema = resolve(doc, schema);

        if let Some(branches) = schema["oneOf"].as_array().or(schema["anyOf"].as_array()) {
            return branches
                .iter()
                .any(|branch| validate(doc, branch, value, at, closed).is_ok())
                .then_some(())
                .ok_or_else(|| {
                    format!("{at} matches none of the {} alternatives", branches.len())
                });
        }

        if let Some(branches) = schema["allOf"].as_array() {
            for branch in branches {
                validate(doc, branch, value, at, Closed::No)?;
            }
            if closed == Closed::No {
                return Ok(());
            }
            let mut declared = BTreeSet::new();
            for branch in branches {
                declared.extend(properties_of(doc, branch));
            }
            return no_strays(value, &declared, at);
        }

        match type_of(schema) {
            Some(types) if !types.iter().any(|t| holds(t, value)) => {
                return Err(format!(
                    "{at} is {} and the document says {types:?}",
                    kind(value)
                ));
            }
            _ => {}
        }
        if value.is_null() {
            return Ok(());
        }

        if let Some(object) = value.as_object() {
            for required in schema["required"].as_array().unwrap_or(&Vec::new()) {
                let name = required.as_str().unwrap_or_default();
                if !object.contains_key(name) {
                    return Err(format!("{at}.{name} is required and absent"));
                }
            }
            for (name, child) in object {
                let declared = &schema["properties"][name];
                if declared.is_null() {
                    // A free-form map (`additionalProperties: <schema>`) says
                    // what its *values* look like, not their names.
                    let extra = &schema["additionalProperties"];
                    if extra.is_object() {
                        validate(doc, extra, child, &format!("{at}.{name}"), Closed::Yes)?;
                    }
                    continue;
                }
                validate(doc, declared, child, &format!("{at}.{name}"), Closed::Yes)?;
            }
            if closed == Closed::Yes
                && schema["properties"].is_object()
                && !schema["additionalProperties"].is_object()
            {
                no_strays(value, &properties_of(doc, schema), at)?;
            }
        }

        if let Some(items) = value.as_array()
            && schema["items"].is_object()
        {
            for (index, item) in items.iter().enumerate() {
                validate(
                    doc,
                    &schema["items"],
                    item,
                    &format!("{at}[{index}]"),
                    Closed::Yes,
                )?;
            }
        }

        // `minimum` is what an unsigned Rust integer publishes. Cheap to honour,
        // and a negative count is a defect worth catching.
        if let Some(minimum) = schema["minimum"].as_i64()
            && let Some(number) = value.as_i64()
            && number < minimum
        {
            return Err(format!(
                "{at} is {number} and the document says at least {minimum}"
            ));
        }

        Ok(())
    }

    /// Every property name a schema declares, following `$ref` and `allOf`.
    fn properties_of(doc: &Value, schema: &Value) -> BTreeSet<String> {
        let schema = resolve(doc, schema);
        let mut names: BTreeSet<String> = schema["properties"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        for branch in schema["allOf"].as_array().unwrap_or(&Vec::new()) {
            names.extend(properties_of(doc, branch));
        }
        names
    }

    /// A field the server sends and the document does not mention is the drift
    /// that costs an integrator most: it looks like their client is wrong.
    fn no_strays(value: &Value, declared: &BTreeSet<String>, at: &str) -> Result<(), String> {
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        for name in object.keys() {
            if !declared.contains(name) {
                return Err(format!("{at}.{name} is sent and undocumented"));
            }
        }
        Ok(())
    }

    fn type_of(schema: &Value) -> Option<Vec<&str>> {
        match &schema["type"] {
            Value::String(one) => Some(vec![one.as_str()]),
            Value::Array(many) => Some(many.iter().filter_map(Value::as_str).collect()),
            _ => None,
        }
    }

    fn holds(declared: &str, value: &Value) -> bool {
        match declared {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            other => panic!("unknown schema type {other:?}"),
        }
    }

    fn kind(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// **The guard on the lookup.**
    ///
    /// [`check`] returns without checking anything when no template matches the
    /// path — right for a test proving a 404, and a silent skip of *everything*
    /// if the matcher ever breaks. `the_validator_is_not_vacuous` would not
    /// notice: it calls `validate` directly.
    ///
    /// So every shape of path this API serves is pinned to the template that
    /// serves it, and two that nothing serves are pinned to `None`.
    #[test]
    fn every_shape_of_path_finds_its_template() {
        let doc = &*DOCUMENT;
        let cases = [
            ("/v1/health", Some("/v1/health")),
            ("/v1/catalogue", Some("/v1/catalogue")),
            ("/v1/sessions", Some("/v1/sessions")),
            ("/v1/sessions/current", Some("/v1/sessions/current")),
            ("/v1/join/abc123", Some("/v1/join/{token}")),
            ("/v1/tenant", Some("/v1/tenant")),
            ("/v1/members", Some("/v1/members")),
            (
                "/v1/members/01a00000-0000-7000-8000-000000000000/modules/sales",
                Some("/v1/members/{identity}/modules/{module}"),
            ),
            (
                "/v1/ledger/entries/JE-1/reversal",
                Some("/v1/ledger/entries/{entry}/reversal"),
            ),
            (
                "/v1/sales/invoices/INV-1",
                Some("/v1/sales/invoices/{invoice}"),
            ),
            (
                "/v1/sales/invoices/INV-1/credit-note",
                Some("/v1/sales/invoices/{invoice}/credit-note"),
            ),
            (
                "/v1/sales/posting-accounts",
                Some("/v1/sales/posting-accounts"),
            ),
            ("/v1/tax_sa/vat-return", Some("/v1/tax_sa/vat-return")),
            // Nothing serves these. Resolving them would swallow the 404 tests
            // that prove a route does not exist for a tenant.
            ("/v1/nonsense", None),
            ("/v1/nonsense/deeper", None),
        ];

        for (concrete, expected) in cases {
            assert_eq!(
                template_for(doc, concrete).as_deref(),
                expected,
                "{concrete} resolved to the wrong operation"
            );
        }
    }

    /// **The guard on the guard.**
    ///
    /// This validator understands a subset of JSON Schema, and a subset is only
    /// safe while it is a superset of what is emitted. A keyword nobody here
    /// implements is a constraint silently not checked — which is how a
    /// hand-rolled validator becomes a test that passes because it looks at
    /// nothing.
    #[test]
    fn every_schema_keyword_is_understood() {
        const UNDERSTOOD: &[&str] = &[
            "$ref",
            "additionalProperties",
            "allOf",
            "anyOf",
            "items",
            "oneOf",
            "properties",
            "required",
            "type",
            // Documentation, not constraints.
            "default",
            "deprecated",
            "description",
            "example",
            "examples",
            "format",
            "propertyNames",
            "title",
            // Constrains a value this validator does not check on its own, but
            // `oneOf` discrimination does — see `MessageArg`.
            "enum",
            // Honoured; see `validate`.
            "minimum",
        ];

        let doc = &*DOCUMENT;
        let mut seen = BTreeSet::new();
        for schema in doc["components"]["schemas"]
            .as_object()
            .expect("there are schemas")
            .values()
        {
            keywords(schema, &mut seen);
        }
        assert!(!seen.is_empty(), "no schemas at all");

        let unknown: Vec<&String> = seen
            .iter()
            .filter(|k| !UNDERSTOOD.contains(&k.as_str()))
            .collect();
        assert!(
            unknown.is_empty(),
            "the document uses schema keywords this validator ignores: {unknown:?}. \
             Implement them, or swap in a real JSON Schema validator."
        );
    }

    /// Every key that appears in a schema position.
    fn keywords(value: &Value, into: &mut BTreeSet<String>) {
        // These carry *values*, not schemas. Descending into them would report
        // an example's field names as keywords.
        const NOT_SCHEMAS: &[&str] = &["default", "enum", "example", "examples"];

        if let Some(object) = value.as_object() {
            for (key, child) in object {
                into.insert(key.clone());
                if NOT_SCHEMAS.contains(&key.as_str()) {
                    continue;
                }
                // `properties` keys are field names; its values are schemas.
                if key == "properties" {
                    for field in child.as_object().into_iter().flatten() {
                        keywords(field.1, into);
                    }
                    continue;
                }
                keywords(child, into);
            }
        } else if let Some(items) = value.as_array() {
            for item in items {
                keywords(item, into);
            }
        }
    }

    /// The validator says no when the document and the body disagree.
    ///
    /// Without this the whole module could be a no-op and every test above would
    /// still be green — which is exactly the failure it exists to prevent.
    #[test]
    fn the_validator_is_not_vacuous() {
        let doc = &*DOCUMENT;
        let schema = serde_json::json!({ "$ref": "#/components/schemas/AccountView" });

        let good = serde_json::json!({
            "code": "1000", "name": "Cash", "kind": "asset",
            "balance": 100, "currency": "SAR", "closed": false, "postings": 2
        });
        assert!(validate(doc, &schema, &good, "$", Closed::Yes).is_ok());

        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("balance");
        assert!(
            validate(doc, &schema, &missing, "$", Closed::Yes).is_err(),
            "missing field"
        );

        let mut renamed = good.clone();
        let object = renamed.as_object_mut().unwrap();
        object.remove("postings");
        object.insert("posting_count".into(), 2.into());
        assert!(
            validate(doc, &schema, &renamed, "$", Closed::Yes).is_err(),
            "renamed field"
        );

        let mut retyped = good.clone();
        retyped["balance"] = serde_json::json!("100");
        assert!(
            validate(doc, &schema, &retyped, "$", Closed::Yes).is_err(),
            "retyped field"
        );

        let mut extra = good.clone();
        extra["surprise"] = serde_json::json!(1);
        assert!(
            validate(doc, &schema, &extra, "$", Closed::Yes).is_err(),
            "extra field"
        );

        // And through `allOf`, which is how `#[serde(flatten)]` is published.
        let detail = serde_json::json!({ "$ref": "#/components/schemas/InvoiceDetailView" });
        let flattened = serde_json::json!({
            "id": "INV-1", "customer": "Acme", "customer_vat": null,
            "issued_on": "2026-08-15T00:00:00Z", "due_on": null, "cancelled_on": null,
            "credit_note": null, "currency": "SAR", "net": 100, "tax": 15, "gross": 115,
            "paid": 0, "outstanding": 115, "payment_count": 0, "note": "",
            "lines": [], "tax_breakdown": [], "payments_": []
        });
        assert!(
            validate(doc, &detail, &flattened, "$", Closed::Yes).is_err(),
            "a flattened shape missing `payments` and carrying `payments_` must fail"
        );
    }
}

/// **The whole VAT return: what was charged, what was paid, and the difference.**
///
/// The number a Saudi business actually files, and the reason the purchases
/// module exists. It is composed in the API from two modules whose read models
/// never see each other — `proj_sales` and `proj_purchases` are separate groups
/// and neither may read the other (architecture L3). Nothing below the
/// composition root could produce this figure.
#[tokio::test]
async fn a_tenant_files_output_tax_less_input_tax() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_both_sides(tenant).await;

    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // Charged: 1,000.00 at 15% → 150.00 of output tax.
    let (status, issued, _) = fixture
        .send(
            bearer(Request::post("/v1/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "crm-1",
                        "customer": { "name": "Rawabi", "vat_number": "310000000000003" },
                        "issued_on": "2026-02-10T00:00:00Z",
                        "currency": "SAR",
                        "lines": [
                            { "description": "Consulting", "net": 100_000, "vat": "standard" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");

    // Paid: 400.00 at 15% → 60.00 of input tax, as the supplier stated it.
    let (status, recorded, _) = fixture
        .send(
            bearer(Request::post("/v1/purchases/bills"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "ap-1",
                        "supplier": { "name": "Najd Supplies", "vat_number": "311234567800003" },
                        "reference": "NS-8891",
                        "billed_on": "2026-02-14T00:00:00Z",
                        "currency": "SAR",
                        "lines": [
                            { "description": "Subcontracting", "account": "5000",
                              "net": 40_000, "vat": "standard", "vat_rate": 1500, "tax": 6_000 }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{recorded}");

    fixture.project_both_sides(tenant).await;

    let (status, filed, _) = fixture
        .send(
            bearer(Request::get(
                "/v1/tax_sa/vat-return\
                 ?from=2026-01-01T00:00:00Z&until=2026-04-01T00:00:00Z&currency=SAR",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{filed}");

    assert_eq!(filed["output"]["tax"], 15_000, "what was charged");
    assert_eq!(
        filed["input"]["tax"], 6_000,
        "what was paid and can be reclaimed"
    );
    assert_eq!(
        filed["payable"], 9_000,
        "the number that gets paid: 150.00 charged less 60.00 reclaimed"
    );
    assert_eq!(filed["output"]["net"], 100_000);
    assert_eq!(filed["input"]["net"], 40_000);

    // A period with nothing in it is zero on both sides, not an error.
    let (status, empty, _) = fixture
        .send(
            bearer(Request::get(
                "/v1/tax_sa/vat-return\
                 ?from=2026-07-01T00:00:00Z&until=2026-10-01T00:00:00Z&currency=SAR",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["payable"], 0);

    fixture.cleanup().await;
}

/// A tenant with only sales gets zeroes for the input side, not a 404.
///
/// A business that has not enabled purchases genuinely reclaimed nothing, and
/// that is a return they can file. Refusing would make the endpoint useless to
/// most of the tenants that need it.
#[tokio::test]
async fn a_return_for_a_tenant_with_one_side_reports_the_other_as_nothing() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_selling_only(tenant).await;

    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    let (status, issued, _) = fixture
        .send(
            bearer(Request::post("/v1/sales/invoices"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "crm-1",
                        "customer": { "name": "Rawabi" },
                        "issued_on": "2026-02-10T00:00:00Z",
                        "currency": "SAR",
                        "lines": [
                            { "description": "Consulting", "net": 100_000, "vat": "standard" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{issued}");
    fixture.project_sales(tenant).await;

    let (status, filed, _) = fixture
        .send(
            bearer(Request::get(
                "/v1/tax_sa/vat-return\
                 ?from=2026-01-01T00:00:00Z&until=2026-04-01T00:00:00Z&currency=SAR",
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{filed}");
    assert_eq!(filed["output"]["tax"], 15_000);
    assert_eq!(filed["input"]["tax"], 0);
    assert_eq!(
        filed["input"]["bands"].as_array().map(Vec::len),
        Some(0),
        "no purchases module, so nothing to report on that side"
    );
    assert_eq!(filed["payable"], 15_000);

    // And the purchases routes themselves are not there for this tenant.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/purchases/bills"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "request.module_not_enabled");

    fixture.cleanup().await;
}

/// **A deprecated module keeps working for the tenants that have it.**
///
/// A build that drops a module strands them: events in the log with nothing that
/// reads them, read models that stop being refreshed, routes that 404 with no
/// explanation, and no way for the tenant to get off it. So a module on its way
/// out stays in the build and stops being *offered* — and the two halves of that
/// are what this checks.
#[tokio::test]
async fn a_deprecated_module_is_kept_by_whoever_has_it_and_offered_to_nobody() {
    let fixture = Fixture::new().await;

    let catalogue = spa_api::modules();
    let (name, _) = catalogue.first().expect("at least one module");

    // Nothing shipped is deprecated today, which is the state to be in — so the
    // catalogue says so, and a client building a picker can rely on the field
    // being there rather than discovering it the day one is.
    let (status, body, _) = fixture
        .send(Request::get("/v1/catalogue").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for module in body.as_array().expect("a list") {
        assert!(
            module["deprecated"].is_null(),
            "{} is deprecated and nothing said so in the plan: {module}",
            module["name"]
        );
        assert!(module["name"].is_string());
    }
    assert!(
        body.as_array()
            .is_some_and(|c| c.iter().any(|m| m["name"] == **name)),
        "the catalogue does not offer {name}"
    );

    fixture.cleanup().await;
}

/// **ZATCA, over HTTP.** Registering, the two obligations, and the chain.
///
/// The decision this exercises is the one a Saudi business is inspected on:
/// an invoice to a VAT-registered buyer is a *standard* one and has to be
/// cleared before they get it; a receipt at a till is *simplified* and has to be
/// reported within a day.
#[tokio::test]
async fn a_tenant_can_see_where_it_stands_with_zatca() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // Nothing registered yet, and the standing says exactly that.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["registered"], false);

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/registration"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "tax_sa.not_registered");

    // A VAT number that is not one is refused here rather than by ZATCA, and
    // the message says which rule.
    let registration = |vat: &str, name: &str| {
        serde_json::json!({
            "vat_number": vat,
            "name": name,
            "name_latin": "Acme Trading",
            "scheme": "crn",
            "identifier": "1010101010",
            "address": {
                "street": "طريق الملك فهد",
                "building": "2322",
                "additional": "9999",
                "district": "العليا",
                "city": "الرياض",
                "postal_code": "12211",
                "country": "SA"
            },
            "effective_from": "2026-01-01T00:00:00Z"
        })
    };

    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/registration"))
                .body(Body::from(
                    registration("123456789012345", "أكمي للتجارة").to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "tax_sa.invalid_registration");

    // And a name with no Arabic in it, because the invoice is an Arabic
    // document.
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/registration"))
                .body(Body::from(
                    registration("310122393500003", "Acme Trading").to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "tax_sa.invalid_registration");

    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/registration"))
                .body(Body::from(
                    registration("310122393500003", "أكمي للتجارة").to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["vat_number"], "310122393500003");

    fixture.cleanup().await;
}

/// **The decision a Saudi business is inspected on.** An invoice to a
/// VAT-registered buyer is a *standard* one and has to be cleared before they
/// get it; a receipt at a till is *simplified* and has to be reported within a
/// day.
#[tokio::test]
async fn zatca_documents_say_which_obligation_they_fall_under() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    fixture.register_with_zatca(&token).await;

    // One invoice to a business, one receipt to a consumer.
    let invoice = |id: &str, buyer: serde_json::Value, net: i64| {
        bearer(Request::post("/v1/sales/invoices"))
            .body(Body::from(
                serde_json::json!({
                    "id": id,
                    "customer": buyer,
                    "issued_on": "2026-02-10T00:00:00Z",
                    "currency": "SAR",
                    "lines": [{ "description": "استشارات", "net": net, "vat": "standard" }]
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture
        .send(invoice(
            "b2b",
            serde_json::json!({ "name": "روابي", "vat_number": "300000000000003" }),
            100_000,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body, _) = fixture
        .send(invoice("b2c", serde_json::json!({ "name": "زبون" }), 2_000))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    fixture.project_tax(tenant).await;

    // **The decision.** The buyer's VAT number is what makes it standard.
    let (status, documents, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/documents"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{documents}");
    let documents = documents.as_array().expect("a list");
    assert_eq!(documents.len(), 2);

    let business = documents
        .iter()
        .find(|d| d["number"] == "INV-00001")
        .expect("the B2B invoice");
    assert_eq!(business["kind"], "standard");
    assert_eq!(business["type_code"], 388);
    assert_eq!(business["status"], "pending");
    assert_eq!(business["icv"], 1);

    let consumer = documents
        .iter()
        .find(|d| d["number"] == "INV-00002")
        .expect("the till receipt");
    assert_eq!(consumer["kind"], "simplified");
    assert_eq!(consumer["icv"], 2);
    assert_eq!(
        consumer["previous_hash"], business["invoice_hash"],
        "the chain does not link the second document to the first"
    );

    // One document, with the bytes that were hashed and the QR that goes on the
    // print.
    let (status, one, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/documents/INV-00001"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    let xml = one["xml"].as_str().expect("the canonical UBL");
    assert!(xml.starts_with("<Invoice xmlns="));
    assert!(xml.contains("name=\"0100000\""), "not marked standard");
    assert!(xml.contains("<cbc:CompanyID>310122393500003</cbc:CompanyID>"));
    assert!(
        one["qr"].as_str().is_some_and(|qr| !qr.is_empty()),
        "no QR on a document that has to print one"
    );
    assert!(
        one["stamped_xml"].is_null(),
        "nothing is stamped until ZATCA stamps it"
    );

    fixture.cleanup().await;
}

/// **The two numbers an inspection asks about**, which are different questions:
/// simplified invoices past their twenty-four hours, and standard invoices the
/// buyer must not have been given yet.
#[tokio::test]
async fn the_zatca_standing_separates_late_from_merely_waiting() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    fixture.register_with_zatca(&token).await;

    for (id, buyer, net) in [
        (
            "b2b",
            serde_json::json!({ "name": "روابي", "vat_number": "300000000000003" }),
            100_000,
        ),
        ("b2c", serde_json::json!({ "name": "زبون" }), 2_000),
    ] {
        let (status, body, _) = fixture
            .send(
                bearer(Request::post("/v1/sales/invoices"))
                    .body(Body::from(
                        serde_json::json!({
                            "id": id,
                            "customer": buyer,
                            "issued_on": "2026-02-10T00:00:00Z",
                            "currency": "SAR",
                            "lines": [{ "description": "استشارات", "net": net, "vat": "standard" }]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    fixture.project_tax(tenant).await;

    // The standing, judged as of a day later: the till receipt is past its
    // twenty-four hours and the standard invoice is waiting for clearance.
    let (status, standing, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca?as_of=2026-02-12T00:00:00Z"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{standing}");
    assert_eq!(standing["registered"], true);
    assert_eq!(standing["overdue"], 1, "the receipt is late");
    assert_eq!(standing["awaiting_clearance"], 1, "the invoice is not");
    assert_eq!(standing["chain_length"], 2);
    assert_eq!(standing["counts"]["pending"], 2);

    // A document nobody issued.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/documents/INV-99999"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "tax_sa.no_such_document");

    fixture.cleanup().await;
}
