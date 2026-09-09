//! The HTTP surface, driven through the real router.
//!
//! No mocked state and no handler called directly: the requests go through
//! `Router::oneshot`, so the extractors, the rejections and the status mapping
//! are all under test. A handler tested by calling it has skipped the part most
//! likely to be wrong.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use erp_api::{AppState, router};
use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, Scope, TenantPools};
use erp_testkit::{Schema, TestDb};
use erp_types::{IdentityId, TenantId};
use futures_util::StreamExt as _;
use tower::ServiceExt;

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

struct Fixture {
    app: Router,
    control: Arc<ControlPlane>,
    db: TestDb,
    /// Where the streams these tests open wait; a test publishes into it the
    /// way the Redis listener would.
    hub: Arc<erp_web::realtime::Hub>,
    /// The DNS these tests publish to. `prove` puts the record a claim asked
    /// for where `verify_domain` will look.
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
/// The id a create is stored under, from a name that reads in the test.
///
/// A create takes its identity from `Idempotency-Key` and nothing else, and the
/// header must be a UUID — a value a human would pick collides with another
/// human's, which is the whole reason the API stopped accepting one in the body.
/// Deriving it from a name keeps the tests readable and the ids stable.
fn idem(name: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()).to_string()
}

/// The next SSE event on a body: `(event, data)`. Comments (keep-alives) are
/// skipped. `None` when nothing arrives in time or the body ends.
async fn next_event(
    body: &mut axum::body::BodyDataStream,
    buffer: &mut String,
    wait: Duration,
) -> Option<(String, String)> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    rest.trim().clone_into(&mut event);
                } else if let Some(rest) = line.strip_prefix("data:") {
                    rest.trim().clone_into(&mut data);
                }
            }
            if event.is_empty() && data.is_empty() {
                continue; // a comment
            }
            return Some((event, data));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, body.next()).await {
            Ok(Some(Ok(bytes))) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
            _ => return None,
        }
    }
}

/// A projection advance, as the worker would announce it.
fn advanced(
    tenant: TenantId,
    group: &str,
    module: &str,
    position: i64,
    streams: Option<Vec<erp_types::StreamId>>,
) -> erp_control::shared::Advanced {
    erp_control::shared::Advanced {
        tenant,
        group: group.to_owned(),
        module: erp_types::ModuleId::new(module).expect("a module"),
        position: erp_types::LogPosition::new(position).expect("a position"),
        streams,
    }
}

/// Turns online booking on for a tenant, the way the settings route would.
async fn open_the_diary(fixture: &Fixture, tenant: TenantId) {
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        booking::PublicBooking::KEY,
        &booking::PublicBooking {
            verify_phone: false,
            hold_minutes: 0,
            open: true,
            deposit_bp: 0,
        },
        None,
        None,
    )
    .await
    .expect("stores the setting");
}

impl Fixture {
    async fn new() -> Self {
        Self::with_hub(Arc::new(erp_web::realtime::Hub::new(
            erp_web::realtime::Caps::default(),
        )))
        .await
    }

    async fn with_hub(hub: Arc<erp_web::realtime::Hub>) -> Self {
        let db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");

        let prover = Arc::new(FakeProver::default());
        let control = Arc::new(
            ControlPlane::new(
                db.pool().clone(),
                TenantPools::new(clusters, PoolConfig::default()),
            )
            .with_prover(Arc::clone(&prover) as Arc<dyn erp_control::DomainProver>),
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

        Self {
            prover,
            // With a sealing key, because a deployment that stores tenant
            // secrets has one — and `no_sealing_key_refuses_rather_than_storing`
            // covers the deployment that does not.
            app: router(
                AppState::new(Arc::clone(&control))
                    // The router is driven with `oneshot`, so there is no socket
                    // and no peer address; the tests that need a caller to be
                    // somebody send `X-Forwarded-For`, the way a proxy would.
                    .trusting_forwarded_for(true)
                    .streaming_through(Arc::clone(&hub))
                    .sealing_with(
                        erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes"),
                    )
                    // And somewhere to keep files, because a deployment that
                    // takes uploads has one — and `files.no_storage` covers the
                    // deployment that does not.
                    .storing_in(std::sync::Arc::new(erp_storage::Local::at(
                        std::env::temp_dir().join(format!("erp-api-files-{}", std::process::id())),
                    ))),
            ),
            control,
            db,
            hub,
        }
    }

    /// Opens a stream and returns its status and body, unread.
    async fn open_stream(
        &self,
        request: Request<Body>,
    ) -> (StatusCode, axum::body::BodyDataStream) {
        let response = self.raw(request).await;
        (response.status(), response.into_body().into_data_stream())
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

    /// The confirmation token for the last email promised to an address.
    ///
    /// **The mailbox**, and the only stand-in for one these tests have. The
    /// token exists nowhere else on purpose: an API that handed it back would
    /// let a caller confirm their own signup, which is the whole thing
    /// `POST /v1/signups` now refuses to do.
    async fn confirmation(&self, email: &str) -> String {
        let body: String = sqlx::query_scalar(
            "SELECT payload ->> 'body' FROM outbox
              WHERE kind = 'email.send' AND payload ->> 'to' = $1
              ORDER BY id DESC LIMIT 1",
        )
        .bind(email)
        .fetch_one(self.control.pool())
        .await
        .unwrap_or_else(|e| panic!("a confirmation was promised to {email}: {e}"));

        body.split_once("/v1/signups/")
            .map(|(_, rest)| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| panic!("no confirmation link in the message to {email}: {body}"))
    }

    /// Signs up and confirms, the way a person with a mailbox does.
    ///
    /// Returns the confirmation's body, which is what the old one-shot signup
    /// used to answer with — so a test that only wants a working tenant reads
    /// the same fields it always did.
    async fn signup(&self, request: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let email = request["email"].as_str().expect("an email").to_owned();

        let (status, body, _) = self
            .send(
                Request::post("/v1/signups")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await;
        if status != StatusCode::ACCEPTED {
            return (status, body);
        }

        let token = self.confirmation(&email).await;
        let (status, body, _) = self
            .send(
                Request::post(format!("/v1/signups/{token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await;
        (status, body)
    }

    async fn provision(&mut self, slug: &str) -> TenantId {
        let tenant = self
            .control
            .register_tenant_on(slug, slug, "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
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
    /// Publishes the record a claim asked for, the way a tenant would at their
    /// DNS provider.
    fn prove(&self, domain: &str, token: &str) {
        self.prover.publish(
            &erp_control::record_name(domain),
            &erp_control::record_value(token),
        );
    }

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
        // An event stream has no end to read to: its body is what
        // `open_stream` is for, and here only the status is the answer.
        let json = if content_type.starts_with(b"text/event-stream") {
            drop(response);
            serde_json::Value::Null
        } else {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body reads");
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        // Every response in this file is also a contract test. See `contract`.
        contract::check(&method, &path, status, &json);
        (status, json, content_type)
    }

    /// The whole response, headers included.
    ///
    /// [`Self::send`] returns the parsed body, which is what almost every test
    /// wants. CORS is decided entirely in headers, so those tests need this.
    async fn raw(&self, request: Request<Body>) -> axum::response::Response {
        let mut request = request;
        if !request.headers().contains_key(header::HOST) {
            request.headers_mut().insert(
                header::HOST,
                axum::http::HeaderValue::from_static("acme.localhost"),
            );
        }
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("the router responds")
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
    async fn enable_module(&self, tenant: TenantId, setup: erp_control::ModuleSetup) {
        self.control
            .install_module(tenant, setup, Actor::system())
            .await
            .expect("module installs");
    }

    async fn enable_ledger(&self, tenant: TenantId) {
        self.enable_module(tenant, ledger::setup()).await;
    }

    /// **Why a tenant that invoices has to say this.** A line carrying no tax
    /// must name the ZATCA article it is untaxed under, and issuing one is
    /// refused until somebody has chosen it. These tests invoice exports at
    /// zero rate and rent as exempt, so they are that tenant.
    async fn configure_vat_reasons(&self, tenant: TenantId) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance access");
        let mut conn = db.acquire().await.expect("a connection");
        erp_eventlog::configuration::set(
            &mut conn,
            ledger::Rates::KEY,
            &ledger::Rates {
                standard: 1_500,
                zero_reason: Some("VATEX-SA-32".to_owned()),
                exempt_reason: Some("VATEX-SA-30".to_owned()),
            },
            Some("the-accountant"),
            None,
        )
        .await
        .expect("rates configure");
    }

    /// Sales needs the ledger underneath it.
    async fn enable_sales(&self, tenant: TenantId) {
        self.enable_ledger(tenant).await;
        self.enable_module(tenant, sales::setup()).await;
        self.configure_vat_reasons(tenant).await;
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
    async fn project<G: erp_projection::ProjectionGroup>(
        &self,
        tenant: TenantId,
        projections: &[std::sync::Arc<dyn erp_projection::Projection<Group = G>>],
        upcasters: &erp_eventlog::Upcasters,
    ) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let refs: Vec<&dyn erp_projection::Projection<Group = G>> =
            projections.iter().map(AsRef::as_ref).collect();

        loop {
            let mut tx = db.begin().await.expect("transaction");
            let progress = erp_projection::run_once_in::<G>(&mut tx, &refs, upcasters, 200)
                .await
                .expect("projects");
            if matches!(progress, erp_projection::Progress::Advanced { .. }) {
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

    async fn project_booking(&self, tenant: TenantId) {
        self.project(tenant, &booking::projections(), booking::upcasters())
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
                            "industry": "Consulting",
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
                    .header("idempotency-key", idem(id))
                    .body(Body::from(
                        serde_json::json!({
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
    /// the signup tests each tried to drop `erp_tenant_acme` — a name that has
    /// not been right since database names stopped being derived from the slug.
    /// Three databases leaked per green run, and nothing noticed. Asking the
    /// rows cannot drift the way remembering can.
    async fn cleanup(self) {
        let names: Vec<String> = sqlx::query_scalar("SELECT database_name FROM tenant")
            .fetch_all(self.db.pool())
            .await
            .expect("reads the tenants this test made");

        for name in &names {
            let _ = erp_testkit::drop_named_database(name).await;
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
        // The key is derived from the body, so each distinct request carries a
        // distinct one and a repeat of the same request is a retry.
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", idem(&body.to_string()))
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
                .header("idempotency-key", idem("inv-1"))
                .body(Body::from(
                    serde_json::json!({
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
                .header("idempotency-key", idem("inv-1"))
                .body(Body::from(
                    serde_json::json!({
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

    let (status, body) = fixture
        .signup(serde_json::json!({
            "slug": "acme",
            "company": "Acme Trading",
            "email": "owner@acme.test",
            "password": "correct horse battery staple",
            "modules": ["ledger"]
        }))
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
        serde_json::json!({
            "slug": "acme", "company": "Acme",
            "email": email, "password": "correct horse battery staple"
        })
    };

    let (first, _) = fixture.signup(signup("a@acme.test")).await;
    assert_eq!(first, StatusCode::CREATED);

    // Refused at the *request*, before any mail: the name is gone, and finding
    // that out after a round trip through a mailbox would be worse.
    let (second, body, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(signup("b@acme.test").to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(second, StatusCode::CONFLICT);
    assert_eq!(body["code"], "provisioning.slug_taken");

    fixture.cleanup().await;
}

/// **The whole of item 5, as one assertion.**
///
/// A request that gets past validation used to run `CREATE DATABASE` and a full
/// migration chain, from an unauthenticated endpoint. So the thing to check is
/// not that signup still works — the test above does that — but that the
/// expensive, irreversible half of it *has not happened yet* when the request
/// comes back.
///
/// Three ways of asking the same question, because one of them alone would be
/// satisfied by a half-measure: no tenant row, no database on the cluster, and
/// no authenticator claiming the address.
#[tokio::test]
async fn a_signup_request_builds_nothing_until_the_address_answers() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
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

    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["email"], "owner@acme.test");
    assert!(
        body.get("token").is_none(),
        "the response must not carry the confirmation token: a caller that had \
         it could confirm its own signup, which is the whole thing this refuses"
    );

    // No tenant.
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant")
        .fetch_one(fixture.control.pool())
        .await
        .expect("tenants are countable");
    assert_eq!(tenants, 0, "a request must not register a tenant");

    // And no claim on the address, which was the other half: signing up as
    // somebody else's address used to lock them out of ever signing up.
    let logins: i64 = sqlx::query_scalar("SELECT count(*) FROM authenticator WHERE handle = $1")
        .bind("owner@acme.test")
        .fetch_one(fixture.control.pool())
        .await
        .expect("authenticators are countable");
    assert_eq!(logins, 0, "a request must not claim the login handle");

    // What it *did* do: promise exactly one email, in the same transaction.
    let promised: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE payload ->> 'to' = $1")
            .bind("owner@acme.test")
            .fetch_one(fixture.control.pool())
            .await
            .expect("effects are countable");
    assert_eq!(promised, 1, "one confirmation, promised and not sent");

    // **And now the disk, which is the cost item 5 was actually about.**
    //
    // Named after the tenant, so it can only be asked about once there is one —
    // which is the assertion. Scoping it to this tenant's own id is also what
    // makes it isolated: `datname LIKE 'erp_tenant_%'` would count every other
    // test sharing the cluster and answer a different question.
    let link = fixture.confirmation("owner@acme.test").await;
    let (status, confirmed, _) = fixture
        .send(
            Request::post(format!("/v1/signups/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{confirmed}");

    let tenant: TenantId = confirmed["tenant"]
        .as_str()
        .expect("a tenant id")
        .parse()
        .expect("it parses");
    let database = format!("erp_tenant_{}", tenant.as_uuid().simple());
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&database)
            .fetch_one(fixture.control.pool())
            .await
            .expect("pg_database is readable");
    assert!(
        exists,
        "{database} should exist once the address answered, and not before"
    );

    fixture.cleanup().await;
}

/// Confirming builds the lot, and the link is spent when it has.
#[tokio::test]
async fn a_confirmation_link_works_once() {
    let fixture = Fixture::new().await;

    let (status, _, _) = fixture
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
    assert_eq!(status, StatusCode::ACCEPTED);

    let link = fixture.confirmation("owner@acme.test").await;
    let confirm = || {
        Request::post(format!("/v1/signups/{link}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap()
    };

    let (status, body, _) = fixture.send(confirm()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "acme");
    assert_eq!(body["modules"][0], "ledger");

    // The session works, which is the part that says provisioning finished.
    let token = body["token"].as_str().expect("a token").to_owned();
    let (status, tenant, _) = fixture
        .send(
            Request::get("/v1/tenant")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tenant}");

    // Twice does not build twice. A second click on the same link in a mail
    // client, or a browser retrying, must not produce a second company.
    let (status, body, _) = fixture.send(confirm()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "signups.not_valid");

    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant")
        .fetch_one(fixture.control.pool())
        .await
        .expect("tenants are countable");
    assert_eq!(tenants, 1, "one link, one company");

    fixture.cleanup().await;
}

/// A token nobody issued is the same answer as one already spent.
#[tokio::test]
async fn an_unissued_confirmation_token_is_not_found() {
    let fixture = Fixture::new().await;

    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/signups/{}", "0".repeat(64)))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "signups.not_valid");
    fixture.cleanup().await;
}

/// **The vector this flow would have introduced while closing a bigger one.**
///
/// Deferring provisioning behind an email makes `POST /v1/signups` a way to
/// send mail to any address somebody names. The interval caps that per address,
/// which is all this endpoint can do until there is a notion of caller to limit
/// (Phase 12c).
#[tokio::test]
async fn a_second_request_to_the_same_address_is_refused_for_a_minute() {
    let fixture = Fixture::new().await;

    let request = |slug: &str| {
        Request::post("/v1/signups")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT_LANGUAGE, "en")
            .body(Body::from(
                serde_json::json!({
                    "slug": slug, "company": "Acme",
                    "email": "owner@acme.test",
                    "password": "correct horse battery staple"
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, _, _) = fixture.send(request("acme")).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body, _) = fixture.send(request("acme-two")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "signups.too_soon");
    assert!(
        body["detail"].as_str().expect("prose").contains("second"),
        "the refusal has to say when to come back: {body}"
    );

    // And it did not send a second one, which is the point.
    let promised: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE payload ->> 'to' = $1")
            .bind("owner@acme.test")
            .fetch_one(fixture.control.pool())
            .await
            .expect("effects are countable");
    assert_eq!(promised, 1, "one address, one message a minute");

    fixture.cleanup().await;
}

/// A slug taken while the link sat in a mailbox is a 409, and the link survives.
///
/// The name is deliberately not reserved — see `0010_signups.sql` — so this is
/// the case that trade-off creates, and the requirement is that it is
/// recoverable rather than that it cannot happen.
#[tokio::test]
async fn a_name_taken_while_you_were_reading_your_mail_does_not_burn_the_link() {
    let mut fixture = Fixture::new().await;

    let (status, _, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme", "company": "Acme Trading",
                        "email": "owner@acme.test",
                        "password": "correct horse battery staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let link = fixture.confirmation("owner@acme.test").await;

    // Somebody else takes the name in the meantime.
    fixture.provision("acme").await;

    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/signups/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "provisioning.slug_taken");

    // **Unclaimed.** A failure that burned the link would turn a recoverable
    // error into a support ticket, so the row is still live and still theirs.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pending_signup
          WHERE handle = $1 AND confirmed_at IS NULL AND cancelled_at IS NULL",
    )
    .bind("owner@acme.test")
    .fetch_one(fixture.control.pool())
    .await
    .expect("pending signups are countable");
    assert_eq!(live, 1, "a failed confirmation must not spend the link");

    fixture.cleanup().await;
}

/// The confirmation is written in the language the form was in.
///
/// It has to be decided at request time and stored: the person has no account,
/// so there is no preference to look up when a worker picks the row up, and by
/// then the only signal there ever was is gone.
#[tokio::test]
async fn the_confirmation_is_written_in_the_language_of_the_form() {
    let fixture = Fixture::new().await;

    let (status, _, _) = fixture
        .send(
            Request::post("/v1/signups")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({
                        "slug": "acme", "company": "أكمي",
                        "email": "owner@acme.test",
                        "password": "correct horse battery staple"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (locale, subject): (String, String) = sqlx::query_as(
        "SELECT payload ->> 'locale', payload ->> 'subject' FROM outbox
          WHERE payload ->> 'to' = $1",
    )
    .bind("owner@acme.test")
    .fetch_one(fixture.control.pool())
    .await
    .expect("a message was promised");

    assert_eq!(locale, "arabic", "the form was in Arabic");
    assert!(
        subject.contains("أكمي"),
        "the subject names the company, in Arabic: {subject}"
    );

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

    let (_, signed_up) = fixture
        .signup(serde_json::json!({
            "slug": "acme", "company": "Acme Trading",
            "email": "owner@acme.test",
            "password": "correct horse battery staple",
            "modules": ["ledger"]
        }))
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

    let document = serde_json::to_value(erp_api::openapi()).expect("the document serializes");
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
    // **The second factor is nobody's role.** It is about the person signing in,
    // not about anything in a tenant — a viewer must be able to protect their
    // own account, and an owner must not be able to touch somebody else's.
    // Every route here acts on `auth.session.identity` and takes no tenant, so
    // every role reaches all four and each one reaches only themselves.
    ("second_factor", ALL_ROLES),
    ("begin_second_factor", ALL_ROLES),
    ("confirm_second_factor", ALL_ROLES),
    ("disable_second_factor", ALL_ROLES),
    // Reading is what a viewer is for.
    ("tenant", ALL_ROLES),
    ("tenant_calendar", ALL_ROLES),
    ("list_members", ALL_ROLES),
    ("list_modules", ALL_ROLES),
    ("list_customers", ALL_ROLES),
    ("get_customer", ALL_ROLES),
    ("list_accounts", ALL_ROLES),
    ("trial_balance", ALL_ROLES),
    ("list_invoices", ALL_ROLES),
    ("get_invoice", ALL_ROLES),
    // Who owes what. A viewer may see it — chasing a debt is not a privileged
    // act, and the person who makes the call is rarely the one who can post.
    ("receivables", ALL_ROLES),
    ("posting_accounts", ALL_ROLES),
    ("books", ALL_ROLES),
    ("vat_rates", ALL_ROLES),
    ("vat_return", ALL_ROLES),
    ("filed_returns", ALL_ROLES),
    ("list_bills", ALL_ROLES),
    ("get_bill", ALL_ROLES),
    // The diary and the rota. A viewer may read both: knowing who is booked in
    // at ten is what everyone on a shop floor needs, and it is the screen most
    // of these businesses live on.
    // What a customer has already paid for. A viewer may read it: the person
    // at the counter has to be able to say "you have three sessions left"
    // without being able to sell them a fourth.
    ("list_entitlements", ALL_ROLES),
    ("get_entitlement", ALL_ROLES),
    ("list_subscriptions", ALL_ROLES),
    ("get_subscription", ALL_ROLES),
    ("outstanding", ALL_ROLES),
    ("deferral_accounts", ALL_ROLES),
    ("list_branches", ALL_ROLES),
    ("get_branch", ALL_ROLES),
    ("list_shifts", ALL_ROLES),
    ("get_shift", ALL_ROLES),
    ("shift_takings", ALL_ROLES),
    ("till_accounts", ALL_ROLES),
    ("list_cards", ALL_ROLES),
    ("get_card", ALL_ROLES),
    ("loyalty_scheme", ALL_ROLES),
    ("list_bookables", ALL_ROLES),
    // What the hours cost. A viewer may read the tariff for the same reason
    // they may read the VAT rate: it is on every quote they give a customer.
    ("tariff", ALL_ROLES),
    ("public_booking_settings", ALL_ROLES),
    ("billing_settings", ALL_ROLES),
    ("get_bookable", ALL_ROLES),
    ("list_reservations", ALL_ROLES),
    ("get_reservation", ALL_ROLES),
    // Where the business stands with ZATCA, and the documents themselves.
    // Reading, so everyone — a clerk at a till needs to see that the receipt
    // they just handed over has been reported.
    ("registration", ALL_ROLES),
    ("onboarding_status", ALL_ROLES),
    ("event_stream", ALL_ROLES),
    ("zatca_standing", ALL_ROLES),
    ("zatca_documents", ALL_ROLES),
    ("zatca_document", ALL_ROLES),
    // Recording what happened. A clerk does this and nothing structural.
    ("post_entry", &["owner", "accountant", "clerk"]),
    ("reverse_entry", &["owner", "accountant", "clerk"]),
    ("issue_invoice", &["owner", "accountant", "clerk"]),
    // Billing a booking is issuing an invoice, so the same hands.
    ("bill_reservation_route", &["owner", "accountant", "clerk"]),
    ("refund_payment", &["owner", "accountant", "clerk"]),
    ("unmatched_customers", ALL_ROLES),
    // The dashboard. A viewer may read what the business sold, how the diary
    // went and what the tills took — those are the numbers a shop floor runs
    // on, and `shift_takings` already says a viewer may see a drawer's
    // variance.
    ("revenue", ALL_ROLES),
    ("utilisation", ALL_ROLES),
    ("takings", ALL_ROLES),
    // **Not the wage bill.** What the people in the room are paid is not a
    // figure a receptionist with a dashboard should be able to total, and the
    // reconciliation is a statement about the books.
    ("people_cost", &["owner", "accountant"]),
    ("reconciliation", &["owner", "accountant"]),
    // Messaging. Reading what the business says and what it has spent is
    // ordinary; **writing a template is not** — it changes what every customer
    // is told and what the tenant is billed for, which is the same kind of act
    // as enabling a module.
    ("vocabulary", ALL_ROLES),
    ("list_templates", ALL_ROLES),
    ("get_template", ALL_ROLES),
    ("messaging_settings", ALL_ROLES),
    ("messaging_budget", ALL_ROLES),
    ("messaging_spend", ALL_ROLES),
    // A device registering itself. A viewer's app may do it, because the app is
    // whatever the person signed in is holding.
    // A token bound to somebody else's id is somebody else's notifications.
    ("register_device", OWNER),
    ("put_template", &["owner"]),
    ("delete_template", &["owner"]),
    ("set_messaging_settings", &["owner"]),
    ("set_messaging_budget", &["owner"]),
    // Sending. A clerk texts a customer; that is the counter's job.
    ("send_message", &["owner", "accountant", "clerk"]),
    // **The bell is everybody's, including the three routes that write.** A
    // viewer must be able to clear their own inbox and say they do not want
    // SMS: neither changes anything about the tenant, and nothing here can name
    // somebody else — the recipient is always the session's own identity.
    ("list_notifications", ALL_ROLES),
    ("mark_read", ALL_ROLES),
    ("mark_all_read", ALL_ROLES),
    ("get_preferences", ALL_ROLES),
    ("set_preferences", ALL_ROLES),
    // **Conversations, and the one place reading is not `read`.** A thread
    // holds staff's private notes about a customer; `viewer` is the role for an
    // external accountant at year end, with every reason to see the books and
    // none to see what the front desk wrote.
    ("read_conversation", &["owner", "accountant", "clerk"]),
    ("add_note", &["owner", "accountant", "clerk"]),
    ("send_in_conversation", &["owner", "accountant", "clerk"]),
    ("list_unmatched", &["owner", "accountant", "clerk"]),
    ("assign_unmatched", &["owner", "accountant", "clerk"]),
    // Documents. Reading what is attached is ordinary; attaching and taking
    // off is recording what happened, which is a clerk's job.
    ("list_attachments", ALL_ROLES),
    ("get_attachment", ALL_ROLES),
    ("download_file", ALL_ROLES),
    // Importing a spreadsheet of customers creates records in bulk under ids
    // the file chose. Structural, so the owner's.
    ("import_customers", &["owner"]),
    // API keys are a way into the tenant. Issuing, rotating and revoking one is
    // changing who has access, which is the same act as adding a member.
    // Inbound callbacks. Reading what arrived is ordinary; the shared secret a
    // provider signs with is a credential, so setting it is the owner's.
    ("list_webhook_events", ALL_ROLES),
    ("set_webhook_secret", &["owner"]),
    ("list_keys", &["owner"]),
    ("issue_key", &["owner"]),
    ("list_dead_letters", OWNER),
    ("requeue_dead_letter", OWNER),
    ("rotate_key", &["owner"]),
    ("revoke_key", &["owner"]),
    ("upload_file", &["owner", "accountant", "clerk"]),
    ("remove_file", &["owner", "accountant", "clerk"]),
    // Gateway payments. Reading what was tried against an invoice is ordinary —
    // a clerk on the phone to a customer needs it. Recording a charge and
    // giving money back both move the books, so they sit with the rest of
    // `post_entries`.
    ("list_payments", ALL_ROLES),
    ("get_payment", ALL_ROLES),
    ("start_payment", &["owner", "accountant", "clerk"]),
    ("refund_gateway_payment", &["owner", "accountant", "clerk"]),
    // A gateway's secret key charges cards. Handing one over is the same act as
    // adding a member, so it is the owner's.
    ("set_gateway", &["owner"]),
    // Settlement. Reading what the gateway still owes is ordinary bookkeeping;
    // recording a payout posts to the ledger, so it sits with the rest of
    // `post_entries`.
    ("list_settlement", ALL_ROLES),
    ("record_payout", &["owner", "accountant", "clerk"]),
    // Saved cards. Offering a customer the card they left last time is what a
    // clerk at a counter does, so the list is ordinary. **Saving one and
    // charging one are not**: a saved card is a standing permission to take
    // money without the customer present, and charging it is a charge. Both
    // sit with the rest of `post_entries`, and removing one does too — a card
    // taken away without the customer asking is a charge that will fail at the
    // worst moment.
    ("list_saved_cards", ALL_ROLES),
    ("save_gateway_card", &["owner", "accountant", "clerk"]),
    ("forget_gateway_card", &["owner", "accountant", "clerk"]),
    ("charge_saved_card", &["owner", "accountant", "clerk"]),
    // Keeping a deposit raises an invoice and declares tax, so it sits with the
    // rest of `post_entries`. **Whether keeping one is a supply at all is a tax
    // position**, which is the owner's — the same call as handing over a
    // gateway's secret key.
    ("retain_deposit", &["owner", "accountant", "clerk"]),
    ("deposit_policy", ALL_ROLES),
    ("set_deposit_policy", &["owner"]),
    // The org chart. Reading it is ordinary — a staff list is on the wall in
    // most businesses. Changing it is changing the authorization structure, and
    // granting a claim escalates every ancestor, so it is the owner's.
    ("list_employees", ALL_ROLES),
    ("get_employee", ALL_ROLES),
    ("list_claims", ALL_ROLES),
    ("hire_employee", OWNER),
    ("amend_employee", OWNER),
    ("reparent_employee", OWNER),
    ("transfer_employee", OWNER),
    // Which login is which person grants nothing — but it decides who receives
    // what, which is the owner's to say.
    ("link_employee_login", OWNER),
    ("unlink_employee_login", OWNER),
    ("record_leaving", OWNER),
    ("grant_claim", OWNER),
    ("revoke_claim", OWNER),
    ("record_document", OWNER),
    ("expiring_documents", ALL_ROLES),
    ("list_skills", ALL_ROLES),
    ("record_skills", OWNER),
    ("record_salary", OWNER),
    ("list_rota", ALL_ROLES),
    ("record_shifts", OWNER),
    ("timesheet", ALL_ROLES),
    ("list_leave", ALL_ROLES),
    // A supervisor approving a timesheet is recording work, not restructuring
    // the business — so this sits with the clerk's other daily recording.
    ("record_day", &["owner", "accountant", "clerk"]),
    ("record_leave", &["owner", "accountant", "clerk"]),
    // Payroll. Drafting posts nothing and approving posts, so both sit with
    // the other recording work — an accountant runs payroll.
    ("list_runs", ALL_ROLES),
    ("get_run", ALL_ROLES),
    ("run_payslips", ALL_ROLES),
    ("draft_run", &["owner", "accountant", "clerk"]),
    ("approve_run", &["owner", "accountant", "clerk"]),
    ("payroll_accounts", ALL_ROLES),
    ("set_payroll_accounts", &["owner", "accountant"]),
    // The Kingdom's own arithmetic. Both calculations answer and record
    // nothing, so anybody may ask; the schedule they are computed from is the
    // shape of the books.
    ("gosi_schedule", ALL_ROLES),
    ("gosi_for", ALL_ROLES),
    ("end_of_service_for", ALL_ROLES),
    ("leave_entitlement", ALL_ROLES),
    ("set_gosi_schedule", &["owner", "accountant"]),
    // Which websites may call this tenant's public API. Reading the list is
    // ordinary; changing it is changing who may reach a business's diary from a
    // browser, which is the owner's decision and nobody else's.
    ("list_origins", ALL_ROLES),
    ("allow_origin", OWNER),
    ("revoke_origin", OWNER),
    ("claim_domain", OWNER),
    ("set_tenant_calendar", OWNER),
    ("verify_domain", OWNER),
    ("attach_customer", OWNER),
    ("record_payment", &["owner", "accountant", "clerk"]),
    ("credit_note", &["owner", "accountant", "clerk"]),
    // A partial credit note moves the books and the VAT return, so it sits with
    // the rest of `post_entries`. Reading them is ordinary bookkeeping.
    ("list_credit_notes", ALL_ROLES),
    ("credit_invoice_part", &["owner", "accountant", "clerk"]),
    ("record_bill", &["owner", "accountant", "clerk"]),
    ("pay_bill", &["owner", "accountant", "clerk"]),
    // Taking a booking, moving it along, and picking the room. This is the
    // receptionist's whole job, and the reason a clerk exists as a role.
    // Selling a package and delivering a session both post to the ledger, so
    // they sit with the other recording work. A clerk at a counter does this
    // all day.
    ("grant_entitlement", &["owner", "accountant", "clerk"]),
    ("redeem_entitlement", &["owner", "accountant", "clerk"]),
    ("expire_entitlement", &["owner", "accountant", "clerk"]),
    ("revoke_entitlement", &["owner", "accountant", "clerk"]),
    ("start_subscription", &["owner", "accountant", "clerk"]),
    ("recognise_subscription", &["owner", "accountant", "clerk"]),
    ("freeze_subscription", &["owner", "accountant", "clerk"]),
    ("resume_subscription", &["owner", "accountant", "clerk"]),
    ("renew_subscription", &["owner", "accountant", "clerk"]),
    ("cancel_subscription", &["owner", "accountant", "clerk"]),
    ("open_shift", &["owner", "accountant", "clerk"]),
    ("ring_sale", &["owner", "accountant", "clerk"]),
    ("take_back", &["owner", "accountant", "clerk"]),
    ("pay_out", &["owner", "accountant", "clerk"]),
    ("close_shift", &["owner", "accountant", "clerk"]),
    ("open_card", &["owner", "accountant", "clerk"]),
    ("earn_on_card", &["owner", "accountant", "clerk"]),
    ("redeem_card_points", &["owner", "accountant", "clerk"]),
    ("expire_card_points", &["owner", "accountant", "clerk"]),
    ("take_reservation", &["owner", "accountant", "clerk"]),
    ("move_reservation", &["owner", "accountant", "clerk"]),
    ("reschedule_reservation", &["owner", "accountant", "clerk"]),
    ("assign_unit", &["owner", "accountant", "clerk"]),
    // Changing the shape of the books. Not a clerk's job — they post into
    // the chart, they do not restructure it.
    ("open_account", &["owner", "accountant"]),
    ("install_chart", &["owner", "accountant"]),
    ("set_posting_accounts", &["owner", "accountant"]),
    // Where a liability is held is the shape of the books, not a day's work.
    ("set_deferral_accounts", &["owner", "accountant"]),
    ("set_loyalty_scheme", &["owner", "accountant"]),
    ("set_till_accounts", &["owner", "accountant"]),
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
    // Generating a signing key and taking a certificate for it. The owner
    // alone, and not because it is administrative — because whoever can do
    // this can replace the key every invoice this business issues is signed
    // with, and an accountant keeping the books is not that person.
    ("begin_onboarding", OWNER),
    ("accept_certificate", OWNER),
    ("activate", OWNER),
    ("add_member", OWNER),
    ("change_role", OWNER),
    ("remove_member", OWNER),
    ("set_module_role", OWNER),
    ("clear_module_role", OWNER),
    // A customer record is tenant data about who the business deals with, so
    // it sits with members and modules and not with the books.
    // Who works here, what rooms there are and when they are open is the shape
    // of the business, not a day's work in it. A clerk books into the rota;
    // they do not write it.
    // Fitting a tenant out for a trade declares its whole rota at once, which
    // is the same decision as declaring one by hand and sits in the same place.
    ("fit_out", OWNER),
    // Which hours cost more is the shape of the business, and it is the half a
    // client must not be able to decide for itself.
    ("set_tariff", OWNER),
    ("set_public_booking_settings", OWNER),
    ("set_billing_settings", OWNER),
    ("declare_bookable", OWNER),
    ("amend_bookable", OWNER),
    ("set_opening_hours", OWNER),
    ("withdraw_bookable", OWNER),
    ("restore_bookable", OWNER),
    // **Barring a customer from a resource is the owner's, and lifting one is
    // too.** Not because reading a complaint is above a clerk, but because
    // there is no override: a bar refuses every booking that would put the two
    // together and nobody can click past it, which makes raising one a heavier
    // lever than taking a chair out of service — and that is already the
    // owner's. Reading the list is ordinary; a receptionist has to know why the
    // diary just refused them.
    ("list_bars", ALL_ROLES),
    ("raise_bar", OWNER),
    ("lift_bar", OWNER),
    ("open_branch", OWNER),
    ("amend_branch", OWNER),
    ("close_branch", OWNER),
    ("reopen_branch", OWNER),
    ("register_customer", OWNER),
    ("amend_customer", OWNER),
    // **Fields a business adds to its customers.** Reading the shape is
    // ordinary — a form has to know what to draw. Deciding it is the owner's:
    // what a business records about a person, and especially *that* it records
    // health details, is not a preference, and it changes what every customer
    // page asks for from then on.
    ("customer_fields", ALL_ROLES),
    ("set_customer_fields", OWNER),
    // Filling one in is clerical work at a counter. Emptying one is too — the
    // old value is kept as history either way.
    ("held_fields", ALL_ROLES),
    ("set_held_fields", &["owner", "accountant", "clerk"]),
    ("clear_held_field", &["owner", "accountant", "clerk"]),
    // **Erasing is not.** This is the answer to somebody asking for their data
    // to be deleted, it takes the history with it, and this system has already
    // declined once to answer "who may erase whom" in passing. The owner's.
    ("erase_held_fields", OWNER),
    ("orphaned_fields", OWNER),
    ("erase_field_values", OWNER),
    ("archive_customer", OWNER),
    ("restore_customer", OWNER),
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
/// So the endpoints come from `erp_api::openapi()` — the same value the router
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
        233,
        "expected two hundred and thirty-three role-scoped operations"
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
                .replace("{resource}", "chair-1")
                .replace("{reservation}", "BK-1")
                // A number, because this one is parsed. Leaving it as the
                // template is how `assign_unit` was found answering a 400 that
                // was not `problem+json`.
                .replace("{line}", "0")
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
                .header("idempotency-key", idem("inv-1"))
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
        .schedule_next_visit(tenant, std::time::Duration::from_hours(1), false)
        .await
        .expect("defers");
    assert!(
        fixture
            .control
            .claim_tenants("w", 10, erp_control::WorkSchedule::default())
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
        .claim_tenants("w", 10, erp_control::WorkSchedule::default())
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
                .header("idempotency-key", idem("INV-1"))
                .body(Body::from(
                    serde_json::json!({
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
    // The key is the record's identity; the statutory number is ours, and its
    // series may have no holes in it.
    assert_eq!(issued["id"], idem("INV-1"), "the key comes back as sent");
    assert_eq!(issued["number"], "INV-00001", "{issued}");
    let position = issued["position"].as_i64().expect("a log position");

    // Read your own write. Without the worker running, this is what the hint is
    // for — so drive the projections first and then ask for exactly that point.
    fixture.project_sales(tenant).await;

    let (status, invoice, _) = fixture
        .send(
            bearer(Request::get(format!(
                "/v1/sales/invoices/{}?consistent_after={position}",
                idem("INV-1")
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

    payment_settles_the_receivable(&mut fixture, &token, tenant).await;
    fixture.cleanup().await;
}

/// The money arrives and the receivable clears.
///
/// Split out from the test above for the reason `drawn_down_every_way` is split
/// out of `prepaid`'s canary: it is a separate claim. The first half is that an
/// invoice posts to the books; this is that a payment settles it.
async fn payment_settles_the_receivable(fixture: &mut Fixture, token: &str, tenant: TenantId) {
    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    let (status, paid, _) = fixture
        .send(
            bearer(Request::post(format!(
                "/v1/sales/invoices/{}/payments",
                idem("INV-1")
            )))
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
    let invoices = invoices["items"].as_array().expect("a list");
    assert_eq!(invoices.len(), 1);
    assert_eq!(invoices[0]["outstanding"], 0);
    assert_eq!(invoices[0]["paid"], 165_000);

    assert_eq!(
        fixture.ledger_balance(token, "acme", "1100").await,
        0,
        "the receivable is settled"
    );
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
                .header("idempotency-key", idem("INV-9"))
                .body(Body::from(
                    serde_json::json!({
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
                .header("idempotency-key", idem("INV-8"))
                .body(Body::from(
                    serde_json::json!({
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
                .header("idempotency-key", idem("INV-LATE"))
                .body(Body::from(
                    serde_json::json!({
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
    assert_eq!(invoices["items"].as_array().expect("a list").len(), 1);

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
                .header("idempotency-key", idem("INV-KEEP"))
                .body(Body::from(
                    serde_json::json!({
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
        invoices["items"].as_array().expect("a list").len(),
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

/// **A module that needs at least one of several, over HTTP.**
///
/// `tax_sa` nets output tax against input tax and needs a source for one side or
/// the other. Requiring both would force a shop with no supplier bills to enable
/// `purchases`; requiring neither — which is what it did — let a tenant turn on
/// a VAT return with nothing feeding it.
#[tokio::test]
async fn a_module_needing_one_of_several_takes_either_and_refuses_neither() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let enable = |module: &str, locale: &str| {
        Request::post("/v1/modules")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT_LANGUAGE, locale)
            .body(Body::from(
                serde_json::json!({ "module": module }).to_string(),
            ))
            .unwrap()
    };

    // The ledger alone is not a VAT return. In Arabic, because the refusal is
    // something a Saudi accountant reads.
    let (status, body, _) = fixture.send(enable("tax_sa", "ar")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.module_requires_one_of");
    assert_eq!(body["args"]["required"]["value"], "sales, purchases");
    assert!(
        body["detail"]
            .as_str()
            .expect("a sentence")
            .contains("واحدة على الأقل"),
        "the refusal has to say *one of*, not read like the AND case: {body}"
    );

    // Either side is enough. Sales here; `one_of_several_is_enough_and_none_of_
    // them_is_not` covers purchases-only and both.
    let (status, body, _) = fixture.send(enable("sales", "en")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture.send(enable("tax_sa", "en")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // And the last side cannot be pulled out from under it.
    let (status, body, _) = fixture
        .send(
            Request::delete("/v1/modules/sales")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.module_in_use");
    assert_eq!(body["args"]["dependent"]["value"], "tax_sa");

    // With the other side on, it can: the return still has something to report.
    let (status, body, _) = fixture.send(enable("purchases", "en")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture
        .send(
            Request::delete("/v1/modules/sales")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "purchases still feeds the return: {body}"
    );

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

/// **Inviting somebody promises the email, in the same transaction.**
///
/// The outbox was built in Phase 2 and had **no producer anywhere in the
/// product** until this one: every piece of it — effects as values, claim under
/// `SKIP LOCKED`, backoff, dead letters, the at-least-once idempotency key —
/// was finished, tested, and reaching nothing. An invitation was a link
/// somebody copied out of this response by hand.
///
/// What this asserts is the part that cannot be checked by reading the code:
/// that the row is there after the request, in the caller's language, carrying
/// the link that actually works.
#[tokio::test]
async fn inviting_somebody_promises_them_an_email() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // In Arabic, because the invitee has no account and therefore no stored
    // language — what the *inviter* was reading is the only signal there is,
    // and it is gone by the time a worker picks the row up.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/invitations")
                .header(header::HOST, "acme.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::from(
                    serde_json::json!({ "handle": "sara@acme.test", "role": "clerk" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let link_token = body["token"].as_str().expect("a token").to_owned();

    let (kind, payload, key): (String, serde_json::Value, String) =
        sqlx::query_as("SELECT kind, payload, idempotency_key FROM outbox")
            .fetch_one(fixture.db.pool())
            .await
            .expect("the invitation promised nothing; the outbox is empty");

    assert_eq!(kind, "email.send");
    assert_eq!(payload["to"], "sara@acme.test");
    assert_eq!(
        payload["locale"], "arabic",
        "rendered in the inviter's language"
    );
    assert!(
        payload["subject"]
            .as_str()
            .is_some_and(|s| s.contains("دعوتك")),
        "the subject is not Arabic: {payload}"
    );

    // **The link in the email is the link that works.** A body with a
    // plausible-looking URL in it that 404s is worse than no email at all.
    let body_text = payload["body"].as_str().expect("a body");
    assert!(
        body_text.contains(&link_token),
        "the email does not carry the invitation's own token"
    );
    assert!(
        body_text.contains("https://acme.localhost/v1/join/"),
        "the link is not addressed to this tenant: {body_text}"
    );

    // The key ties the promise to the invitation, so a redelivery is one email
    // rather than a second invitation.
    assert!(key.starts_with("invitation:"), "{key}");

    // And the token in the email is honoured, which is the only proof the link
    // is real rather than well-formed.
    let (status, _, _) = fixture
        .send(
            Request::get(format!("/v1/join/{link_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the emailed link does not work");

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

    // Their own password, still, and still checked — at the *request*, because
    // by the time the link is opened there is no password left to check it
    // against.
    let (status, body) = fixture
        .signup(serde_json::json!({
            "slug": "second",
            "company": "Second Company",
            "email": "owner@acme.test",
            "password": "hunter2hunter2",
            "modules": []
        }))
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
    assert_eq!(members[0].role, erp_control::Role::Owner);

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
                .header("idempotency-key", idem("INV-CONFIGURED"))
                .body(Body::from(
                    serde_json::json!({
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
                .header("idempotency-key", idem("E-OOPS"))
                .body(Body::from(
                    serde_json::json!({
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
        bearer(Request::post(format!(
            "/v1/ledger/entries/{}/reversal",
            idem("E-OOPS")
        )))
        .header("idempotency-key", idem(id))
        .body(Body::from(
            serde_json::json!({
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
    assert_eq!(body["args"]["by"]["value"], idem("E-OOPS-R"));

    // Reversing something that was never posted is about the tenant's state,
    // not the request's shape.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/ledger/entries/NOPE/reversal"))
                .header("idempotency-key", idem("NOPE-R"))
                .body(Body::from(
                    serde_json::json!({ "occurred_on": "2026-03-05T00:00:00Z" }).to_string(),
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
        Request::post(format!(
            "/v1/sales/invoices/{}/credit-note",
            idem("INV-OOPS")
        ))
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
    let invoice = &invoices["items"].as_array().expect("a list")[0];
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
                .header("idempotency-key", idem("INV-VAT"))
                .body(Body::from(
                    serde_json::json!({
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

    let period = "from=2026-01-01&until=2026-04-01&currency=SAR";
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
                 ?from=2026-04-01&until=2026-01-01&currency=SAR",
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
        serde_json::to_value(erp_api::openapi()).expect("the document serializes")
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
                .header("idempotency-key", idem("crm-1"))
                .body(Body::from(
                    serde_json::json!({
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
                .header("idempotency-key", idem("ap-1"))
                .body(Body::from(
                    serde_json::json!({
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
                 ?from=2026-01-01&until=2026-04-01&currency=SAR",
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
                 ?from=2026-07-01&until=2026-10-01&currency=SAR",
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
                .header("idempotency-key", idem("crm-1"))
                .body(Body::from(
                    serde_json::json!({
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
                 ?from=2026-01-01&until=2026-04-01&currency=SAR",
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

    let catalogue = erp_api::modules();
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
            "industry": "Consulting",
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
            .header("idempotency-key", idem(id))
            .body(Body::from(
                serde_json::json!({
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
    let documents = documents["items"].as_array().expect("a list");
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
                    .header("idempotency-key", idem(id))
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

/// **ZATCA onboarding, over HTTP, with no network.**
///
/// The path a deployment falls back to when the automated one breaks, and the
/// one that works today: generate the key and the request here, take the request
/// to ZATCA with the taxpayer's OTP, bring back what it issues.
#[tokio::test]
async fn a_tenant_can_generate_a_signing_key_and_take_a_certificate_for_it() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.register_with_zatca(&token).await;
    // The certificate names the business the registration names, and that is
    // read from the projection — so it has to have caught up.
    fixture.project_tax(tenant).await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    // Nothing yet.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["live"], false);
    assert_eq!(body["reached"].as_array().expect("a list").len(), 0);

    let unit = |environment: &str| {
        serde_json::json!({
            "environment": environment,
            "branch": "الفرع الرئيسي",
            "common_name": "EGS1-886431145",
            "serial": "886431145",
            "industry": "Consulting"
        })
    };

    // An environment nobody has is refused before a key is generated for it.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(unit("staging").to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.unknown_zatca_environment");

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(unit("simulation").to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["compliance_documents"], 6, "both kinds were declared");
    assert!(
        body["submit_to"]
            .as_str()
            .is_some_and(|url| url.contains("simulation") && url.ends_with("/compliance")),
        "{body}"
    );

    // **The request is a real CSR**, on the curve ZATCA specifies, over a key
    // this system now holds and never sent.
    let csr = body["csr"].as_str().expect("a CSR");
    let pem = base64_decode(csr);
    let request = openssl::x509::X509Req::from_pem(&pem).expect("a certificate request");
    assert_eq!(
        request
            .public_key()
            .expect("a key")
            .ec_key()
            .expect("an EC key")
            .group()
            .curve_name(),
        Some(openssl::nid::Nid::SECP256K1)
    );

    fixture.cleanup().await;
}

/// **A certificate for somebody else's key is refused before it is stored**, and
/// the real one is taken. The first is a document every invoice would be
/// rejected on; the second is what makes the tenant able to issue at all.
#[tokio::test]
async fn a_certificate_is_checked_against_the_key_it_is_meant_for() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.register_with_zatca(&token).await;
    fixture.project_tax(tenant).await;

    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };

    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(
                    serde_json::json!({
                        "environment": "simulation",
                        "branch": "الفرع الرئيسي",
                        "common_name": "EGS1-886431145",
                        "serial": "886431145",
                        "industry": "Consulting"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request =
        openssl::x509::X509Req::from_pem(&base64_decode(body["csr"].as_str().expect("a CSR")))
            .expect("a certificate request");

    // A certificate for somebody else's key is refused, and nothing is stored.
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/zatca/onboarding/certificate"))
                .body(Body::from(
                    serde_json::json!({
                        "stage": "compliance",
                        "environment": "simulation",
                        "token": certificate_for_a_stranger(),
                        "secret": "the-csid-secret",
                        "request_id": "1234"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.certificate_key_mismatch");

    // The real one is taken.
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/zatca/onboarding/certificate"))
                .body(Body::from(
                    serde_json::json!({
                        "stage": "compliance",
                        "environment": "simulation",
                        "token": certificate_over(&request),
                        "secret": "the-csid-secret",
                        "request_id": "1234567890123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stage"], "compliance");
    // The common name is minted from the unit's serial, not the caller's — the
    // subject carries `CN=EGS-<serial>`.
    assert!(
        body["subject"]
            .as_str()
            .is_some_and(|s| s.contains("CN=EGS-")),
        "{body}"
    );

    // The status endpoint reads `proj_tax_sa.onboarding` rather than loading the
    // aggregate (L7), so the projection has to have run — which in production is
    // the worker and here is this call. `reached` comes from the sealed secrets
    // and would answer without it; `environment` would come back null, which is
    // exactly how this assertion caught the change.
    fixture.project_tax(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reached"], serde_json::json!(["compliance"]));
    assert_eq!(
        body["live"], false,
        "a compliance certificate does not clear real invoices"
    );
    assert_eq!(body["environment"], "simulation");

    fixture.cleanup().await;
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("base64")
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A certificate over the key in this request, the way ZATCA would issue one.
fn certificate_over(request: &openssl::x509::X509Req) -> String {
    base64_encode(&sign_certificate(
        request.subject_name(),
        &request.public_key().expect("a key"),
    ))
}

/// A certificate over a key nobody here holds.
fn certificate_for_a_stranger() -> String {
    let group =
        openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP256K1).expect("secp256k1");
    let key = openssl::ec::EcKey::generate(&group).expect("a key");
    let key = openssl::pkey::PKey::from_ec_key(key).expect("a key");
    // Only the public half goes in a certificate.
    let key = openssl::pkey::PKey::public_key_from_pem(&key.public_key_to_pem().expect("pem"))
        .expect("a public key");

    let mut name = openssl::x509::X509NameBuilder::new().expect("a builder");
    name.append_entry_by_text("CN", "somebody else")
        .expect("a name");
    let name = name.build();

    base64_encode(&sign_certificate(&name, &key))
}

fn sign_certificate(
    subject: &openssl::x509::X509NameRef,
    public: &openssl::pkey::PKey<openssl::pkey::Public>,
) -> Vec<u8> {
    let group =
        openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP256K1).expect("secp256k1");
    let ca = openssl::ec::EcKey::generate(&group).expect("a CA key");
    let ca = openssl::pkey::PKey::from_ec_key(ca).expect("a CA key");

    let mut certificate = openssl::x509::X509::builder().expect("a builder");
    certificate.set_version(2).expect("v3");
    certificate.set_subject_name(subject).expect("subject");
    certificate.set_issuer_name(subject).expect("issuer");
    certificate.set_pubkey(public).expect("public key");
    certificate
        .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).expect("now"))
        .expect("not before");
    certificate
        .set_not_after(&openssl::asn1::Asn1Time::days_from_now(1826).expect("five years"))
        .expect("not after");
    let serial = openssl::bn::BigNum::from_u32(0x0BAD_CAFE)
        .and_then(|bn| openssl::asn1::Asn1Integer::from_bn(&bn))
        .expect("a serial");
    certificate.set_serial_number(&serial).expect("serial");
    certificate
        .sign(&ca, openssl::hash::MessageDigest::sha256())
        .expect("signs");
    certificate.build().to_pem().expect("pem")
}

/// A ZATCA that issues whatever it is shown, for driving the worker's half of
/// onboarding without a network. The route's half makes one real call and is
/// covered by the module tests with the same kind of fake.
#[derive(Debug, Default)]
struct IssuingZatca {
    checked: std::sync::Mutex<usize>,
}

fn issued_over(
    subject: &openssl::x509::X509NameRef,
    key: &openssl::pkey::PKey<openssl::pkey::Public>,
    request_id: &str,
) -> tax_sa::zatca::onboarding::CsidResponse {
    tax_sa::zatca::onboarding::CsidResponse {
        request_id: Some(serde_json::json!(request_id)),
        disposition: Some("ISSUED".to_owned()),
        token: Some(base64_encode(&sign_certificate(subject, key))),
        secret: Some("the-csid-secret".to_owned()),
        errors: None,
    }
}

#[async_trait::async_trait]
impl tax_sa::zatca::onboarding::Registrar for IssuingZatca {
    async fn compliance_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _otp: &tax_sa::zatca::onboarding::Otp,
        request: &tax_sa::zatca::onboarding::ComplianceRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        let csr = openssl::x509::X509Req::from_pem(&base64_decode(&request.csr)).expect("a CSR");
        Ok(issued_over(
            csr.subject_name(),
            &csr.public_key().expect("a key"),
            "compliance-1",
        ))
    }

    async fn check_compliance(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _compliance: &tax_sa::zatca::onboarding::Csid,
        _submission: &tax_sa::zatca::wire::Submission,
    ) -> Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered> {
        *self.checked.lock().expect("not poisoned") += 1;
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        })
    }

    async fn production_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        compliance: &tax_sa::zatca::onboarding::Csid,
        _request: &tax_sa::zatca::onboarding::ProductionRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        // Over the same key: the production certificate replaces the compliance
        // one for the unit that earned it.
        let certificate = compliance.certificate().expect("a certificate");
        Ok(issued_over(
            certificate.subject_name(),
            &certificate.public_key().expect("a key"),
            "production-1",
        ))
    }

    async fn renew_csid(
        &self,
        _environment: tax_sa::zatca::csr::Environment,
        _production: &tax_sa::zatca::onboarding::Csid,
        _otp: &tax_sa::zatca::onboarding::Otp,
        _request: &tax_sa::zatca::onboarding::ComplianceRequest,
    ) -> Result<tax_sa::zatca::onboarding::CsidResponse, tax_sa::zatca::wire::Unanswered> {
        unreachable!("nothing is renewed in these tests")
    }
}

/// **One OTP, over HTTP, and the status says where it stands.** The
/// registration carries the industry; the route derives the unit and stops at
/// the compliance certificate; the worker finishes; asking again is refused
/// before a key is touched.
#[expect(
    clippy::too_many_lines,
    reason = "one story, told once, from registration to live"
)]
#[tokio::test]
async fn a_tenant_goes_live_from_one_otp_and_the_status_says_so() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");

    // A registration from before the industry existed: no certificate can be
    // asked for until it is added, and the answer says so.
    tax_sa::register_taxpayer(
        &db,
        tax_sa::Registration {
            vat_number: "310122393500003".to_owned(),
            name: "أكمي للتجارة".to_owned(),
            name_latin: None,
            scheme: tax_sa::IdScheme::Crn,
            identifier: "1010101010".to_owned(),
            address: tax_sa::Address {
                street: "طريق الملك فهد".to_owned(),
                building: "2322".to_owned(),
                additional: None,
                district: "العليا".to_owned(),
                city: "الرياض".to_owned(),
                postal_code: "12211".to_owned(),
                country: "SA".to_owned(),
            },
            industry: None,
        },
        chrono::Utc::now(),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("registers");
    fixture.project_tax(tenant).await;
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(r#"{"environment":"simulation"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "tax_sa.no_industry");

    // With the industry, and six digits or nothing before anything is generated.
    fixture.register_with_zatca(&token).await;
    fixture.project_tax(tenant).await;
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding/activate"))
                .body(Body::from(r#"{"environment":"simulation","otp":"12"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.not_an_otp");

    // The route's half, by hand — its one network call is a fake's job in the
    // module tests. The unit is derived: the registered name is the O and, with
    // no branch given, the OU; the common name is minted.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding"))
                .body(Body::from(r#"{"environment":"simulation"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["compliance_documents"], 6);
    let request =
        openssl::x509::X509Req::from_pem(&base64_decode(body["csr"].as_str().expect("a CSR")))
            .expect("a certificate request");
    let subject: Vec<String> = request
        .subject_name()
        .entries()
        .map(|e| {
            format!(
                "{}={}",
                e.object().nid().short_name().expect("a name"),
                e.data().to_string().expect("utf8")
            )
        })
        .collect();
    assert!(
        subject.contains(&"O=أكمي للتجارة".to_owned()),
        "{subject:?}"
    );
    assert!(
        subject.contains(&"OU=أكمي للتجارة".to_owned()),
        "{subject:?}"
    );
    assert!(
        subject.iter().any(|e| e.starts_with("CN=EGS-")),
        "{subject:?}"
    );
    let (status, body, _) = fixture
        .send(
            bearer(Request::put("/v1/tax_sa/zatca/onboarding/certificate"))
                .body(Body::from(
                    serde_json::json!({
                        "stage": "compliance",
                        "environment": "simulation",
                        "token": certificate_over(&request),
                        "secret": "the-csid-secret",
                        "request_id": "compliance-1"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    fixture.project_tax(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "checking");
    assert_eq!(body["live"], false);
    assert!(body["checks"].is_null());
    assert!(body["refusal"].is_null());

    // The worker's half.
    let zatca = IssuingZatca::default();
    let finished = tax_sa::zatca::finish::finish(
        &db,
        &erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes"),
        &zatca,
        chrono::Utc::now(),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("finishes");
    assert_eq!(
        finished.checks.as_ref().map(|c| (c.submitted, c.passed)),
        Some((6, 6))
    );
    assert!(finished.production.is_some());
    fixture.project_tax(tenant).await;

    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/tax_sa/zatca/onboarding"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "live");
    assert_eq!(body["live"], true);
    assert_eq!(body["checks"]["submitted"], 6);
    assert!(!body["checks"]["passed_at"].is_null());
    assert!(body["refusal"].is_null());
    assert_eq!(
        body["reached"],
        serde_json::json!(["compliance", "production"])
    );

    // Live: asking again is refused before any key is touched — and before
    // ZATCA is called, which is why this test can ask.
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/tax_sa/zatca/onboarding/activate"))
                .body(Body::from(r#"{"environment":"simulation","otp":"123456"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "tax_sa.already_live");

    fixture.cleanup().await;
}

/// **A deployment with no sealing key refuses rather than storing a key in the
/// clear.** Law L6: failures stop, they do not degrade.
#[tokio::test]
async fn no_sealing_key_refuses_rather_than_storing_one_in_the_clear() {
    let mut fixture = Fixture::new().await;
    // The same control plane, served by a router that was given no sealing key.
    fixture.app = router(AppState::new(Arc::clone(&fixture.control)));

    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/tax_sa/zatca/onboarding")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "environment": "sandbox",
                        "branch": "الفرع الرئيسي",
                        "common_name": "EGS1",
                        "serial": "1",
                        "industry": "Consulting"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "request.no_sealing_key");

    // And nothing was written, so there is no half-generated key to trip over.
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    let secrets: i64 = sqlx::query_scalar("SELECT count(*) FROM module_secret")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    drop(conn);
    drop(db);
    assert_eq!(secrets, 0);

    fixture.cleanup().await;
}

/// **Paging returns every row, and says when it has run out.**
///
/// Before this, a list took a limit and returned that many — a tenant with more
/// invoices than fit saw a prefix and a response that looked complete. This
/// walks the cursors to the end and counts.
#[tokio::test]
async fn a_list_longer_than_one_page_can_be_read_to_the_end() {
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

    // Five invoices, some sharing a tax point so the cursor's second part is
    // what separates them — the case a timestamp-only cursor would skip.
    for n in 1..=5 {
        let day = if n <= 2 { 10 } else { 11 };
        let (status, body, _) = fixture
            .send(
                bearer(Request::post("/v1/sales/invoices"))
                    .header("idempotency-key", idem(&format!("inv-{n}")))
                    .body(Body::from(
                        serde_json::json!({
                            "customer": { "name": "زبون" },
                            "issued_on": format!("2026-02-{day:02}T00:00:00Z"),
                            "currency": "SAR",
                            "lines": [
                                { "description": "خدمة", "net": 1_000 * n, "vat": "standard" }
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    fixture.project_sales(tenant).await;

    // Walk it two at a time.
    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let path = match &after {
            Some(cursor) => format!("/v1/sales/invoices?limit=2&after={cursor}"),
            None => "/v1/sales/invoices?limit=2".to_owned(),
        };
        let (status, body, _) = fixture
            .send(bearer(Request::get(&path)).body(Body::empty()).unwrap())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        for invoice in body["items"].as_array().expect("items") {
            let number = invoice["number"].as_str().expect("a number").to_owned();
            assert!(!seen.contains(&number), "{number} came back twice");
            seen.push(number);
        }

        match body["next"].as_str() {
            Some(cursor) => after = Some(cursor.to_owned()),
            // **Absent means the list ended**, which is the statement the old
            // shape could not make.
            None => break,
        }
    }

    assert_eq!(seen.len(), 5, "paging lost or repeated rows: {seen:?}");

    // **Newest tax point first, and every row exactly once.**
    //
    // Within one tax point the order is the cursor's second part, which is the
    // id — and an id is a UUID now, so it is stable but no longer creation
    // order. That is what the cursor actually promises: rows sharing a
    // timestamp are separated so none is skipped or repeated. Asserting the
    // exact sequence within a day would be asserting an accident of how the
    // client used to number its own keys.
    let (later, earlier) = seen.split_at(3);
    let mut later = later.to_vec();
    let mut earlier = earlier.to_vec();
    later.sort();
    earlier.sort();
    assert_eq!(
        later,
        vec![
            "INV-00003".to_owned(),
            "INV-00004".to_owned(),
            "INV-00005".to_owned()
        ],
        "the three sharing the later tax point came first"
    );
    assert_eq!(
        earlier,
        vec!["INV-00001".to_owned(), "INV-00002".to_owned()],
        "the two sharing the earlier tax point came last"
    );

    // A cursor from somewhere else is refused rather than silently starting
    // over, which would look like the list restarting.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get("/v1/sales/invoices?after=not-a-cursor"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.invalid_cursor");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Phase 17 — the public surface
// ---------------------------------------------------------------------------

/// **A stranger can see what a business offers, and nothing else.**
///
/// The whole safety argument for `erp_web::Public` in one test: the same caller,
/// on the same host, with no credential, reaches the two public routes and is
/// refused by every other one. It is refused by *authentication*, not by a
/// capability — which is the stronger answer, because it means a public route
/// added tomorrow that forgot to be public still cannot be reached without a
/// token.
#[tokio::test]
async fn a_stranger_sees_what_is_offered_and_can_reach_nothing_else() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // The business records two chairs and takes one out of service.
    for (id, name) in [("CHAIR-1", "كرسي ١"), ("CHAIR-2", "كرسي ٢")] {
        let (status, body, _) = fixture
            .send(
                Request::post("/v1/booking/resources")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", idem(id))
                    .body(Body::from(
                        serde_json::json!({
                            "id": id, "name": name, "kind": "person", "capacity": 1
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/booking/resources/CHAIR-2/withdrawal")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "why": "مكسور" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The public surface takes no `consistent_after`: a customer has no write
    // position to pass, and a site reading a diary a moment behind is correct.
    // So the test drives the projection the way the worker would.
    fixture.project_booking(tenant).await;

    // A stranger. No token, no account, nothing.
    let (status, body, _) = fixture
        .send(
            get("/v1/booking/public/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let offered: Vec<&str> = body["items"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(
        offered,
        vec!["CHAIR-1"],
        "a broken chair was offered to a customer"
    );

    // And what a customer is shown is narrower than what a member is shown.
    let first = &body["items"][0];
    assert!(
        first.get("capacity").is_none() && first.get("withdrawn_why").is_none(),
        "the public shape leaked a member's fields: {first}"
    );

    // The same stranger, on the same host, against everything else.
    for path in [
        "/v1/booking/resources",
        "/v1/booking/reservations",
        "/v1/sales/invoices",
        "/v1/ledger/accounts",
        "/v1/members",
    ] {
        let (status, body, _) = fixture.send(get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} answered a stranger: {body}"
        );
    }

    fixture.cleanup().await;
}

/// Availability is a number a form checks, and a service nobody declared is a
/// 404 rather than a zero — a stale link has to be distinguishable from a full
/// diary.
#[tokio::test]
async fn a_stranger_can_ask_whether_a_slot_is_free() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/booking/resources")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem("CHAIR-1"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 2
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let window = "from=2026-05-01T09:00:00Z&until=2026-05-01T10:00:00Z";
    let (status, body, _) = fixture
        .send(
            get(&format!(
                "/v1/booking/public/availability?resource=CHAIR-1&{window}"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["free"], 2, "an empty diary is entirely free");

    // A link from last year, to a chair that never existed.
    let (status, body, _) = fixture
        .send(
            get(&format!(
                "/v1/booking/public/availability?resource=CHAIR-NONE&{window}"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a stale link looked like a full diary: {body}"
    );

    fixture.cleanup().await;
}

/// A business that does not take bookings has no public booking page, and says
/// so with the same 404 as a business that does not exist.
#[tokio::test]
async fn a_business_without_the_module_has_no_public_page() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_ledger(tenant).await;

    for path in [
        "/v1/booking/public/services",
        "/v1/booking/public/availability?resource=X&from=2026-05-01T09:00:00Z&until=2026-05-01T10:00:00Z",
    ] {
        let (status, body, _) = fixture.send(get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
        assert_eq!(body["code"], "request.module_not_enabled");
    }

    // And a business nobody has heard of.
    let (status, _, _) = fixture
        .send(
            get("/v1/booking/public/services")
                .header(header::HOST, "nobody.localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    fixture.cleanup().await;
}

/// **A browser at an allowed origin is answered; one at a lookalike is not.**
///
/// The lookalike is the point. `https://salon.com.attacker.example` ends with
/// `salon.com`, so any check written with `ends_with` admits it — and the page
/// that gets in can read a tenant's diary with a visitor's browser.
#[tokio::test]
async fn a_verified_origin_is_answered_and_a_lookalike_is_not() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;

    let token = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");
    fixture.prove("salon.example", &token);
    // Proof first: an origin is licensed under a proved domain and not before.
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("verifies");
    fixture
        .control
        .allow_origin(
            tenant,
            "salon.example",
            "https://salon.example",
            Actor::system(),
        )
        .await
        .expect("licenses");

    // Now the real request carries the header back.
    let response = fixture
        .raw(
            get("/v1/booking/public/services")
                .header(header::ORIGIN, "https://salon.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://salon.example"),
    );
    assert_eq!(
        response.headers().get("vary").and_then(|v| v.to_str().ok()),
        Some("Origin"),
        "a shared cache could serve one origin's response to another"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "the tenant's own app, on a proved origin, calls the API with its session"
    );

    // The lookalike, and a bare different origin.
    for origin in [
        "https://salon.example.attacker.test",
        "https://attacker.test",
        "http://salon.example",
    ] {
        let response = fixture
            .raw(
                get("/v1/booking/public/services")
                    .header(header::ORIGIN, origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "{origin} was let in"
        );
    }

    fixture.cleanup().await;
}

/// A preflight is answered without reaching a handler, and a refused one says
/// nothing about what the allowlist contains.
#[tokio::test]
async fn a_preflight_is_answered_at_the_edge() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");
    fixture.prove("salon.example", &token);
    // Proof first: an origin is licensed under a proved domain and not before.
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("verifies");
    fixture
        .control
        .allow_origin(
            tenant,
            "salon.example",
            "https://salon.example",
            Actor::system(),
        )
        .await
        .expect("licenses");

    let preflight = |origin: &str| {
        Request::options("/v1/booking/public/services")
            .header(header::HOST, "acme.localhost")
            .header(header::ORIGIN, origin)
            .header("access-control-request-method", "GET")
            .body(Body::empty())
            .unwrap()
    };

    let response = fixture.raw(preflight("https://salon.example")).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://salon.example"),
    );
    let allowed_headers = response
        .headers()
        .get("access-control-allow-headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        allowed_headers.contains("authorization") && allowed_headers.contains("if-match"),
        "a proved origin is the tenant's own app, and it carries a session: {allowed_headers}"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok()),
        Some("GET, POST, PUT, PATCH, DELETE"),
    );

    // Refused: still a 204, still no explanation.
    let response = fixture.raw(preflight("https://attacker.test")).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "a page not on the list was told it was"
    );

    fixture.cleanup().await;
}

/// Revoking an origin takes effect at once on the node that did it, rather than
/// after the entry cache's TTL — the same promise logging out already makes.
#[tokio::test]
async fn revoking_an_origin_takes_effect_at_once() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;

    let token = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");
    fixture.prove("salon.example", &token);
    // Proof first: an origin is licensed under a proved domain and not before.
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("verifies");
    fixture
        .control
        .allow_origin(
            tenant,
            "salon.example",
            "https://salon.example",
            Actor::system(),
        )
        .await
        .expect("licenses");
    assert!(
        fixture
            .control
            .allows_origin(tenant, "https://salon.example")
            .await
            .expect("asks"),
        "a verified origin was refused"
    );

    fixture
        .control
        .revoke_origin(tenant, "https://salon.example", Actor::system())
        .await
        .expect("revokes");

    assert!(
        !fixture
            .control
            .allows_origin(tenant, "https://salon.example")
            .await
            .expect("asks"),
        "a revoked origin was still cached"
    );

    fixture.cleanup().await;
}

/// **The public surface is bounded, and a business's own staff are not.**
///
/// The bound has to hold without a session to attribute abuse to, and it must
/// not be the thing that takes the shop offline: a booking form under attack
/// stops answering strangers, and the people at the counter keep working.
#[tokio::test]
async fn a_flood_at_the_booking_page_does_not_close_the_counter() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Hammer the public page until it says no.
    let mut refused = None;
    for _ in 0..800 {
        let (status, body, _) = fixture
            .send(
                get("/v1/booking/public/services")
                    .header(header::ORIGIN, "https://flood.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(body);
            break;
        }
    }

    let body = refused.expect("the public surface answered 800 requests unbounded");
    assert_eq!(body["code"], "request.too_many_requests");
    assert!(
        body["args"]["seconds"]["value"].as_i64().unwrap_or(0) > 0,
        "a caller was refused without being told when to come back: {body}"
    );

    // The counter is unaffected: a member with a session goes through the
    // authenticated path, which this limiter never sees.
    let (status, body, _) = fixture
        .send(
            get("/v1/booking/resources")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a flood at the public page closed the shop: {body}"
    );

    fixture.cleanup().await;
}

/// **Online booking is off until a business turns it on**, and what it takes
/// when it is on is a request rather than a promise.
#[tokio::test]
async fn a_stranger_cannot_book_until_the_business_opens_the_diary() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/booking/resources")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem("CHAIR-1"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let booking = || {
        Request::post("/v1/booking/public/reservations")
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idem("PUBLIC-BOOKING-1"))
            .body(Body::from(
                serde_json::json!({
                    "customer_name": "سارة",
                    "customer_phone": "+966500000000",
                    "lines": [{
                        "resource": "CHAIR-1",
                        "from": "2026-05-01T09:00:00Z",
                        "until": "2026-05-01T10:00:00Z"
                    }]
                })
                .to_string(),
            ))
            .unwrap()
    };

    // Closed by default — and a 404, which does not confirm that it would work
    // for somebody else.
    let (status, body, _) = fixture.send(booking()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a stranger booked a diary nobody opened: {body}"
    );

    // The business opens it.
    {
        let db = fixture
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            booking::PublicBooking::KEY,
            &booking::PublicBooking {
                verify_phone: false,
                hold_minutes: 0,
                open: true,
                deposit_bp: 2_000,
            },
            None,
            None,
        )
        .await
        .expect("stores the setting");
    }

    let (status, body, _) = fixture.send(booking()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["stage"], "reserved",
        "a public booking promised something the business had not agreed to"
    );
    assert_eq!(
        body["deposit_bp"], 2_000,
        "the site was not told what will be asked for"
    );

    // A retry of the same submit — a phone that lost signal — books once.
    let (status, again, _) = fixture.send(booking()).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(again["id"], body["id"], "a retry booked a second slot");

    // And the slot is genuinely held: the chair takes one at a time.
    let (status, free, _) = fixture
        .send(
            get("/v1/booking/public/availability?resource=CHAIR-1&from=2026-05-01T09:00:00Z&until=2026-05-01T10:00:00Z")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{free}");
    assert_eq!(free["free"], 0, "the booking did not hold the slot");

    fixture.cleanup().await;
}

/// **A stranger pays a deposit through a lender, end to end over HTTP.**
///
/// The service carries a published price, so the public booking is priced and
/// a deposit is asked for; the deposit is requested through Tabby with what a
/// lender needs; the worker opens the checkout; and the read beside the route
/// hands the waiting customer the page to go to. Every way the request can
/// fall short of what the lender needs is refused by name, and a landing page
/// off the business's own site is refused as such.
#[expect(
    clippy::too_many_lines,
    reason = "one deposit's whole life through a lender — published, booked, refused four \
              ways, requested, opened, read back — and splitting it would mean five fixtures \
              for one story"
)]
#[tokio::test]
async fn a_public_deposit_is_paid_through_a_lender_at_the_published_price() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    fixture.enable_module(tenant, sales::setup()).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    fixture.enable_module(tenant, payments::setup()).await;
    fixture.enable_module(tenant, branches::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let post = |path: &str, key: &str, body: serde_json::Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idem(key))
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // The business takes Tabby, has a branch with an address, and publishes
    // what the chair costs to book.
    let (status, body, _) = fixture
        .send(
            Request::put("/v1/payments/gateways/tabby")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "provider": "tabby", "secret": "sk_test_x",
                                        "merchant_code": "bassat" })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, branch, _) = fixture
        .send(post(
            "/v1/branches",
            "OLAYA",
            serde_json::json!({
                "name": "العليا",
                "address": { "street": "King Fahd Road", "building": "12",
                             "city": "Riyadh", "postal_code": "12211", "country": "SA" }
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    let branch = branch["id"].as_str().expect("a branch id").to_owned();
    let (status, body, _) = fixture
        .send(post(
            "/v1/booking/resources",
            "CHAIR-1",
            serde_json::json!({
                "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1,
                "branch": branch,
                "rate": { "amount": 20_000, "currency": "SAR" }
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Online booking is on, with a fifth down and half an hour to pay it.
    {
        let db = fixture
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            booking::PublicBooking::KEY,
            &booking::PublicBooking {
                verify_phone: false,
                hold_minutes: 30,
                open: true,
                deposit_bp: 2_000,
            },
            None,
            None,
        )
        .await
        .expect("stores the setting");
    }

    // The business's own site is where a lender may send the customer back.
    let claim = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");
    fixture.prove("salon.example", &claim);
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("proved");
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/origins")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "domain": "salon.example", "origin": "https://salon.example" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    fixture
        .project::<booking::Booking>(tenant, &booking::projections(), booking::upcasters())
        .await;
    // The branch's address is read from its own read model.
    fixture
        .project::<branches::Branches>(tenant, &branches::projections(), branches::upcasters())
        .await;

    // **The public page shows the price**, and a booking is priced at it.
    let (status, services, _) = fixture
        .send(
            get("/v1/booking/public/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{services}");
    assert_eq!(services["items"][0]["rate"]["amount"], 20_000, "{services}");

    let (status, booked, _) = fixture
        .send(
            Request::post("/v1/booking/public/reservations")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem("PUBLIC-BOOKING-1"))
                .body(Body::from(
                    serde_json::json!({
                        "customer_name": "سارة",
                        "customer_phone": "+966500000000",
                        "lines": [{
                            "resource": "CHAIR-1", "what": "قص",
                            "from": "2026-05-01T09:00:00Z",
                            "until": "2026-05-01T10:00:00Z"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    let reservation = booked["id"].as_str().expect("an id").to_owned();
    fixture
        .project::<booking::Booking>(tenant, &booking::projections(), booking::upcasters())
        .await;

    let deposit = |key: &str, body: Option<serde_json::Value>| {
        let mut request = Request::post(format!(
            "/v1/booking/public/reservations/{reservation}/deposit"
        ))
        .header("Idempotency-Key", key.to_owned());
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        request
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap()
    };
    let key = "5d1d2f1e-6b3f-4b7e-9a1e-0c1d2e3f4a5b";
    let return_to = serde_json::json!({
        "success": "https://salon.example/booked",
        "cancel": "https://salon.example/cancelled",
        "failure": "https://salon.example/declined"
    });

    // **Every way the request falls short is refused by name.**
    for (body, code) in [
        (
            serde_json::json!({ "provider": "stripe" }),
            "payments.provider_not_offered",
        ),
        // Known, and not configured by this business.
        (
            serde_json::json!({ "provider": "tamara" }),
            "payments.provider_not_offered",
        ),
        (
            serde_json::json!({ "provider": "tabby", "return_to": return_to }),
            "payments.lender_needs",
        ),
        (
            serde_json::json!({ "provider": "tabby", "email": "sara@example.com",
                                "return_to": { "success": "https://evil.example/booked",
                                               "cancel": "https://salon.example/c",
                                               "failure": "https://salon.example/f" } }),
            "payments.landing_not_allowed",
        ),
    ] {
        let (status, answer, _) = fixture.send(deposit(key, Some(body.clone()))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
        assert_eq!(answer["code"], code, "{body}: {answer}");
    }

    // **The charge, at a fifth of the published price plus tax.**
    let (status, due, _) = fixture
        .send(deposit(
            key,
            Some(serde_json::json!({
                "provider": "tabby", "email": "sara@example.com", "return_to": return_to
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{due}");
    assert_eq!(due["payment"], key);
    assert_eq!(due["provider"], "tabby");
    assert_eq!(due["net"], 4_000, "a fifth of 200.00: {due}");
    assert_eq!(due["tax"], 600, "at fifteen per cent: {due}");
    assert_eq!(due["amount"], 4_600);
    assert!(
        due["pay_at"].is_null(),
        "nowhere to go until the worker has opened it"
    );

    fixture
        .project::<payments::Payments>(tenant, &payments::projections(), payments::upcasters())
        .await;
    let status_of = || {
        get(&format!(
            "/v1/booking/public/reservations/{reservation}/deposit"
        ))
        .body(Body::empty())
        .unwrap()
    };
    let (status, waiting, _) = fixture.send(status_of()).await;
    assert_eq!(status, StatusCode::OK, "{waiting}");
    assert_eq!(waiting["stage"], "requested");
    assert!(waiting["pay_at"].is_null());
    assert_eq!(waiting["paid"], false);
    assert!(
        !waiting["due_by"].is_null(),
        "the hold has a deadline: {waiting}"
    );

    // **The worker opens the checkout, and the lender is told the truth.**
    let lender = RecordingLender::default();
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let opened = payments::open_checkouts(
        &db,
        &lender,
        chrono::Utc::now(),
        25,
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("the checkout pass runs");
    assert_eq!(opened.started, 1, "{opened:?}");
    let told = lender.told.lock().expect("not poisoned").clone();
    assert_eq!(told.len(), 1);
    let charge = &told[0];
    assert_eq!(charge.reference, key);
    assert_eq!(charge.amount.minor(), 4_600);
    assert_eq!(charge.returns.success, "https://salon.example/booked");
    assert_eq!(
        charge.returns.notification.as_deref(),
        Some("https://acme.localhost/v1/hooks/tabby"),
        "the hook, on the business's own host"
    );
    let buyer = charge.buyer.as_ref().expect("a buyer");
    assert_eq!(buyer.email, "sara@example.com");
    assert_eq!(buyer.phone, "+966500000000", "the booking's own number");
    let basket = charge.basket.as_ref().expect("a basket");
    assert_eq!(basket.deliver_to.city, "Riyadh", "the branch's address");
    assert_eq!(basket.deliver_to.line, "King Fahd Road 12");
    assert_eq!(basket.tax.minor(), 600);
    assert_eq!(basket.items[0].category, "Services");

    fixture
        .project::<payments::Payments>(tenant, &payments::projections(), payments::upcasters())
        .await;
    let (status, ready, _) = fixture.send(status_of()).await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["stage"], "pending");
    assert_eq!(ready["pay_at"], "https://checkout.tabby.ai/s/1");
    assert_eq!(ready["provider"], "tabby");

    // A reload asks with a fresh key and is told the same charge, page and all.
    let (status, again, _) = fixture
        .send(deposit(
            "0f0e0d0c-0b0a-4c9d-8e7f-6a5b4c3d2e1f",
            Some(serde_json::json!({
                "provider": "tabby", "email": "sara@example.com", "return_to": return_to
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(again["payment"], key, "a second tab got a second charge");
    assert_eq!(again["pay_at"], "https://checkout.tabby.ai/s/1");

    fixture.cleanup().await;
}

/// **A completed booking is billed with its deposit deducted, end to end.**
///
/// The deposit settled and raised its prepayment invoice; the service is
/// delivered; the desk asks for the invoice and gets a final invoice that
/// charges the rest, names the prepayment invoice, and — as ZATCA wants it —
/// shows the whole supply with the prepayment deducted. Asking again gets the
/// same invoice. And a business that asks for it on completion gets it from
/// the worker's pass without asking at all.
#[expect(
    clippy::too_many_lines,
    reason = "one booking's whole life through to its final invoice, and the automatic \
              path beside it; splitting it would mean two fixtures for one story"
)]
#[tokio::test]
async fn a_completed_booking_is_billed_with_its_deposit_deducted() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_sales(tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    fixture.enable_module(tenant, payments::setup()).await;
    fixture.enable_module(tenant, tax_sa::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&token, "acme", "services").await;

    let post = |path: String, key: &str, body: serde_json::Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idem(key))
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let at = |day: &str| -> erp_types::Timestamp {
        format!("{day}T09:00:00Z").parse().expect("an instant")
    };

    // Registered with ZATCA, so the documents render.
    tax_sa::register_taxpayer(
        &db,
        tax_sa::Registration {
            vat_number: "310122393500003".to_owned(),
            name: "صالون الأمل".to_owned(),
            name_latin: None,
            scheme: tax_sa::IdScheme::Crn,
            identifier: "1010101010".to_owned(),
            address: tax_sa::Address {
                street: "طريق الملك فهد".to_owned(),
                building: "1234".to_owned(),
                additional: None,
                district: "العليا".to_owned(),
                city: "الرياض".to_owned(),
                postal_code: "12211".to_owned(),
                country: "SA".to_owned(),
            },
            industry: Some("Beauty".to_owned()),
        },
        at("2026-01-01"),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("registers");

    let (status, body, _) = fixture
        .send(post(
            "/v1/booking/resources".to_owned(),
            "CHAIR-1",
            serde_json::json!({ "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1 }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A booking priced at 200, taken at the desk.
    let book = |key: &str, day: &str| {
        post(
            "/v1/booking/reservations".to_owned(),
            key,
            serde_json::json!({
                "customer_name": "سارة", "customer_phone": "+966500000000",
                "lines": [{
                    "what": "صبغة", "from": format!("{day}T09:00:00Z"), "until": format!("{day}T10:00:00Z"),
                    "takes": [{ "resource": "CHAIR-1" }],
                    "charge": { "rate": 20_000, "currency": "SAR", "quantity": 1 }
                }]
            }),
        )
    };
    let (status, booked, _) = fixture.send(book("BOOKING-1", "2026-05-01")).await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    let reservation =
        erp_types::AggregateId::new(booked["id"].as_str().expect("an id")).expect("an id");

    // A fifth down, paid: the worker's join, done here by hand.
    let payment =
        erp_types::AggregateId::new("5d1d2f1e-6b3f-4b7e-9a1e-0c1d2e3f4a5b").expect("an id");
    {
        let sar = erp_types::CurrencyCode::new("SAR").expect("a currency");
        let mut tx = db.begin().await.expect("transaction");
        payments::start_in(
            &mut tx,
            &payment,
            &payments::Attempt {
                pay_at: None,
                provider: "moyasar".to_owned(),
                gateway_id: payment.to_string(),
                collects: payments::Collects::Advance(payments::Advance {
                    against: reservation.clone(),
                    net: erp_types::Money::from_minor(4_000, sar),
                    buyer: payments::Buyer {
                        name: "سارة".to_owned(),
                        vat_number: None,
                    },
                }),
                amount: erp_types::Money::from_minor(4_600, sar),
            },
            at("2026-04-20"),
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("starts");
        payments::settle_in(
            &mut tx,
            &payment,
            &erp_payments::Charged {
                id: payment.to_string(),
                status: erp_payments::Status::Paid,
                amount: erp_types::Money::from_minor(4_600, sar),
                refunded: erp_types::Money::from_minor(0, sar),
                fee: None,
                challenge: None,
                message: None,
            },
            at("2026-04-20"),
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("settles, raising the prepayment invoice");
        booking::secure_in(
            &mut tx,
            &reservation,
            &payment,
            at("2026-04-20"),
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("secures");
        tx.commit().await.expect("commits");
    }

    // The service is delivered.
    for stage in ["confirmed", "arrived", "in_service", "completed"] {
        let (status, body, _) = fixture
            .send(
                Request::post(format!("/v1/booking/reservations/{reservation}/stage"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "stage": stage }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{stage}: {body}");
    }
    fixture
        .project::<booking::Booking>(tenant, &booking::projections(), booking::upcasters())
        .await;
    fixture
        .project::<sales::Sales>(tenant, &sales::projections(), sales::upcasters())
        .await;

    // **The desk asks for the invoice.**
    let bill = || {
        Request::post(format!("/v1/booking/reservations/{reservation}/invoice"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, billed, _) = fixture.send(bill()).await;
    assert_eq!(status, StatusCode::CREATED, "{billed}");
    assert_eq!(billed["invoice"], format!("bk-{reservation}"));
    let number = billed["number"].as_str().expect("a number").to_owned();
    let deducted = billed["deducted"]
        .as_str()
        .expect("the prepayment invoice")
        .to_owned();

    // Asked again: the same invoice, not a second one.
    let (status, again, _) = fixture.send(bill()).await;
    assert_eq!(status, StatusCode::CREATED, "{again}");
    assert_eq!(again["number"], number, "a second invoice was raised");

    fixture
        .project::<sales::Sales>(tenant, &sales::projections(), sales::upcasters())
        .await;
    fixture
        .project::<booking::Booking>(tenant, &booking::projections(), booking::upcasters())
        .await;
    fixture
        .project::<tax_sa::TaxSa>(tenant, &tax_sa::projections(), tax_sa::upcasters())
        .await;
    let mut conn = db.acquire().await.expect("connection");

    // The final invoice charges the rest: 200 less the 40 deposit, plus tax.
    let invoice = sales::invoice(&mut conn, &format!("bk-{reservation}"))
        .await
        .expect("reads")
        .expect("the final invoice");
    assert_eq!(invoice.summary.number, number);
    assert_eq!(
        invoice.summary.gross.minor(),
        18_400,
        "{:?}",
        invoice.summary
    );
    assert_eq!(invoice.summary.tax.minor(), 2_400);
    assert_eq!(
        invoice.summary.prepaid_number.as_deref(),
        Some(deducted.as_str())
    );
    let booking_row = booking::reservation(&mut conn, reservation.as_str())
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(
        booking_row.summary.billed_by.as_deref(),
        Some(format!("bk-{reservation}").as_str())
    );

    // **And ZATCA sees the whole supply with the prepayment deducted.**
    let document = tax_sa::document(&mut conn, &number)
        .await
        .expect("reads")
        .expect("a document");
    let xml = document.xml.expect("rendered");
    assert!(
        xml.contains(r#"<cbc:TaxInclusiveAmount currencyID="SAR">230.00</cbc:TaxInclusiveAmount>"#),
        "{xml}"
    );
    assert!(
        xml.contains(r#"<cbc:PrepaidAmount currencyID="SAR">46.00</cbc:PrepaidAmount>"#),
        "{xml}"
    );
    assert!(
        xml.contains(r#"<cbc:PayableAmount currencyID="SAR">184.00</cbc:PayableAmount>"#),
        "{xml}"
    );
    assert!(
        xml.contains("<cbc:DocumentTypeCode>386</cbc:DocumentTypeCode>"),
        "{xml}"
    );
    assert!(
        xml.contains(&format!("<cbc:ID>{deducted}</cbc:ID>")),
        "{xml}"
    );
    drop(conn);

    // **Automatically, when the business asks for it.** A second booking, no
    // deposit, completed — and the worker's pass bills it once the setting
    // is on, and not before.
    let (status, booked, _) = fixture.send(book("BOOKING-2", "2026-05-02")).await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    let second = erp_types::AggregateId::new(booked["id"].as_str().expect("an id")).expect("an id");
    for stage in ["confirmed", "arrived", "in_service", "completed"] {
        let (status, body, _) = fixture
            .send(
                Request::post(format!("/v1/booking/reservations/{second}/stage"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "stage": stage }).to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{stage}: {body}");
    }
    fixture
        .project::<booking::Booking>(tenant, &booking::projections(), booking::upcasters())
        .await;

    let by_the_worker = erp_eventlog::Metadata::default();
    let pass = || erp_api::billing::bill_completions(&db, at("2026-05-03"), &by_the_worker);
    assert_eq!(
        pass().await.expect("runs"),
        0,
        "billed without being asked to"
    );

    let (status, body, _) = fixture
        .send(
            Request::put("/v1/booking/billing")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "on_completion": true }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        pass().await.expect("runs"),
        1,
        "the completed booking was billed"
    );
    assert_eq!(pass().await.expect("runs"), 0, "and not again");

    fixture
        .project::<sales::Sales>(tenant, &sales::projections(), sales::upcasters())
        .await;
    let mut conn = db.acquire().await.expect("connection");
    let invoice = sales::invoice(&mut conn, &format!("bk-{second}"))
        .await
        .expect("reads")
        .expect("the worker's invoice");
    assert_eq!(
        invoice.summary.gross.minor(),
        23_000,
        "no deposit: the whole supply"
    );
    assert_eq!(invoice.summary.prepaid_number, None);

    fixture.cleanup().await;
}

/// A lender that records what it was told and answers with a page.
#[derive(Debug, Default)]
struct RecordingLender {
    told: std::sync::Mutex<Vec<erp_payments::Charge>>,
}

#[async_trait::async_trait]
impl erp_payments::Gateway for RecordingLender {
    fn provider(&self) -> &'static str {
        "tabby"
    }
    async fn charge(
        &self,
        charge: &erp_payments::Charge,
    ) -> Result<erp_payments::Charged, erp_payments::GatewayError> {
        self.told.lock().expect("not poisoned").push(charge.clone());
        Ok(erp_payments::Charged {
            id: "tabby_1".to_owned(),
            status: erp_payments::Status::Initiated,
            amount: charge.amount,
            refunded: erp_types::Money::from_minor(0, charge.amount.currency()),
            fee: None,
            challenge: Some("https://checkout.tabby.ai/s/1".to_owned()),
            message: None,
        })
    }
    async fn fetch(&self, id: &str) -> Result<erp_payments::Charged, erp_payments::GatewayError> {
        Err(erp_payments::GatewayError::NoSuchPayment(id.to_owned()))
    }
    async fn capture(
        &self,
        _id: &str,
        _reference: &str,
        _amount: Option<erp_types::Money>,
    ) -> Result<erp_payments::Charged, erp_payments::GatewayError> {
        unreachable!("nothing here captures")
    }
    async fn refund(
        &self,
        _id: &str,
        _reference: &str,
        _amount: Option<erp_types::Money>,
    ) -> Result<erp_payments::Charged, erp_payments::GatewayError> {
        unreachable!("nothing here refunds")
    }
    async fn void(&self, _id: &str) -> Result<erp_payments::Charged, erp_payments::GatewayError> {
        unreachable!("nothing here voids")
    }
}

/// **A stranger follows a link from a text message.**
///
/// Phase 11e's whole surface: no account, no token, no header but `Host`, and a
/// `Location` back. The three answers are three different instructions to the
/// person holding the phone, and this asserts all three.
#[tokio::test]
async fn a_stranger_follows_a_short_link_and_is_redirected() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;

    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");

    let made = {
        let mut conn = db.acquire().await.expect("connection");
        let good = erp_links::shorten(
            &mut conn,
            &erp_links::New {
                key: "booking.reminder.BK-1".to_owned(),
                target: "/v1/booking/public/services".to_owned(),
                external: false,
                expires_at: None,
                single_use: false,
                at: "2026-05-01T00:00:00Z".parse().expect("an instant"),
            },
        )
        .await
        .expect("shortens");

        // One that has already run out. `chrono::Utc::now()` is what the route
        // measures against, so the expiry has to be a real instant in the past
        // rather than a fixture date.
        let stale = erp_links::shorten(
            &mut conn,
            &erp_links::New {
                key: "booking.reminder.BK-2".to_owned(),
                target: "/v1/booking/public/services".to_owned(),
                external: false,
                expires_at: Some("2020-01-01T00:00:00Z".parse().expect("an instant")),
                single_use: false,
                at: "2019-01-01T00:00:00Z".parse().expect("an instant"),
            },
        )
        .await
        .expect("shortens");

        (good, stale)
    };
    drop(db);

    // Followed with nothing but a host: this is somebody who has never signed
    // in and never will.
    let response = fixture
        .raw(
            Request::get(format!("/l/{}", made.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/v1/booking/public/services"),
        "an internal target goes out relative, so it resolves against this host"
    );
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store, private"),
        "a cached redirect is a single-use link used twice"
    );

    // Expired is `410 Gone`, not `404`: "ask for a new one", not "check you
    // copied it whole".
    let (status, body, _) = fixture
        .send(
            Request::get(format!("/l/{}", made.1))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body["code"], "links.expired");

    let (status, body, _) = fixture
        .send(
            Request::get("/l/0123456789abcdef")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "links.no_such_link");
}

/// **A document goes up, comes back byte for byte, and is never served inline.**
///
/// The last part is the security half. An uploaded file is somebody else's
/// bytes with somebody else's declared type; serving it inline means a browser
/// may render it **in the tenant's own origin**, and an HTML file uploaded as a
/// "document" then runs as the tenant.
#[tokio::test]
async fn a_document_is_uploaded_and_comes_back_as_an_attachment() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, files::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let pdf = b"%PDF-1.7 a signed contract".to_vec();

    // **A document goes on a record that exists.** There is no invoice INV-1
    // on this tenant, so the upload is refused before any bytes are stored.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/files/DOC-1/content?owner_kind=invoice&owner_id=INV-1&name=%D8%B9%D9%82%D8%AF.pdf")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("Idempotency-Key", idem("DOC-1"))
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(pdf.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "files.no_such_owner");

    // The business itself always exists.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/files/DOC-1/content?owner_kind=tenant&owner_id=SELF&name=%D8%B9%D9%82%D8%AF.pdf")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("Idempotency-Key", idem("DOC-1"))
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(pdf.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["engine"], "local");
    assert_eq!(body["size"], 26);
    assert!(
        body["checksum"].as_str().is_some_and(|c| c.len() == 64),
        "{body}"
    );
    // **No URL anywhere in the record.** A URL is where a file is today.
    assert!(
        !body.to_string().contains("http"),
        "the record carries a URL: {body}"
    );

    fixture
        .project::<files::Files>(tenant, &files::projections(), files::upcasters())
        .await;

    // It is listed against the invoice.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/files?owner_kind=tenant&owner_id=SELF")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");

    // And it comes back exactly, as an attachment.
    let response = fixture
        .raw(
            Request::get("/v1/files/DOC-1/content")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        disposition.starts_with("attachment;"),
        "served inline: {disposition}"
    );
    assert!(
        disposition.contains("%D8%B9%D9%82%D8%AF"),
        "the Arabic name did not survive the header: {disposition}"
    );
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    let back = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    assert_eq!(back.as_ref(), pdf.as_slice());
}

/// **A file larger than the ceiling is refused, and a JSON body that size still
/// is.**
///
/// The upload route raises the body limit for itself and for nothing else. A
/// limit raised globally would make every JSON endpoint a way to take the
/// process down.
#[tokio::test]
async fn the_raised_body_limit_applies_to_uploads_and_nowhere_else() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, files::setup()).await;
    fixture.enable_module(tenant, crm::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Two mebibytes: over the default and well under the file ceiling.
    let big = vec![b'x'; 2 * 1024 * 1024];

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/files/DOC-1/content?owner_kind=tenant&owner_id=SELF&name=big.bin")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("Idempotency-Key", idem("big-DOC-1"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(big.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an upload of this size: {body}"
    );

    // The same number of bytes as JSON, at a route that is not an upload.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem("big"))
                .body(Body::from(big))
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "the raised limit leaked to a route that is not an upload"
    );
}

/// **Any list the API can page is a spreadsheet, and nothing had to be written
/// for it.**
///
/// The export is the same query with a different encoder, applied as one layer
/// — so a list added tomorrow is exportable the day it exists. This asserts it
/// against two lists in two modules, neither of which knows.
#[tokio::test]
async fn any_list_comes_back_as_a_spreadsheet_when_asked_for_one() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    for (id, name) in [("C-1", "نورة"), ("C-2", "Najd, Ltd \"the\" one")] {
        let (status, body, _) = fixture
            .send(
                Request::post("/v1/crm/customers")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Idempotency-Key", idem(id))
                    .body(Body::from(
                        serde_json::json!({
                            "name": name, "kind": "person", "phone": "+966500000000"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    fixture
        .project::<crm::Crm>(tenant, &crm::projections(), crm::upcasters())
        .await;

    let response = fixture
        .raw(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT, "text/csv")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    let sheet = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads")
            .to_vec(),
    )
    .expect("utf-8");

    assert!(sheet.lines().count() >= 3, "a header and two rows: {sheet}");
    assert!(sheet.contains("name"), "{sheet}");
    assert!(sheet.contains("نورة"), "{sheet}");
    // The quoting a naive encoder would get wrong.
    assert!(
        sheet.contains(r#""Najd, Ltd ""the"" one""#),
        "the comma and the quotes were not escaped: {sheet}"
    );

    // A second list, in the same shape, with nothing written for it.
    let response = fixture
        .raw(
            Request::get("/v1/modules")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT, "text/csv")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/csv; charset=utf-8"),
        "a list this test did not have to know about"
    );

    // And JSON is still what a client without the header gets.
    let (status, body, kind) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        String::from_utf8_lossy(&kind).starts_with("application/json"),
        "{}",
        String::from_utf8_lossy(&kind)
    );
}

/// **Partial failure is the outcome, not an exception.**
///
/// Five rows, two of them bad: three go in and two come back with their row
/// number and what was wrong. Then the file is corrected and re-uploaded under
/// the same key, and the three that already went in are not duplicated.
#[tokio::test]
async fn an_import_takes_the_good_rows_and_reports_the_bad_ones() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let file = idem("customers.csv");

    let sheet = "id,name,kind,phone,email\n\
                 C-1,نورة,person,+966500000001,\n\
                 C-2,Najd Consulting,company,,hello@najd.example\n\
                 ,Nobody,person,+966500000003,\n\
                 C-4,,person,+966500000004,\n\
                 C-5,Ahmed,person,+966500000005,\n";

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/crm/customers/import")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "text/csv")
                .header("Idempotency-Key", file.clone())
                .body(Body::from(sheet))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 3, "{body}");
    assert_eq!(body["rejected"].as_array().map(Vec::len), Some(2), "{body}");

    // **The spreadsheet's own row numbers**, counting the header as row 1.
    assert_eq!(body["rejected"][0]["row"], 4);
    assert_eq!(body["rejected"][0]["code"], "crm.no_id_column");
    assert_eq!(body["rejected"][1]["row"], 5);
    assert_eq!(body["rejected"][1]["code"], "crm.no_name");
    assert!(
        body["rejected"][1]["detail"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "a rejection with no sentence in it: {body}"
    );

    // The corrected file, under the same key.
    let corrected = "id,name,kind,phone,email\n\
                     C-1,نورة,person,+966500000001,\n\
                     C-2,Najd Consulting,company,,hello@najd.example\n\
                     C-3,Somebody,person,+966500000003,\n\
                     C-4,Fixed Name,person,+966500000004,\n\
                     C-5,Ahmed,person,+966500000005,\n";

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/crm/customers/import")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "text/csv")
                .header("Idempotency-Key", file)
                .body(Body::from(corrected))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"], 5, "{body}");
    assert!(body["rejected"].as_array().is_some_and(Vec::is_empty));

    // Five customers, not eight: the three that went in the first time were
    // recognised rather than duplicated.
    fixture
        .project::<crm::Crm>(tenant, &crm::projections(), crm::upcasters())
        .await;
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().map(Vec::len), Some(5), "{body}");

    // And the events say so: five registrations and nothing else.
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(&mut *conn)
        .await
        .expect("counts");
    assert_eq!(events, 5, "a re-upload duplicated rows");
}

/// **A key that reads bookings cannot post journal entries.**
///
/// Phase 12c's whole point in one test: the key is issued with a role that
/// would permit posting and a scope that does not, and the scope wins. Then it
/// is rotated with an overlap — both halves work — and revoked, and the old one
/// stops.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one key's whole life — issued, scoped, rotated, revoked — and \
              splitting it would mean four fixtures for one story"
)]
async fn an_api_key_is_narrowed_by_its_scopes_and_survives_a_rotation() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, ledger::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // An owner's role, and a scope that reads customers and nothing else.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/keys")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "Booking widget",
                        "scopes": ["crm:read"],
                        "role": "owner"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key_id = body["id"].as_str().expect("an id").to_owned();
    let secret = body["secret"].as_str().expect("a secret").to_owned();
    let public = body["public_key"]
        .as_str()
        .expect("a public key")
        .to_owned();

    assert!(secret.starts_with("sk_"), "{secret}");
    assert!(public.starts_with("pk_"), "{public}");
    assert!(
        secret.contains(public.trim_start_matches("pk_")),
        "the private key does not name its public half, so it cannot be looked up"
    );

    // **The scope it has.**
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // **The scope it does not**, despite the owner's role.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/ledger/accounts")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "keys.out_of_scope");

    // A route outside any module needs a wildcard, and this key has none.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/members")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // **Rotation, with an overlap.** Both keys work.
    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/keys/{key_id}/rotation"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({}).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let replacement = body["secret"].as_str().expect("a secret").to_owned();
    assert_eq!(body["rotated_from"], key_id);
    assert_eq!(
        body["scopes"],
        serde_json::json!(["crm:read"]),
        "a rotation must not change what a key may do"
    );

    for (which, key) in [("the old one", &secret), ("the new one", &replacement)] {
        let (status, body, _) = fixture
            .send(
                Request::get("/v1/crm/customers")
                    .header(header::AUTHORIZATION, format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{which} stopped working: {body}");
    }

    // And the old one carries an expiry now, which is the overlap.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/keys")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let old = body
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["id"] == key_id.as_str()))
        .expect("the old key is still listed");
    assert!(old["expires_at"].is_string(), "{old}");
    assert!(old["last_used_at"].is_string(), "it was used: {old}");

    // **Revoked, and it stops.**
    let (status, body, _) = fixture
        .send(
            Request::delete(format!("/v1/keys/{key_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "why": "rotated" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // The replacement is untouched.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {replacement}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Revoking again is `204`: the caller wanted it off and it is off.
    let (status, _, _) = fixture
        .send(
            Request::delete(format!("/v1/keys/{key_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "why": "again" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// **A key for one tenant is nothing on another's subdomain.**
///
/// The check is membership, not a comparison anybody had to remember to write:
/// the key's machine identity is a member of exactly one tenant, so `enter`
/// refuses everywhere else.
#[tokio::test]
async fn a_key_does_not_work_on_another_tenants_subdomain() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let acme = fixture.provision("acme").await;
    let other = fixture.provision("other").await;
    fixture.join(user, acme).await;
    fixture.join(user, other).await;
    fixture.enable_module(acme, crm::setup()).await;
    fixture.enable_module(other, crm::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/keys")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "Acme's", "scopes": ["*:read"], "role": "viewer"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let secret = body["secret"].as_str().expect("a secret").to_owned();

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::HOST, "acme.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/crm/customers")
                .header(header::HOST, "other.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// **A client is told what to build against, in a typed error and not a 500.**
///
/// And a client that says nothing is served, because `curl` and a browser say
/// nothing and an API that cannot be tried without reading the documentation
/// first is an API nobody tries.
#[tokio::test]
async fn a_client_outside_the_version_range_is_refused_and_told_what_to_build_against() {
    let fixture = Fixture::new().await;

    // Says nothing: served, and told what is current.
    let response = fixture
        .raw(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-api-current")
            .and_then(|v| v.to_str().ok()),
        Some(erp_web::version::CURRENT.to_string().as_str())
    );
    assert_eq!(
        response
            .headers()
            .get("x-api-minimum")
            .and_then(|v| v.to_str().ok()),
        Some(erp_web::version::FLOOR.to_string().as_str())
    );
    assert!(
        response.headers().get("x-api-deprecated").is_none(),
        "current is not deprecated"
    );

    // Declares the current one: the same.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/health")
                .header("x-api-version", erp_web::version::CURRENT.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Declares one this build has never had.
    let response = fixture
        .raw(
            Request::get("/v1/health")
                .header("x-api-version", (erp_web::version::CURRENT + 1).to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // **On the refusal too.** A client that has just been told its version is
    // wrong is exactly the one that needs to know which is right.
    assert_eq!(
        response
            .headers()
            .get("x-api-current")
            .and_then(|v| v.to_str().ok()),
        Some(erp_web::version::CURRENT.to_string().as_str())
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("problem+json");
    assert_eq!(body["code"], "request.api_version_too_new");
    assert_eq!(
        body["args"]["current"]["value"],
        i64::from(erp_web::version::CURRENT),
        "the refusal has to name what to build against: {body}"
    );

    // Something that is not a version at all.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/health")
                .header("x-api-version", "v2.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.api_version_too_new");
}

/// **Tamara's callback reaches the door it was built for.** Tamara posts two
/// bodies — a registered webhook's `event_type` and a checkout notification's
/// `order_status` — neither with an `id`, both under the same HS256
/// `tamaraToken`. Both are accepted, each names its own delivery, and a resend
/// of either is a duplicate.
#[tokio::test]
async fn a_tamara_callback_of_either_shape_is_accepted_and_told_apart() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::put("/v1/hooks/tamara/secret")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "secret": "notify-secret" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let post = |payload: &str, bearer: &str| {
        Request::post("/v1/hooks/tamara")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::from(payload.to_owned()))
            .unwrap()
    };
    let good = tamara_token(b"notify-secret");

    // The webhook shape, as Tamara documents it.
    let webhook = serde_json::json!({
        "order_id": "4fdb781f-5e13-4ae2-9dc6-3ee49e3878a3",
        "order_reference_id": "INV-1", "order_number": "90001860",
        "event_type": "order_approved", "data": []
    })
    .to_string();
    let (status, body, _) = fixture.send(post(&webhook, &good)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body["event_id"],
        "4fdb781f-5e13-4ae2-9dc6-3ee49e3878a3.order_approved"
    );
    assert_eq!(body["duplicate"], false);

    // The notification shape — what the checkout's notification URL gets.
    let notification = serde_json::json!({
        "order_id": "4fdb781f-5e13-4ae2-9dc6-3ee49e3878a3",
        "order_reference_id": "INV-1", "order_number": "90001860",
        "order_status": "approved"
    })
    .to_string();
    let (status, body, _) = fixture.send(post(&notification, &good)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body["event_id"],
        "4fdb781f-5e13-4ae2-9dc6-3ee49e3878a3.status.approved"
    );
    assert_eq!(
        body["duplicate"], false,
        "a notification is not the webhook"
    );

    // Sent again, the way Tamara retries: a duplicate, still accepted.
    let (status, body, _) = fixture.send(post(&webhook, &good)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["duplicate"], true, "{body}");

    // Somebody else's token is nobody.
    let (status, body, _) = fixture
        .send(post(&webhook, &tamara_token(b"other-secret")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Authentic and still not a Tamara body: said precisely, because the
    // sender has been proved to be Tamara.
    let (status, body, _) = fixture
        .send(post(r#"{"order_id":"4fdb781f"}"#, &good))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Two deliveries recorded, with the retry counted on the first.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/hooks/tamara/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = body.as_array().expect("a list");
    assert_eq!(events.len(), 2, "{body}");
    let approved = events
        .iter()
        .find(|e| e["event_id"] == "4fdb781f-5e13-4ae2-9dc6-3ee49e3878a3.order_approved")
        .expect("the webhook");
    assert_eq!(approved["kind"], "order_approved");
    assert_eq!(approved["deliveries"], 2, "{body}");
}

/// A `tamaraToken` the way Tamara mints one: HS256 over `{iss, iat, exp}`.
fn tamara_token(secret: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let now = chrono::Utc::now().timestamp();
    let head = b64.encode(br#"{"typ":"JWT","alg":"HS256"}"#);
    let claims = b64.encode(format!(
        r#"{{"iss":"Tamara","iat":{now},"exp":{}}}"#,
        now + 600
    ));
    let key = openssl::pkey::PKey::hmac(secret).expect("a key");
    let mut signer =
        openssl::sign::Signer::new(openssl::hash::MessageDigest::sha256(), &key).expect("signs");
    signer
        .update(format!("{head}.{claims}").as_bytes())
        .expect("signs");
    let signature = signer.sign_to_vec().expect("signs");
    format!("{head}.{claims}.{}", b64.encode(signature))
}

/// **A callback is verified before its body means anything, and arriving twice
/// does nothing twice.**
///
/// Phase 12b in one test. The unsigned, wrongly-signed and replayed forms are
/// all refused with one answer, and the same event delivered three times is one
/// effect.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "four refusals and three deliveries of one callback; splitting it \
              would mean four fixtures for one story"
)]
async fn a_webhook_is_verified_once_and_deduplicated_however_often_it_arrives() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, body, _) = fixture
        .send(
            Request::put("/v1/hooks/gateway/secret")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "secret": "whsec_abc123" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let payload = serde_json::json!({ "id": "evt_1", "type": "payment.succeeded" }).to_string();
    let now = chrono::Utc::now().timestamp().to_string();
    let sign = |timestamp: &str, body: &str| {
        erp_web::webhook::sign(b"whsec_abc123", format!("{timestamp}.{body}").as_bytes())
    };

    // **Unsigned.**
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/hooks/gateway")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "webhooks.not_verified");

    // **Signed with the wrong secret** — the same answer, deliberately.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/hooks/gateway")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-webhook-timestamp", now.clone())
                .header(
                    "x-webhook-signature",
                    erp_web::webhook::sign(b"wrong", format!("{now}.{payload}").as_bytes()),
                )
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "webhooks.not_verified");

    // **A copy somebody kept**, re-sent with its original timestamp an hour on.
    let stale = (chrono::Utc::now().timestamp() - 3_600).to_string();
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/hooks/gateway")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-webhook-timestamp", stale.clone())
                .header("x-webhook-signature", sign(&stale, &payload))
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // **The real thing.**
    let good = |n: &str, p: &str| {
        Request::post("/v1/hooks/gateway")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-webhook-timestamp", n.to_owned())
            .header("x-webhook-signature", sign(n, p))
            .body(Body::from(p.to_owned()))
            .unwrap()
    };

    let (status, body, _) = fixture.send(good(&now, &payload)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["event_id"], "evt_1");
    assert_eq!(body["duplicate"], false);

    // **Twice more**, the way a provider retries. Still `202`: they did nothing
    // wrong, and an error would make them retry something already done.
    for _ in 0..2 {
        let (status, body, _) = fixture.send(good(&now, &payload)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["duplicate"], true, "{body}");
    }

    // Recorded once, with the deliveries counted.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/hooks/gateway/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body[0]["event_id"], "evt_1");
    assert_eq!(body[0]["kind"], "payment.succeeded");
    assert_eq!(body[0]["deliveries"], 3, "{body}");

    // And one effect, not three.
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    let promised: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox WHERE kind = 'webhook.gateway'")
            .fetch_one(&mut *conn)
            .await
            .expect("counts");
    assert_eq!(
        promised, 1,
        "three deliveries promised more than one effect"
    );

    // A different event under the same secret is its own.
    let second = serde_json::json!({ "id": "evt_2", "type": "payment.refunded" }).to_string();
    let (status, body, _) = fixture.send(good(&now, &second)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["duplicate"], false);
}

/// **Signing in with a phone number, and the two limiters that bound it.**
///
/// Phase 12e. The code is never in a response, a wrong guess is the same answer
/// as an expired one, and asking again too soon is refused with the wait.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one code's whole life — requested, throttled, guessed, used, spent \
              — and splitting it would mean five fixtures for one story"
)]
async fn a_phone_number_signs_in_with_a_code_that_is_single_use() {
    let fixture = Fixture::new().await;
    let phone = "+966500000001";

    let (status, body, _) = fixture
        .send(
            Request::post("/v1/codes")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": "00966 50 000 0001" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["phone"], phone, "the number was normalised: {body}");
    // **The code is never in a response.** It is in a text message, and this is
    // the assertion that keeps it that way.
    assert!(
        !body.to_string().contains("code"),
        "a code reached the caller: {body}"
    );

    // **The request limiter.** One went a moment ago.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/codes")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": phone }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "codes.too_soon");
    assert!(
        body["args"]["seconds"]["value"]
            .as_i64()
            .is_some_and(|s| s > 0),
        "the refusal has to say how long: {body}"
    );

    // The code itself, out of the promised text — which is where a phone would
    // read it from.
    let code = promised_code(&fixture, phone).await;

    // **A wrong guess**, and the answer says nothing about why.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions/code")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": phone, "code": "000000" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "codes.not_valid");

    // A number nobody has ever asked for gets the same answer.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions/code")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": "+966500009999", "code": "000000" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "codes.not_valid");

    // **The real code.**
    let response = fixture
        .raw(
            Request::post("/v1/sessions/code")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": phone, "code": code }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // **Two surfaces, one session.** The cookie and the body carry the same
    // token, and the cookie is not readable by a script.
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");

    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let token = body["token"].as_str().expect("a token").to_owned();
    assert!(cookie.contains(&token), "the cookie is a different session");

    // **Both surfaces authenticate.** This identity is a member of no tenant,
    // so `/v1/tenant` answers 404 — and the contrast with the 401 below is the
    // assertion: 404 means the session was accepted and only the membership was
    // missing.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/tenant")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the bearer was refused: {body}"
    );

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/tenant")
                .header(header::COOKIE, format!("erp_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the cookie was refused: {body}"
    );

    let (status, body, _) = fixture
        .send(Request::get("/v1/tenant").body(Body::empty()).unwrap())
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no credential should not reach a tenant lookup: {body}"
    );

    // **Single use.** The same code again is nothing.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions/code")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "phone": phone, "code": code }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

/// Not a phone number is a `400`, and it says what one looks like.
#[tokio::test]
async fn something_that_is_not_a_phone_number_is_refused() {
    let fixture = Fixture::new().await;

    for raw in ["0500000000", "500000000", "not a number", ""] {
        let (status, body, _) = fixture
            .send(
                Request::post("/v1/codes")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::json!({ "phone": raw }).to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{raw} was accepted: {body}"
        );
        assert_eq!(body["code"], "codes.not_a_phone_number");
    }
}

/// The code, out of the text the control plane promised — which is where a
/// phone would read it from, and the only place it exists.
async fn promised_code(fixture: &Fixture, phone: &str) -> String {
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox
          WHERE kind = 'sms.send' AND payload->>'to' = $1
          ORDER BY id DESC LIMIT 1",
    )
    .bind(phone)
    .fetch_one(fixture.db.pool())
    .await
    .expect("a text was promised");

    let found = payload["body"].as_str().and_then(|body| {
        body.split_whitespace()
            .find(|word| word.len() == 6 && word.chars().all(|c| c.is_ascii_digit()))
    });
    match found {
        Some(code) => code.to_owned(),
        None => panic!("no code in {payload}"),
    }
}

// ---------------------------------------------------------------------------
// Rate limiting — every surface that has nobody to blame
// ---------------------------------------------------------------------------

/// Every operation the document marks unauthenticated, as
/// `(method, path, operation_id)`.
fn public_operations() -> Vec<(String, String, String)> {
    let document = serde_json::to_value(erp_api::openapi()).expect("the document serializes");
    let mut found = Vec::new();
    for (path, item) in document["paths"].as_object().expect("there are paths") {
        for (method, operation) in item.as_object().expect("a path item") {
            let Some(id) = operation["operationId"].as_str() else {
                continue;
            };
            let public = operation["security"]
                .as_array()
                .is_some_and(std::vec::Vec::is_empty);
            if public {
                found.push((method.clone(), path.clone(), id.to_owned()));
            }
        }
    }
    found.sort();
    found
}

/// The two public routes that read no database and hash nothing: a load
/// balancer polls the first, and the second is this document. Bounding them
/// would make a health check the thing that takes the node out of rotation.
const UNBOUNDED_BY_DESIGN: &[&str] = &["/v1/health", "/v1/openapi.json"];

/// A path template with something in every placeholder.
fn with_params(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let name = &rest[open + 1..open + close];
        out.push_str(match name {
            "token" => "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
            "provider" => "moyasar",
            _ => "x",
        });
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// **Every unauthenticated route is bounded, and this is the build failing if
/// one is not.**
///
/// The bound is not a property of a route; it is a property of the extractor
/// the route takes — [`erp_web::Anonymous`] or [`erp_web::Public`] — and a
/// handler can be written without either. So this hammers each public
/// operation from one address until it sees a 429, and a route that answers
/// seven hundred times is one that took neither. The list comes from the
/// document, so a route added tomorrow is on it without anybody remembering.
///
/// Why this exists: the first version of this API bounded the booking page and
/// nothing else. Login, signup, invitation acceptance and one-time codes all
/// ran unbounded — every one a password oracle with Argon2 attached, and the
/// last a phone bill.
#[tokio::test]
async fn every_public_route_is_rate_limited() {
    let mut fixture = Fixture::new().await;
    // A real tenant, so the tenant-scoped routes reach the limiter rather
    // than stopping at "no such business".
    fixture.provision("acme").await;

    let operations = public_operations();
    assert!(
        operations.len() > 10,
        "the document lists {} public operations; the router has more than that",
        operations.len()
    );

    let mut bounded = Vec::new();
    for (n, (method, path, id)) in operations.iter().enumerate() {
        if UNBOUNDED_BY_DESIGN.contains(&path.as_str()) {
            continue;
        }
        // One address per operation, so what is measured is this route's own
        // bound and not the budget an earlier route used up.
        let address = format!("203.0.113.{}", n + 1);
        let uri = with_params(path);
        let mut refused = None;
        for _ in 0..700 {
            let request = Request::builder()
                .method(method.to_uppercase().as_str())
                .uri(&uri)
                .header(header::HOST, "acme.localhost")
                .header(erp_web::FORWARDED_FOR, &address)
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", uuid::Uuid::now_v7().to_string())
                .body(Body::from("{}"))
                .expect("request builds");
            let (status, body, content_type) = fixture.send(request).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                refused = Some((body, content_type));
                break;
            }
        }
        let (body, content_type) = refused.unwrap_or_else(|| {
            panic!(
                "{id} ({method} {path}) answered 700 requests from one address and never said \
                 no. It takes neither `Anonymous` nor `Public`, so nothing bounds it — an \
                 unauthenticated route that hashes a password or sends a text is an oracle \
                 or a phone bill."
            )
        });
        assert_eq!(body["code"], "request.too_many_requests", "{id}: {body}");
        assert!(
            body["args"]["seconds"]["value"].as_i64().unwrap_or(0) > 0,
            "{id} refused without saying when to come back: {body}"
        );
        assert!(
            content_type.starts_with(b"application/problem+json"),
            "{id}: a 429 that is not problem+json"
        );
        bounded.push(id.clone());
    }
    assert!(
        bounded.len() >= 10,
        "only {} public operations were checked: {bounded:?}",
        bounded.len()
    );
}

/// **One account cannot be guessed at from many addresses.** The per-caller
/// bound stops one address guessing at everybody; this is the other half — a
/// distributed guess at one person's password runs out of attempts on the
/// account, whatever it comes from.
#[tokio::test]
async fn one_account_cannot_be_guessed_at_from_many_addresses() {
    let fixture = Fixture::new().await;
    fixture
        .user("target@acme.test", "correct horse battery staple")
        .await;

    let attempt = |address: String| {
        Request::post("/v1/sessions")
            .header(erp_web::FORWARDED_FOR, address)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "handle": "Target@acme.test", "password": "wrong" })
                    .to_string(),
            ))
            .expect("request builds")
    };

    for n in 0..erp_web::rate::AUTH_PER_HANDLE.count {
        let (status, body, _) = fixture.send(attempt(format!("198.51.100.{n}"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {n}: {body}");
    }

    // The next guess comes from yet another address and is refused anyway:
    // the budget followed the account, not the caller. (Mixed case in the
    // handle above is deliberate — one account, one budget.)
    let (status, body, _) = fixture.send(attempt("198.51.100.250".to_owned())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "request.too_many_requests");

    // And the right password is refused too while the window lasts. That is
    // the cost of the bound, and it is the correct one: a lockout that let the
    // right password through would tell a guesser when they had it.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(erp_web::FORWARDED_FOR, "198.51.100.251")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "target@acme.test",
                        "password": "correct horse battery staple"
                    })
                    .to_string(),
                ))
                .expect("request builds"),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Another account from the same addresses is untouched.
    fixture
        .user("other@acme.test", "correct horse battery staple")
        .await;
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(erp_web::FORWARDED_FOR, "198.51.100.1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "other@acme.test",
                        "password": "correct horse battery staple"
                    })
                    .to_string(),
                ))
                .expect("request builds"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// **One address cannot cause texts to many numbers.** The per-number cooldown
/// bounds how often one phone is texted; it says nothing about how many phones.
/// An attacker whose own premium numbers receive the codes is bounded here, and
/// by the platform-wide breaker behind it.
#[tokio::test]
async fn one_address_cannot_cause_texts_to_many_numbers() {
    let fixture = Fixture::new().await;

    let ask = |number: String| {
        Request::post("/v1/codes")
            .header(erp_web::FORWARDED_FOR, "203.0.113.77")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "phone": number }).to_string(),
            ))
            .expect("request builds")
    };

    for n in 0..erp_web::rate::CODES_PER_CALLER.count {
        let (status, body, _) = fixture.send(ask(format!("+96650000010{n}"))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "number {n}: {body}");
    }

    // A sixth, different number from the same address: no text.
    let (status, body, _) = fixture.send(ask("+966500000199".to_owned())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE kind = 'sms.send' AND payload ->> 'to' = '+966500000199'",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("counts");
    assert_eq!(sent, 0, "a refused request still promised a text");
}

/// The limiter keys on an address the caller did not write. With a trusted
/// proxy in front, that is the **last** hop of `X-Forwarded-For` — the one the
/// proxy appended — and earlier hops are ignored, so a caller who prepends
/// addresses of their own choosing is still one caller.
#[tokio::test]
async fn a_caller_cannot_mint_addresses_by_prepending_to_the_forwarded_chain() {
    let fixture = Fixture::new().await;
    let mut refused = false;
    for n in 0..=erp_web::rate::AUTH_PER_CALLER.count {
        let (status, _, _) = fixture
            .send(
                Request::post("/v1/sessions")
                    // A fresh fake client per request, then the proxy's own entry.
                    .header(
                        erp_web::FORWARDED_FOR,
                        format!("10.0.0.{n}, 192.0.2.44"),
                    )
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "handle": format!("u{n}@x.test"), "password": "wrong" })
                            .to_string(),
                    ))
                    .expect("request builds"),
            )
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "rotating the first entry of X-Forwarded-For bought a fresh budget each time"
    );
}

/// **A held value needs a customer to be about.** Written under an id that
/// parses but names nobody, it would be shown on no page, found by no erasure
/// request and reported by nothing — health data the business does not know it
/// holds. The first version wrote it.
#[tokio::test]
async fn a_held_field_cannot_be_set_on_a_customer_that_does_not_exist() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;

    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, body, _) = fixture
        .send(
            Request::put("/v1/crm/customers/CUST-404/fields")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "values": [] }).to_string()))
                .unwrap(),
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "crm.no_such_customer");
    fixture.cleanup().await;
}

/// **Every answer is problem+json, including the two axum used to give away.**
/// A bare 404 or 405 with no body is what a client reading `code` from every
/// refusal cannot read.
#[tokio::test]
async fn an_unknown_route_and_a_wrong_method_answer_in_problem_json() {
    let fixture = Fixture::new().await;

    let response = fixture
        .raw(
            Request::get("/v1/nothing/here")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .expect("json");
    assert_eq!(body["code"], "request.no_such_route");

    let response = fixture
        .raw(Request::delete("/v1/health").body(Body::empty()).unwrap())
        .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .expect("json");
    assert_eq!(body["code"], "request.method_not_allowed");

    fixture.cleanup().await;
}

/// **The second of two people editing a setting is told so.** A settings `GET`
/// carries the version as `ETag`; a `PUT` that sends it back as `If-Match`
/// lands only if nobody wrote in between. The first version had no such
/// header and every settings screen was last-write-wins.
#[tokio::test]
async fn a_stale_settings_write_is_refused_and_a_fresh_one_lands() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let read = |token: String| {
        Request::get("/v1/ledger/vat-rates")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let write = |token: String, if_match: Option<&str>, rate: u32| {
        let mut request = Request::put("/v1/ledger/vat-rates")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(version) = if_match {
            request = request.header(header::IF_MATCH, version);
        }
        request
            .body(Body::from(
                serde_json::json!({ "standard": rate }).to_string(),
            ))
            .unwrap()
    };

    let response = fixture.raw(read(token.clone())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response.headers()[header::ETAG]
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "an ETag is quoted: {etag}"
    );

    // With the version it read: lands.
    let (status, body, _) = fixture.send(write(token.clone(), Some(&etag), 1_500)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // The same version again is somebody who did not see that write.
    let (status, body, _) = fixture.send(write(token.clone(), Some(&etag), 500)).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["code"], "eventlog.configuration_conflict");

    let response = fixture.raw(read(token.clone())).await;
    let fresh = response.headers()[header::ETAG]
        .to_str()
        .expect("ascii")
        .to_owned();
    assert_ne!(fresh, etag, "a write moves the version");
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .expect("json");
    assert_eq!(body["standard"], 1_500, "the refused write changed nothing");

    // Something that is not a version is a client bug, not a condition.
    let (status, body, _) = fixture
        .send(write(token.clone(), Some("yesterday"), 500))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.not_a_version");

    // No header: unconditional, which a script that owns the setting wants.
    let (status, body, _) = fixture.send(write(token.clone(), None, 500)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    fixture.cleanup().await;
}

/// **The public site has a switch, and the deposit is a fraction.** The first
/// build read `booking.public` everywhere and wrote it nowhere.
#[tokio::test]
async fn public_booking_settings_can_be_set_and_a_deposit_over_the_price_cannot() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let write = |deposit_bp: u32| {
        Request::put("/v1/booking/public-settings")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "open": true,
                    "deposit_bp": deposit_bp,
                    "hold_minutes": 30,
                    "verify_phone": false
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, body, _) = fixture.send(write(10_001)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.not_a_fraction");

    let (status, body, _) = fixture.send(write(2_500)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/public-settings")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["open"], true);
    assert_eq!(body["deposit_bp"], 2_500);
    assert_eq!(body["hold_minutes"], 30);
    assert_eq!(body["verify_phone"], false);

    fixture.cleanup().await;
}

/// **A domain is proved by the record the tenant was told to publish**, and by
/// nothing else. The first version marked a domain verified on request.
#[tokio::test]
async fn a_domain_is_proved_only_by_its_published_record() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, claimed, _) = fixture
        .send(
            Request::post("/v1/domains")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "domain": "salon.example" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{claimed}");
    assert_eq!(claimed["record_name"], "_erp-challenge.salon.example");
    let value = claimed["record_value"]
        .as_str()
        .expect("a value")
        .to_owned();
    assert!(value.starts_with("erp-verification="), "{value}");

    let verify = || {
        Request::post("/v1/domains/salon.example/verification")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    // Nothing published: refused, and told what to publish.
    let (status, body, _) = fixture.send(verify()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "domains.not_proved");
    assert_eq!(
        body["args"]["record"]["value"],
        "_erp-challenge.salon.example"
    );

    // The wrong thing published: still refused.
    fixture.prover.publish(
        "_erp-challenge.salon.example",
        "erp-verification=somebody-elses-token",
    );
    let (status, body, _) = fixture.send(verify()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The right record: proved, and proved again is the same answer.
    fixture
        .prover
        .publish("_erp-challenge.salon.example", &value);
    let (status, body, _) = fixture.send(verify()).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _, _) = fixture.send(verify()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A domain nobody claimed is not found.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/domains/other.example/verification")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "domains.not_claimed");

    fixture.cleanup().await;
}

/// **An origin is `https://<host>[:port]` under a proved domain.** Once CORS
/// serves the whole API, an entry here is the whole tenant, so the shape and
/// the domain are both checked; the first version stored whatever it was sent.
#[tokio::test]
async fn an_origin_must_be_https_under_a_proved_domain() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let claim = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");

    let allow = |domain: &str, origin: &str| {
        Request::post("/v1/origins")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "domain": domain, "origin": origin }).to_string(),
            ))
            .unwrap()
    };

    // Unproved: refused with what to do about it.
    let (status, body, _) = fixture
        .send(allow("salon.example", "https://salon.example"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "domains.not_proved");

    fixture.prove("salon.example", &claim);
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("proved");

    for (origin, code) in [
        ("http://salon.example", "origins.not_an_origin"),
        ("https://salon.example/booking", "origins.not_an_origin"),
        ("https://user@salon.example", "origins.not_an_origin"),
        (
            "https://salon.example.attacker.test",
            "origins.outside_domain",
        ),
        ("https://attacker.test", "origins.outside_domain"),
    ] {
        let (status, body, _) = fixture.send(allow("salon.example", origin)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{origin}: {body}");
        assert_eq!(body["code"], code, "{origin}");
    }
    for origin in ["https://salon.example", "https://App.Salon.Example:8443"] {
        let (status, body, _) = fixture.send(allow("salon.example", origin)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{origin}: {body}");
    }
    let origins = fixture.control.origins(tenant).await.expect("lists");
    assert_eq!(
        origins,
        ["https://app.salon.example:8443", "https://salon.example"],
        "stored lowercased, as sent otherwise"
    );

    fixture.cleanup().await;
}

/// **A proved domain serves the API on any host under it**, public and
/// authenticated alike; a lookalike, or the same host before the proof, reaches
/// nobody.
#[tokio::test]
async fn a_proved_domain_serves_the_api_on_every_host_under_it() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let on = |host: &str, path: &str| {
        Request::get(path)
            .header(header::HOST, host)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, body, _) = fixture.send(on("api.salon.example", "/v1/tenant")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unproved host reaches nobody: {body}"
    );

    let claim = fixture
        .control
        .claim_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("claims");
    fixture.prove("salon.example", &claim);
    fixture
        .control
        .verify_domain(tenant, "salon.example", Actor::system())
        .await
        .expect("proved");

    for host in [
        "salon.example",
        "api.salon.example",
        "Book.Salon.Example:443",
    ] {
        let (status, body, _) = fixture.send(on(host, "/v1/tenant")).await;
        assert_eq!(status, StatusCode::OK, "{host}: {body}");
        assert_eq!(body["id"], tenant.to_string(), "{host}");
        // And the public surface, on the same host.
        let (status, body, _) = fixture.send(on(host, "/v1/booking/public/services")).await;
        assert_eq!(status, StatusCode::OK, "{host}: {body}");
    }
    for host in [
        "salon.example.attacker.test",
        "notsalon.example",
        "attacker.test",
    ] {
        let (status, body, _) = fixture.send(on(host, "/v1/tenant")).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{host} reached a tenant: {body}"
        );
    }
    // The subdomain of the platform keeps working beside the custom domain.
    let (status, _, _) = fixture.send(on("acme.localhost", "/v1/tenant")).await;
    assert_eq!(status, StatusCode::OK);

    fixture.cleanup().await;
}

/// **The tenant's clock is a setting**: an IANA zone, refused when it is not
/// one, and versioned like every other setting.
#[tokio::test]
async fn the_tenant_calendar_is_a_named_zone() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let read = || {
        Request::get("/v1/tenant/calendar")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let write = |zone: &str| {
        Request::put("/v1/tenant/calendar")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({ "zone": zone }).to_string()))
            .unwrap()
    };

    let (status, body, _) = fixture.send(read()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["zone"], "Asia/Riyadh",
        "Riyadh until somebody says otherwise"
    );

    let (status, body, _) = fixture.send(write("Mars/Olympus")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.not_a_zone");

    let (status, body, _) = fixture.send(write("Europe/Berlin")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let response = fixture.raw(read()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().contains_key(header::ETAG),
        "versioned like every setting"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .expect("json");
    assert_eq!(body["zone"], "Europe/Berlin");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Real time: the signal stream
// ---------------------------------------------------------------------------

/// **`ready` names every group the tenant may see, at its checkpoint, and
/// nothing else.** The snapshot a screen reconciles against.
#[tokio::test]
async fn ready_names_every_group_the_tenant_may_see_and_nothing_else() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut buffer = String::new();
    let (event, data) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("a first event");
    assert_eq!(event, "ready");
    let ready: serde_json::Value = serde_json::from_str(&data).expect("json");
    let groups = ready["groups"].as_object().expect("groups");
    assert!(groups.contains_key("sales"), "{ready}");
    assert!(groups.contains_key("ledger"), "{ready}");
    assert!(groups.contains_key("tax_sa"), "{ready}");
    assert!(
        !groups.contains_key("booking"),
        "a module the tenant lacks: {ready}"
    );
    assert!(groups["sales"].is_i64());

    fixture.cleanup().await;
}

/// **A signal for a module the tenant lacks is not delivered**, and one for a
/// module it has is — with the group and the position.
#[tokio::test]
async fn a_signal_for_a_module_the_tenant_lacks_is_not_delivered() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (_, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let mut buffer = String::new();
    next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("ready");

    fixture
        .hub
        .publish(&advanced(tenant, "booking", "booking", 5, None));
    fixture
        .hub
        .publish(&advanced(tenant, "sales", "sales", 6, None));
    let (event, data) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("the sales signal");
    assert_eq!(event, "advanced");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).expect("json"),
        serde_json::json!({ "group": "sales", "position": 6 }),
        "the booking signal was delivered to a tenant without booking"
    );

    fixture.cleanup().await;
}

/// **Opening a stream asks for a visit.** A dormant tenant's worker has
/// backed off for hours; a screen that just opened should not wait for it.
#[tokio::test]
async fn opening_a_stream_asks_for_a_visit() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    sqlx::query("UPDATE tenant SET next_visit_at = now() + interval '1 hour' WHERE id = $1")
        .bind(tenant.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("dormant");

    let (status, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut buffer = String::new();
    next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("ready");

    let due: bool = sqlx::query_scalar("SELECT next_visit_at <= now() FROM tenant WHERE id = $1")
        .bind(tenant.as_uuid())
        .fetch_one(fixture.db.pool())
        .await
        .expect("reads");
    assert!(due, "the stream did not ask for a visit");

    fixture.cleanup().await;
}

/// **Without Redis nothing can be watched**, and the route says so rather
/// than opening a stream nothing would ever write to (L6).
#[tokio::test]
async fn without_redis_nothing_can_be_watched() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // The same control plane, a state with no hub: the deployment without Redis.
    let bare = router(AppState::new(Arc::clone(&fixture.control)).trusting_forwarded_for(true));
    let response = bare
        .oneshot(
            Request::get("/v1/events")
                .header(header::HOST, "acme.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("responds");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "request.no_realtime");

    fixture.cleanup().await;
}

/// **The caps refuse the stream past them**, staff and public counted apart.
#[tokio::test]
async fn the_caps_refuse_the_stream_past_them() {
    let hub = Arc::new(erp_web::realtime::Hub::new(erp_web::realtime::Caps {
        staff_per_tenant: 1,
        public_per_tenant: 1,
    }));
    let mut fixture = Fixture::with_hub(Arc::clone(&hub)).await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let open = || {
        Request::get("/v1/events")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, _first) = fixture.open_stream(open()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = fixture.send(open()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "request.too_many_streams");
    assert_eq!(hub.open(tenant), (1, 0));

    fixture.cleanup().await;
}

/// **A stream ends after its lifetime with `reconnect`**, so authorization is
/// re-run by the reconnect rather than outlived by the stream.
#[tokio::test]
async fn a_stream_ends_after_its_lifetime_with_reconnect() {
    let hub = Arc::new(
        erp_web::realtime::Hub::new(erp_web::realtime::Caps::default())
            .living(Duration::from_millis(300)),
    );
    let mut fixture = Fixture::with_hub(hub).await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (_, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let mut buffer = String::new();
    let (event, _) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("ready");
    assert_eq!(event, "ready");
    let (event, _) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("reconnect");
    assert_eq!(event, "reconnect");
    assert!(
        next_event(&mut body, &mut buffer, Duration::from_millis(500))
            .await
            .is_none(),
        "the stream did not end"
    );

    fixture.cleanup().await;
}

/// **A phone hears only its own reservation**, and "many" wakes it too.
#[tokio::test]
async fn a_phone_hears_only_its_own_reservation() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    open_the_diary(&fixture, tenant).await;

    let stream_for = |id: &str| {
        Request::get(format!("/v1/booking/public/reservations/{id}/events"))
            .body(Body::empty())
            .unwrap()
    };
    let mine = idem("PUBLIC-BOOKING-1");
    let theirs = idem("PUBLIC-BOOKING-2");
    let (status, mut my_body) = fixture.open_stream(stream_for(&mine)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, mut their_body) = fixture.open_stream(stream_for(&theirs)).await;
    let (mut my_buffer, mut their_buffer) = (String::new(), String::new());
    let (event, data) = next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2))
        .await
        .expect("ready");
    assert_eq!(event, "ready");
    let ready: serde_json::Value = serde_json::from_str(&data).expect("json");
    assert_eq!(ready["reservation"], mine);
    assert!(ready["position"].is_i64());
    next_event(&mut their_body, &mut their_buffer, Duration::from_secs(2))
        .await
        .expect("ready");

    let reservation = |id: &str| {
        erp_types::StreamId::new(
            <booking::Reservation as erp_eventlog::Aggregate>::domain(),
            erp_types::AggregateId::new(id).expect("an id"),
        )
    };
    fixture.hub.publish(&advanced(
        tenant,
        "booking",
        "booking",
        9,
        Some(vec![reservation(&mine)]),
    ));
    let (event, data) = next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2))
        .await
        .expect("mine");
    assert_eq!(event, "advanced");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).expect("json"),
        serde_json::json!({ "position": 9 })
    );
    assert!(
        next_event(
            &mut their_body,
            &mut their_buffer,
            Duration::from_millis(300)
        )
        .await
        .is_none(),
        "the other phone woke"
    );

    fixture
        .hub
        .publish(&advanced(tenant, "booking", "booking", 10, None));
    assert!(
        next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2))
            .await
            .is_some()
    );
    assert!(
        next_event(&mut their_body, &mut their_buffer, Duration::from_secs(2))
            .await
            .is_some(),
        "many did not wake every phone"
    );

    fixture.cleanup().await;
}

/// **The exit criterion.** Two screens and a phone agree about a schedule
/// within a second of a booking, and nobody polled: the booking is made through
/// the public route, the worker projects and announces it, all three streams
/// hear it at the committed position, and a read at that position shows it.
#[expect(
    clippy::too_many_lines,
    reason = "the phase's exit criterion, told once"
)]
#[tokio::test]
async fn two_screens_and_a_phone_agree_within_a_second_and_nobody_polled() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/booking/resources"))
                .header("Idempotency-Key", idem("CHAIR-1"))
                .body(Body::from(
                    serde_json::json!({
                        "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    open_the_diary(&fixture, tenant).await;
    fixture.project_booking(tenant).await;

    // Two counter screens, and the phone that is about to book.
    let staff = || {
        bearer(Request::get("/v1/events"))
            .body(Body::empty())
            .unwrap()
    };
    let reservation = idem("PUBLIC-BOOKING-1");
    let (_, mut screen_a) = fixture.open_stream(staff()).await;
    let (_, mut screen_b) = fixture.open_stream(staff()).await;
    let (_, mut phone) = fixture
        .open_stream(
            Request::get(format!(
                "/v1/booking/public/reservations/{reservation}/events"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    let (mut buf_a, mut buf_b, mut buf_p) = (String::new(), String::new(), String::new());
    for (body, buffer) in [
        (&mut screen_a, &mut buf_a),
        (&mut screen_b, &mut buf_b),
        (&mut phone, &mut buf_p),
    ] {
        let (event, _) = next_event(body, buffer, Duration::from_secs(2))
            .await
            .expect("ready");
        assert_eq!(event, "ready");
    }

    // The booking, from the phone.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/booking/public/reservations")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", &reservation)
                .body(Body::from(
                    serde_json::json!({
                        "customer_name": "سارة",
                        "customer_phone": "+966500000000",
                        "lines": [{
                            "resource": "CHAIR-1",
                            "from": "2026-05-01T09:00:00Z",
                            "until": "2026-05-01T10:00:00Z"
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The worker, played by this test exactly as `ProjectionJob::tick` does:
    // project in a transaction, commit, announce what was committed.
    let announced = {
        let db = fixture
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let projections = booking::projections();
        let refs: Vec<&dyn erp_projection::Projection<Group = booking::Booking>> =
            projections.iter().map(AsRef::as_ref).collect();
        let mut tx = db.begin().await.expect("transaction");
        let progress = erp_projection::run_once_in::<booking::Booking>(
            &mut tx,
            &refs,
            booking::upcasters(),
            200,
        )
        .await
        .expect("projects");
        let erp_projection::Progress::Advanced { to, streams, .. } = progress else {
            panic!("nothing to project: {progress:?}");
        };
        tx.commit().await.expect("commits");
        fixture.hub.publish(&erp_control::shared::Advanced {
            tenant,
            group: "booking".to_owned(),
            module: booking::module_id(),
            position: to,
            streams,
        });
        to
    };

    // Within a second, all three heard it, at the committed position.
    for (body, buffer, expected) in [
        (
            &mut screen_a,
            &mut buf_a,
            serde_json::json!({ "group": "booking", "position": announced.get() }),
        ),
        (
            &mut screen_b,
            &mut buf_b,
            serde_json::json!({ "group": "booking", "position": announced.get() }),
        ),
        (
            &mut phone,
            &mut buf_p,
            serde_json::json!({ "position": announced.get() }),
        ),
    ] {
        let (event, data) = next_event(body, buffer, Duration::from_secs(1))
            .await
            .expect("advanced within a second");
        assert_eq!(event, "advanced");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&data).expect("json"),
            expected
        );
    }

    // And a read at that position shows the booking — the screen's re-fetch.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get(format!(
                "/v1/booking/reservations/{reservation}?consistent_after={}",
                announced.get()
            )))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stage"], "reserved");

    fixture.cleanup().await;
}

/// **The deposit status waits for the position a stream named.** The phone's
/// re-fetch after `advanced` must not read a row the worker has not written;
/// a position the read model has not reached is a 503 that says so, not a
/// 404 that says there is no deposit.
#[tokio::test]
async fn the_deposit_status_waits_for_the_position_a_stream_named() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    fixture.enable_module(tenant, payments::setup()).await;
    open_the_diary(&fixture, tenant).await;

    let reservation = idem("PUBLIC-BOOKING-1");
    let (status, body, _) = fixture
        .send(
            Request::get(format!(
                "/v1/booking/public/reservations/{reservation}/deposit?consistent_after=999999"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "request.not_caught_up");

    fixture.cleanup().await;
}

/// **The bell rings on one screen and not the other**, and nobody polled.
///
/// Phase 13c's exit criterion, and the proof that a signal carrying a position
/// rather than data is enough: both screens are told that the `notifications`
/// group advanced, both re-fetch their own inbox, and only the person the
/// notification was addressed to sees anything change.
///
/// It is also what stops the obvious mistake — putting the notification on the
/// wire. A payload stream would have to decide who may see it, in a second
/// dialect, and that decision is already made by the route.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the exit criterion: two logins, two streams, an announcement and \
              four re-fetches are what the claim is made of"
)]
async fn a_bell_rings_on_one_screen_and_not_the_other() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.join_as(clerk, tenant, "clerk").await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, hr::setup()).await;
    // Invoices, because what this tenant is told about is a document. Not for
    // anything they issue — for the bindings the wording renders against.
    fixture.enable_sales(tenant).await;
    fixture.enable_module(tenant, messaging::setup()).await;
    fixture.enable_module(tenant, notifications::setup()).await;

    // The owner is somebody in the org chart, and that is what gives them an
    // inbox. Nobody has linked the clerk to anything.
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    hr::hire(
        &db,
        &erp_types::AggregateId::new("EMP-1").expect("an id"),
        &hr::Hire {
            details: hr::Details {
                name: "المديرة".to_owned(),
                name_latin: None,
                national_id: None,
                email: Some("owner@acme.test".to_owned()),
                phone: None,
            },
            reports_to: None,
            branch: None,
            at: chrono::Utc::now(),
        },
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("hired");
    hr::link_login(
        &db,
        &erp_types::AggregateId::new("EMP-1").expect("an id"),
        &owner.to_string(),
        chrono::Utc::now(),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("links");
    fixture
        .project::<hr::Hr>(tenant, &hr::projections(), hr::upcasters())
        .await;

    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let clerk_token = fixture.token("clerk@acme.test", "hunter2hunter2").await;

    // Two screens, both watching.
    let mut streams = Vec::new();
    for token in [&owner_token, &clerk_token] {
        let (status, body) = fixture
            .open_stream(
                Request::get("/v1/events")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        streams.push((body, String::new()));
    }
    for (body, buffer) in &mut streams {
        let (event, data) = next_event(body, buffer, Duration::from_secs(2))
            .await
            .expect("ready");
        assert_eq!(event, "ready");
        let ready: serde_json::Value = serde_json::from_str(&data).expect("json");
        assert!(
            ready["groups"]
                .as_object()
                .expect("groups")
                .contains_key("notifications"),
            "the bell is not a group either screen is watching: {ready}"
        );
    }

    // Both inboxes start empty.
    for token in [&owner_token, &clerk_token] {
        let (status, body, _) = fixture
            .send(
                Request::get("/v1/notifications")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["unread"], 0, "{body}");
    }

    // Something happens, and it is addressed to whoever runs the business.
    let mut tx = db.begin().await.expect("transaction");
    let announced = notifications::announce(
        &mut tx,
        &notifications::Announcing {
            kind: notifications::Kind::TaxRefused,
            subject: messaging::Subject::new(
                messaging::Topic::Invoice,
                erp_types::AggregateId::new("INV-1").expect("an id"),
            ),
            at: chrono::Utc::now(),
        },
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("announces");
    tx.commit().await.expect("commits");
    assert!(announced.announced);
    fixture
        .project::<notifications::Notifications>(
            tenant,
            &notifications::projections(),
            notifications::upcasters(),
        )
        .await;

    // The worker would publish this the moment the group advanced.
    fixture
        .hub
        .publish(&advanced(tenant, "notifications", "notifications", 9, None));

    // **Both screens hear it**, because a signal names a group and not a
    // person — and re-fetching is what tells each of them whether it was
    // theirs.
    for (body, buffer) in &mut streams {
        let (event, data) = next_event(body, buffer, Duration::from_secs(2))
            .await
            .expect("the bell signal");
        assert_eq!(event, "advanced");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&data).expect("json"),
            serde_json::json!({ "group": "notifications", "position": 9 })
        );
    }

    let (_, mine, _) = fixture
        .send(
            Request::get("/v1/notifications?unread=true")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        mine["unread"], 1,
        "the person it was addressed to was not told: {mine}"
    );
    assert_eq!(mine["items"][0]["kind"], "tax_refused", "{mine}");
    assert!(
        mine["items"][0]["title"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "a notification with nothing to say: {mine}"
    );

    let (_, theirs, _) = fixture
        .send(
            Request::get("/v1/notifications")
                .header(header::AUTHORIZATION, format!("Bearer {clerk_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(theirs["unread"], 0, "somebody else's bell rang: {theirs}");
    assert_eq!(theirs["items"].as_array().expect("items").len(), 0);

    // And clearing it is the reader's own act.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/notifications/read")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    fixture
        .project::<notifications::Notifications>(
            tenant,
            &notifications::projections(),
            notifications::upcasters(),
        )
        .await;
    let (_, mine, _) = fixture
        .send(
            Request::get("/v1/notifications")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        mine["unread"], 0,
        "clearing the bell left it ringing: {mine}"
    );

    fixture.cleanup().await;
}

/// **A conversation, and a reply reaching an open screen.**
///
/// Phase 13d over HTTP: a note stays inside, a message goes out, both are in
/// the thread in the order they happened — and the 13a stream names the group
/// so a screen with the thread open re-fetches without polling.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a customer, a stream, two kinds of line, a refusal and a signal — \
              what the claim is made of"
)]
async fn a_conversation_holds_both_kinds_and_reaches_an_open_screen() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, messaging::setup()).await;
    fixture.enable_module(tenant, conversations::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Somebody to talk to.
    let (status, _, _) = fixture
        .send(
            Request::post("/v1/crm/customers")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem("CUST-1"))
                .body(Body::from(
                    serde_json::json!({
                        "name": "نورة",
                        "kind": "person",
                        "phone": "+966500000001"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    fixture
        .project::<crm::Crm>(tenant, &crm::projections(), crm::upcasters())
        .await;

    // A create takes its id from the `Idempotency-Key`, so the customer is
    // stored under what that key derives.
    let customer = idem("CUST-1");
    let thread = format!("/v1/conversations/customer/{customer}");

    // A screen watching this tenant.
    let (status, mut stream) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut buffer = String::new();
    let (event, data) = next_event(&mut stream, &mut buffer, Duration::from_secs(2))
        .await
        .expect("ready");
    assert_eq!(event, "ready");
    assert!(
        serde_json::from_str::<serde_json::Value>(&data).expect("json")["groups"]
            .as_object()
            .expect("groups")
            .contains_key("conversations"),
        "a screen cannot watch the group a conversation lives in"
    );

    // An internal note, then something said to her.
    let (status, _, _) = fixture
        .send(
            Request::post(format!("{thread}/notes"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "text": "اتصلت، تريد الخميس" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, said, _) = fixture
        .send(
            Request::post(format!("{thread}/messages"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "text": "الخميس الساعة ١٠", "channel": "sms" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{said}");

    // WhatsApp is refused, and says why.
    let (status, refused, _) = fixture
        .send(
            Request::post(format!("{thread}/messages"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "text": "مرحبا", "channel": "whatsapp" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "conversations.not_a_channel_for_this");

    fixture
        .project::<conversations::Conversations>(
            tenant,
            &conversations::projections(),
            conversations::upcasters(),
        )
        .await;

    let (status, body, _) = fixture
        .send(
            Request::get(&thread)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lines = body["items"].as_array().expect("items");
    assert_eq!(lines.len(), 2, "{body}");
    assert_eq!(
        lines[0]["kind"], "note",
        "a conversation reads newest first"
    );
    assert!(
        lines[0]["channel"].is_null(),
        "a note went out on a channel"
    );
    assert_eq!(lines[1]["kind"], "said");
    assert_eq!(lines[1]["channel"], "sms");
    assert_eq!(lines[1]["address"], "+966500000001");

    // The worker would publish this the moment the group advanced.
    fixture.hub.publish(&advanced(
        tenant,
        "conversations",
        "conversations",
        12,
        None,
    ));
    let (event, data) = next_event(&mut stream, &mut buffer, Duration::from_secs(2))
        .await
        .expect("the conversation signal");
    assert_eq!(event, "advanced");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).expect("json"),
        serde_json::json!({ "group": "conversations", "position": 12 })
    );

    fixture.cleanup().await;
}
