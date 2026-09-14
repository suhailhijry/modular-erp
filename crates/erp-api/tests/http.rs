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
                    // Open, as a development stack is; the one test about a
                    // closed deployment builds its own router.
                    .opening_signup(true)
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

    /// The enrolment token from the last second-factor-reset email to an
    /// address. **The mailbox again**, and the only place the token exists:
    /// neither reset route answers with it, so nobody who cannot read the
    /// person's mail can enrol for them.
    async fn enrolment_link(&self, email: &str) -> String {
        let body: String = sqlx::query_scalar(
            "SELECT payload ->> 'body' FROM outbox
              WHERE kind = 'email.send' AND payload ->> 'to' = $1
                AND idempotency_key LIKE 'enrolment:%'
              ORDER BY id DESC LIMIT 1",
        )
        .bind(email)
        .fetch_one(self.control.pool())
        .await
        .unwrap_or_else(|e| panic!("an enrolment link was promised to {email}: {e}"));

        body.split_once("/second-factor/")
            .map(|(_, rest)| {
                rest.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| panic!("no enrolment link in the message to {email}: {body}"))
    }

    /// Puts somebody on the org chart and links their login, so a claim can
    /// reach them. `above` is who they report to.
    async fn hire(
        &self,
        tenant: TenantId,
        employee: &str,
        email: &str,
        identity: IdentityId,
        above: Option<&str>,
    ) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let id = erp_types::AggregateId::new(employee).expect("an id");
        hr::hire(
            &db,
            &id,
            &hr::Hire {
                details: hr::Details {
                    name: email.to_owned(),
                    name_latin: None,
                    national_id: None,
                    email: Some(email.to_owned()),
                    phone: None,
                },
                reports_to: above.map(|a| erp_types::AggregateId::new(a).expect("an id")),
                branch: None,
                at: chrono::Utc::now(),
            },
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("hired");
        hr::link_login(
            &db,
            &id,
            &identity.to_string(),
            chrono::Utc::now(),
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("links");
        self.project::<hr::Hr>(tenant, &hr::projections(), hr::upcasters())
            .await;
    }

    /// Grants a claim company-wide, propagating, the way the granting screen
    /// does.
    async fn grant_claim(&self, tenant: TenantId, employee: &str, claim: &str) {
        let db = self
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        hr::grant_claim(
            &db,
            &erp_types::AggregateId::new(employee).expect("an id"),
            &hr::Claim {
                name: claim.to_owned(),
                branch: None,
            },
            true,
        )
        .await
        .expect("granted");
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

    /// Enrols an authenticator app for somebody with the password
    /// `hunter2hunter2`, and signs them in with its code. Returns the token and
    /// the recovery codes.
    async fn enrolled_token(&self, identity: IdentityId, email: &str) -> (String, Vec<String>) {
        // The app's own key — see `Fixture::with_hub`.
        let sealing = erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes");
        let enrolment = self
            .control
            .begin_second_factor(identity, "ERP", email, &sealing, None)
            .await
            .expect("enrolment begins");
        let secret = erp_control::totp::unbase32(&enrolment.secret).expect("base32");
        let now = chrono::Utc::now();
        let seconds = u64::try_from(now.timestamp()).expect("after 1970");
        let code = erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS)
            .expect("a code");
        let recovery = self
            .control
            .confirm_second_factor(identity, &code, None, now, &sealing, None, None)
            .await
            .expect("enrolment confirms")
            .recovery_codes;

        let (status, body, _) = self
            .send(
                Request::post("/v1/sessions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "handle": email, "password": "hunter2hunter2", "code": code
                        })
                        .to_string(),
                    ))
                    .expect("request builds"),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        (
            body["token"].as_str().expect("a token").to_owned(),
            recovery,
        )
    }

    /// Somebody on platform staff, signed in with both factors — made the way
    /// `operator grant-staff` makes one. Returns their identity, token and
    /// recovery codes.
    async fn staff(
        &self,
        email: &str,
        role: erp_control::PlatformRole,
    ) -> (IdentityId, String, Vec<String>) {
        let identity = self.user(email, "hunter2hunter2").await;
        let (token, recovery) = self.enrolled_token(identity, email).await;
        self.control
            .grant_staff(email, role, Actor::system())
            .await
            .expect("staff are granted");
        (identity, token, recovery)
    }

    /// Every connection the control pool will give — four, `erp-testkit`'s
    /// size — open and idle.
    ///
    /// For the tests that race two calls: without it one side spends its turn
    /// opening a connection while the other runs to the end, and a race that
    /// only sometimes happens proves nothing.
    async fn warm(&self) {
        let pool = self.control.pool();
        let open = tokio::join!(pool.begin(), pool.begin(), pool.begin(), pool.begin());
        for tx in [open.0, open.1, open.2, open.3] {
            tx.expect("a connection")
                .rollback()
                .await
                .expect("rolls back");
        }
    }

    /// A request as whoever holds `token`: `(status, body)`.
    async fn as_caller(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let (status, answer, _) = self.send(request.body(body).unwrap()).await;
        (status, answer)
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

/// **The statements and the calendar, over HTTP.** A tenant sets a 4-4-5
/// calendar, posts across two fiscal years, and reads every statement by it:
/// periods, balances as at a date, the profit and loss for a period, the
/// balance sheet with its two trading results, and the journal paged and
/// filtered. Then the books close, the calendar is locked, and a posting slipped
/// in behind the projection makes the sheet a 503 rather than a lie.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one set of postings, and every statement route that reads it"
)]
async fn statements_are_read_by_the_fiscal_calendar() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let post = |path: &str, key: &str, body: serde_json::Value| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", idem(key))
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let put = |path: &str, body: serde_json::Value| {
        Request::put(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let read = |path: &str| {
        Request::get(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    // A tenant that never chose has calendar months from 1 January.
    let (status, calendar, _) = fixture.send(read("/v1/ledger/fiscal-calendar")).await;
    assert_eq!(status, StatusCode::OK, "{calendar}");
    assert_eq!(calendar["starts_on"], "2000-01-01");
    assert_eq!(calendar["pattern"], "monthly");

    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/fiscal-calendar",
            serde_json::json!({ "starts_on": "2026-01-01", "pattern": "weekly" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "ledger.not_a_pattern");

    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/fiscal-calendar",
            serde_json::json!({ "starts_on": "2026-01-01", "pattern": "4-4-5" }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // 2026 starts on the Thursday nearest 1 January — 1 January itself — and
    // its first period is four weeks.
    let (status, periods, _) = fixture.send(read("/v1/ledger/periods?year=2026")).await;
    assert_eq!(status, StatusCode::OK, "{periods}");
    assert_eq!(periods.as_array().unwrap().len(), 12);
    assert_eq!(periods[0]["id"], "2026-P01");
    assert_eq!(periods[0]["from"], "2026-01-01");
    assert_eq!(periods[0]["until"], "2026-01-29");
    assert_eq!(periods[0]["closed"], false);
    assert_eq!(periods[11]["id"], "2026-P12");

    for (code, kind) in [
        ("1000", "asset"),
        ("3000", "equity"),
        ("3100", "equity"),
        ("4000", "revenue"),
        ("5000", "expense"),
    ] {
        let (status, body, _) = fixture
            .send(post(
                "/v1/ledger/accounts",
                code,
                serde_json::json!({ "code": code, "name": code, "kind": kind, "currency": "SAR" }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // Capital and a sale in 2025; a sale and the rent in 2026-P02.
    for (key, on, debit, credit, minor) in [
        ("capital", "2025-06-01T00:00:00Z", "1000", "3000", 100_000),
        ("sale-2025", "2025-12-15T00:00:00Z", "1000", "4000", 50_000),
        ("sale-2026", "2026-02-10T00:00:00Z", "1000", "4000", 30_000),
        ("rent-2026", "2026-02-20T00:00:00Z", "5000", "1000", 10_000),
    ] {
        let (status, body, _) = fixture
            .send(post(
                "/v1/ledger/entries",
                key,
                serde_json::json!({
                    "occurred_on": on,
                    "memo": key,
                    "lines": [
                        { "account": debit, "amount": { "minor": minor, "currency": "SAR" } },
                        { "account": credit, "amount": { "minor": -minor, "currency": "SAR" },
                          "memo": "the other side" }
                    ]
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    fixture.project_ledger(tenant).await;

    // Balances as at the start of 2026: the capital and the 2025 sale.
    let (status, balances, _) = fixture
        .send(read("/v1/ledger/balances?until=2026-01-01T00:00:00Z"))
        .await;
    assert_eq!(status, StatusCode::OK, "{balances}");
    let cash = balances
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "1000")
        .expect("cash is shown");
    assert_eq!(cash["balance"], 150_000);
    assert_eq!(cash["postings"], 2);

    // The profit and loss for the period the 2026 entries fall in.
    let (status, pnl, _) = fixture
        .send(read(
            "/v1/ledger/statements/profit-and-loss?period=2026-P02",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{pnl}");
    assert_eq!(pnl["period"], "2026-P02");
    assert_eq!(pnl["from"], "2026-01-28T21:00:00Z", "Riyadh midnight");
    let sar = &pnl["currencies"][0];
    assert_eq!(sar["currency"], "SAR");
    assert_eq!(sar["revenue"][0]["code"], "4000");
    assert_eq!(sar["revenue"][0]["amount"], 30_000);
    assert_eq!(sar["expenses"][0]["code"], "5000");
    assert_eq!(sar["expenses"][0]["amount"], 10_000);
    assert_eq!(sar["total_revenue"], 30_000);
    assert_eq!(sar["total_expenses"], 10_000);
    assert_eq!(sar["result"], 20_000);

    let (status, body, _) = fixture
        .send(read(
            "/v1/ledger/statements/profit-and-loss?from=2026-01-01T00:00:00Z",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "ledger.not_a_range");
    let (status, body, _) = fixture
        .send(read(
            "/v1/ledger/statements/profit-and-loss?period=2026-P13",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "ledger.no_such_period");

    // The balance sheet as at 1 March 2026: this year's result and every
    // earlier year's are two equity lines the chart does not hold.
    let (status, sheet, _) = fixture
        .send(read(
            "/v1/ledger/statements/balance-sheet?as_at=2026-03-01T00:00:00Z",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{sheet}");
    assert_eq!(sheet["fiscal_year_started"], "2026-01-01");
    let sar = &sheet["currencies"][0];
    assert_eq!(sar["assets"][0]["code"], "1000");
    assert_eq!(sar["assets"][0]["amount"], 170_000);
    assert_eq!(sar["equity"][0]["code"], "3000");
    assert_eq!(sar["equity"][0]["amount"], 100_000);
    assert_eq!(sar["current_year_result"], 20_000);
    assert_eq!(sar["prior_years_result"], 50_000);
    assert_eq!(sar["total_assets"], 170_000);
    assert_eq!(sar["total_liabilities"], 0);
    assert_eq!(sar["total_equity"], 170_000);

    // The journal: newest first, paged, filtered, and one entry with its lines.
    let (status, page, _) = fixture.send(read("/v1/ledger/entries?limit=3")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [idem("rent-2026"), idem("sale-2026"), idem("sale-2025")]
    );
    let next = page["next"].as_str().expect("a fourth entry remains");
    let (status, page, _) = fixture
        .send(read(&format!("/v1/ledger/entries?limit=3&after={next}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["id"], idem("capital"));
    assert!(page["next"].is_null(), "{page}");

    let (status, page, _) = fixture.send(read("/v1/ledger/entries?account=4000")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let (status, page, _) = fixture
        .send(read(
            "/v1/ledger/entries?from=2025-01-01T00:00:00Z&until=2026-01-01T00:00:00Z",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);

    let (status, rent, _) = fixture
        .send(read(&format!("/v1/ledger/entries/{}", idem("rent-2026"))))
        .await;
    assert_eq!(status, StatusCode::OK, "{rent}");
    assert_eq!(rent["occurred_on"], "2026-02-20T00:00:00Z");
    assert_eq!(rent["currency"], "SAR");
    assert_eq!(rent["lines"][0]["account"], "5000");
    assert_eq!(rent["lines"][0]["debit"], 10_000);
    assert_eq!(rent["lines"][0]["credit"], 0);
    assert_eq!(rent["lines"][1]["credit"], 10_000);
    assert_eq!(rent["lines"][1]["memo"], "the other side");
    let (status, body, _) = fixture.send(read("/v1/ledger/entries/nothing")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["code"], "ledger.no_such_entry");

    // Close 2025 by its last period — the first close ever may be any period,
    // and everything before it closes with it.
    let act = |path: &str| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, body, _) = fixture.send(act("/v1/ledger/years/2025/close")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ledger.year_open");
    let (status, body, _) = fixture.send(act("/v1/ledger/periods/2025-P12/close")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture.send(act("/v1/ledger/periods/2026-P02/close")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ledger.period_out_of_order");
    assert_eq!(body["args"]["next"]["value"], "2026-P01");
    let (status, periods, _) = fixture.send(read("/v1/ledger/periods?year=2025")).await;
    assert_eq!(status, StatusCode::OK, "{periods}");
    assert!(
        periods
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["closed"] == true),
        "{periods}"
    );

    // Book 2025: one closing entry, into 3100.
    let (status, year, _) = fixture.send(act("/v1/ledger/years/2025/close")).await;
    assert_eq!(status, StatusCode::OK, "{year}");
    assert_eq!(year["booked"], true);
    assert_eq!(year["closed"], true);
    assert_eq!(
        year["closing_entries"],
        serde_json::json!(["closing-2025-SAR-1"])
    );
    let (status, books, _) = fixture.send(read("/v1/ledger/books")).await;
    assert_eq!(status, StatusCode::OK, "{books}");
    assert_eq!(books["booked"], serde_json::json!([2025]));
    fixture.project_ledger(tenant).await;
    let (status, closing, _) = fixture
        .send(read("/v1/ledger/entries/closing-2025-SAR-1"))
        .await;
    assert_eq!(status, StatusCode::OK, "{closing}");
    assert_eq!(closing["closing"], true);
    assert_eq!(closing["lines"][0]["account"], "4000");
    assert_eq!(
        closing["lines"][0]["debit"], 50_000,
        "2025's sale posted away"
    );
    assert_eq!(closing["lines"][1]["account"], "3100");
    assert_eq!(closing["lines"][1]["credit"], 50_000, "the year's profit");
    // The 2026 profit and loss is what it was: the closing entry is not trade.
    let (status, pnl, _) = fixture
        .send(read(
            "/v1/ledger/statements/profit-and-loss?period=2026-P02",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{pnl}");
    assert_eq!(pnl["currencies"][0]["result"], 20_000);
    let (status, body, _) = fixture
        .send(act("/v1/ledger/periods/2025-P12/reopen"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ledger.year_booked");
    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/closing-accounts",
            serde_json::json!({ "USD": "3900" }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, accounts, _) = fixture.send(read("/v1/ledger/closing-accounts")).await;
    assert_eq!(status, StatusCode::OK, "{accounts}");
    assert_eq!(accounts, serde_json::json!({ "USD": "3900" }));
    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/closing-accounts",
            serde_json::json!({ "DOLLARS": "3900" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The calendar may change from the next open year — a new segment on the
    // boundary — and nowhere inside closed time or off a boundary.
    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/fiscal-calendar",
            serde_json::json!({ "starts_on": "2026-01-01", "pattern": "monthly" }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, calendar, _) = fixture.send(read("/v1/ledger/fiscal-calendar")).await;
    assert_eq!(status, StatusCode::OK, "{calendar}");
    assert_eq!(calendar["pattern"], "monthly");
    assert_eq!(calendar["segments"].as_array().unwrap().len(), 2);
    assert_eq!(calendar["segments"][0]["pattern"], "4-4-5");
    let (status, periods, _) = fixture.send(read("/v1/ledger/periods?year=2026")).await;
    assert_eq!(status, StatusCode::OK, "{periods}");
    assert_eq!(periods[0]["until"], "2026-02-01", "2026 is monthly now");
    let (status, periods, _) = fixture.send(read("/v1/ledger/periods?year=2025")).await;
    assert_eq!(status, StatusCode::OK, "{periods}");
    assert_eq!(periods[0]["until"], "2025-01-30", "2025 stays 4-4-5");
    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/fiscal-calendar",
            serde_json::json!({ "starts_on": "2025-06-01", "pattern": "monthly" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ledger.calendar_locked");
    let (status, body, _) = fixture
        .send(put(
            "/v1/ledger/fiscal-calendar",
            serde_json::json!({ "starts_on": "2026-03-01", "pattern": "quarterly" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "ledger.not_a_year_start");
    assert_eq!(body["args"]["next"]["value"], "2027-01-01");

    // A posting nothing balanced — behind the projection's back — and the
    // sheet is refused rather than shown.
    {
        let db = fixture
            .control
            .enter_for_maintenance(tenant)
            .await
            .expect("maintenance entry");
        let mut conn = db.acquire().await.expect("connection");
        sqlx::query(
            "INSERT INTO proj_ledger.posting \
                (id, entry_id, line_index, account, amount, currency, occurred_on, recorded_at) \
             VALUES (gen_random_uuid(), 'ghost', 0, '1000', 1, 'SAR', now(), now())",
        )
        .execute(&mut *conn)
        .await
        .expect("a posting slips in");
    }
    let (status, body, _) = fixture
        .send(read("/v1/ledger/statements/balance-sheet"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "ledger.sheet_does_not_balance");
    assert!(
        body["detail"].as_str().unwrap().contains("0.01 SAR"),
        "the message must say by how much: {}",
        body["detail"]
    );

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

/// **Signup is closed unless the deployment opens it** (decided 2026-09-14).
/// A router built without `opening_signup` refuses the form with a message
/// that says who to call, before any budget is charged; the confirmation link
/// still answers, because a company staff set up is confirmed through it.
#[tokio::test]
async fn signup_is_closed_unless_the_deployment_opens_it() {
    let fixture = Fixture::new().await;
    let closed = router(AppState::new(Arc::clone(&fixture.control)));
    let send = |request: Request<Body>| {
        let closed = closed.clone();
        async move {
            let response = closed.oneshot(request).await.expect("answers");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .expect("a body");
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            (status, body)
        }
    };

    let (status, body) = send(
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
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("signups.closed")),
        "{body}"
    );

    // The link's route is not what is closed: a bad token is the usual 404.
    let (status, body) = send(
        Request::post("/v1/signups/not-a-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("signups.not_valid")),
        "{body}"
    );

    // And a self-signup that carries its password refuses a second one at the
    // link rather than ignoring what somebody typed as theirs.
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
    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/signups/{link}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "password": "another password entirely" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("signups.password_not_needed")),
        "{body}"
    );

    fixture.cleanup().await;
}

/// **A member confined to a branch acts there and reads there, and nowhere
/// else** — decided 2026-09-14, and the first time `X-Branch` is anything but
/// a header the caller wrote. The owner confines a clerk to Olaya: Malaz is
/// refused, no header means Olaya, the shelves and the summary show Olaya
/// alone, and the org chart cannot be read company-wide. With two branches
/// the request has to name one. A key is bound through its own membership like
/// a person. An empty list lifts it. A branch that is not open cannot be
/// confined to.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one clerk through every door a branch list closes and opens, plus a key"
)]
async fn a_member_confined_to_a_branch_acts_and_reads_there_and_nowhere_else() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.join_as(clerk, tenant, "clerk").await;
    fixture.enable_ledger(tenant).await;
    fixture.enable_module(tenant, branches::setup()).await;
    fixture.enable_module(tenant, inventory::setup()).await;
    fixture.enable_module(tenant, hr::setup()).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let clerk_token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    fixture
        .install_chart(&owner_token, "acme", "services")
        .await;

    let post =
        |token: &str, path: String, key: &str, branch: Option<&str>, body: serde_json::Value| {
            let mut request = Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", idem(key));
            if let Some(branch) = branch {
                request = request.header("x-branch", branch);
            }
            request.body(Body::from(body.to_string())).unwrap()
        };
    let address =
        serde_json::json!({ "street": "King Fahd Road", "city": "Riyadh", "country": "SA" });
    for (key, name) in [("BRANCH-OLAYA", "العليا"), ("BRANCH-MALAZ", "الملز")] {
        let (status, body, _) = fixture
            .send(post(
                &owner_token,
                "/v1/branches".to_owned(),
                key,
                None,
                serde_json::json!({ "name": name, "address": address }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let olaya = idem("BRANCH-OLAYA");
    let malaz = idem("BRANCH-MALAZ");
    let (status, body, _) = fixture
        .send(post(
            &owner_token,
            "/v1/inventory/products".to_owned(),
            "PROD-MILK",
            None,
            serde_json::json!({ "name": "حليب", "unit": "bottle", "tracking": "none" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let milk = idem("PROD-MILK");
    let receipt = |token: &str, key: &str, branch: Option<&str>| {
        post(
            token,
            format!("/v1/inventory/stock/{milk}/receipts"),
            key,
            branch,
            serde_json::json!({ "quantity": 6, "value": { "minor": 3_000, "currency": "SAR" } }),
        )
    };
    let confine = |branches: serde_json::Value| {
        Request::put(format!("/v1/members/{clerk}/branches"))
            .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "branches": branches }).to_string(),
            ))
            .unwrap()
    };
    let member_branches = |identity: IdentityId| {
        let fixture = &fixture;
        let owner_token = &owner_token;
        async move {
            let (status, members) = fixture
                .as_caller(owner_token, "GET", "/v1/members", None)
                .await;
            assert_eq!(status, StatusCode::OK, "{members}");
            members
                .as_array()
                .expect("a list")
                .iter()
                .find(|m| m["identity"] == identity.to_string())
                .expect("listed")["branches"]
                .clone()
        }
    };

    // Every branch, until the owner says otherwise — and only an open branch
    // can be said.
    assert_eq!(member_branches(clerk).await, serde_json::json!([]));
    let (status, body, _) = fixture
        .send(confine(serde_json::json!(["BR-JEDDAH"])))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("request.no_such_branch")),
        "{body}"
    );
    let (status, body, _) = fixture.send(confine(serde_json::json!([olaya]))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(member_branches(clerk).await, serde_json::json!([olaya]));

    // **Acting.** Malaz is refused; no header is Olaya.
    let (status, body, _) = fixture
        .send(receipt(&clerk_token, "RCV-MALAZ", Some(malaz.as_str())))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.wrong_branch")),
        "{body}"
    );
    assert_eq!(body["args"]["branch"]["value"], malaz, "{body}");
    let (status, body, _) = fixture.send(receipt(&clerk_token, "RCV-OLAYA", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // The owner, unconfined, stocks Malaz.
    let (status, body, _) = fixture
        .send(receipt(&owner_token, "RCV-OWNER", Some(malaz.as_str())))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    fixture
        .project::<inventory::Inventory>(tenant, &inventory::projections(), inventory::upcasters())
        .await;

    // **Reading.** The clerk's shelves are Olaya's; asking for Malaz is refused;
    // the summary shows Olaya alone, where the owner sees both.
    let (status, shelves) = fixture
        .as_caller(&clerk_token, "GET", "/v1/inventory/stock", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{shelves}");
    let shelf_branches: Vec<&str> = shelves["items"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|s| s["branch"].as_str().expect("a branch"))
        .collect();
    assert_eq!(shelf_branches, [olaya.as_str()], "{shelves}");
    let (status, body) = fixture
        .as_caller(
            &clerk_token,
            "GET",
            &format!("/v1/inventory/stock?branch={malaz}"),
            None,
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.wrong_branch")),
        "{body}"
    );
    let summary = |token: String| {
        let fixture = &fixture;
        async move {
            let (status, body) = fixture
                .as_caller(&token, "GET", "/v1/inventory/summary", None)
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["branches"]
                .as_array()
                .expect("branches")
                .iter()
                .map(|b| b["branch"].as_str().expect("a branch").to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(summary(clerk_token.clone()).await, vec![olaya.clone()]);
    assert_eq!(
        summary(owner_token.clone()).await.len(),
        2,
        "the owner sees every branch"
    );
    // The org chart, company-wide, is for somebody who belongs to the company.
    let (status, body) = fixture
        .as_caller(&clerk_token, "GET", "/v1/hr/employees?scope=all", None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.name_a_branch")),
        "{body}"
    );
    let (status, body) = fixture
        .as_caller(&owner_token, "GET", "/v1/hr/employees?scope=all", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // **Two branches: the request has to say which.**
    let (status, body, _) = fixture
        .send(confine(serde_json::json!([olaya, malaz])))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture.send(receipt(&clerk_token, "RCV-WHICH", None)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.name_a_branch")),
        "{body}"
    );
    let (status, body, _) = fixture
        .send(receipt(&clerk_token, "RCV-MALAZ-2", Some(malaz.as_str())))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // **A key is bound like a person**, through its own membership.
    let (status, key) = fixture
        .as_caller(
            &owner_token,
            "POST",
            "/v1/keys",
            Some(serde_json::json!({ "name": "Till", "scopes": ["*:post_entries"], "role": "clerk" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let secret = key["secret"].as_str().expect("a secret").to_owned();
    let (status, body, _) = fixture
        .send(receipt(&secret, "RCV-KEY-FREE", Some(malaz.as_str())))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an unconfined key acts anywhere: {body}"
    );
    let (_, members) = fixture
        .as_caller(&owner_token, "GET", "/v1/members", None)
        .await;
    let machine: IdentityId = members
        .as_array()
        .expect("a list")
        .iter()
        .find(|m| m["handle"].is_null())
        .expect("the key's membership is listed")["identity"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("an identity");
    let (status, body, _) = fixture
        .send(
            Request::put(format!("/v1/members/{machine}/branches"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "branches": [olaya] }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture
        .send(receipt(&secret, "RCV-KEY-MALAZ", Some(malaz.as_str())))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.wrong_branch")),
        "a confined key acted outside its branch: {body}"
    );

    // **Lifted.**
    let (status, body, _) = fixture.send(confine(serde_json::json!([]))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(member_branches(clerk).await, serde_json::json!([]));
    let (status, body, _) = fixture
        .send(receipt(&clerk_token, "RCV-FREE", Some(malaz.as_str())))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    fixture.cleanup().await;
}

/// **Billing sets a company up once it has paid, and the owner chooses a
/// password at the link** — the closed-signup path (decided 2026-09-14).
/// Support may not; the link with no password is refused and stays live; a
/// short password is refused; the right one builds the company with the
/// modules asked for and signs the owner in. An owner whose address already
/// has an account proves that account's password instead, as at an
/// invitation. The request is on the platform record under the staff
/// member's name.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one company from the order to the owner signed in, and every refusal on the way"
)]
async fn billing_sets_a_company_up_and_the_owner_chooses_a_password_at_the_link() {
    let fixture = Fixture::new().await;
    let (billing, token, _) = fixture
        .staff("billing@erp.test", erp_control::PlatformRole::Billing)
        .await;
    let (_, support, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    let order = |slug: &str, email: &str| {
        Some(serde_json::json!({
            "slug": slug, "company": "Bassat Media Productions",
            "owner_email": email, "modules": ["ledger"]
        }))
    };

    let (status, body) = fixture
        .as_caller(
            &support,
            "POST",
            "/v1/platform/tenants",
            order("bassat", "owner@bassat.sa"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        body["args"]["capability"]["value"], "create_tenants",
        "{body}"
    );

    let (status, body) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/platform/tenants",
            order("bassat", "Owner@Bassat.sa"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["email"], "owner@bassat.sa", "lowercased, as stored");
    assert_eq!(body["slug"], "bassat");

    let link = fixture.confirmation("owner@bassat.sa").await;
    let confirm = |body: serde_json::Value| {
        Request::post(format!("/v1/signups/{link}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // No password: refused, and the link is not spent.
    let (status, body, _) = fixture.send(confirm(serde_json::json!({}))).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("signups.password_required")),
        "{body}"
    );
    let (status, body, _) = fixture
        .send(confirm(serde_json::json!({ "password": "short" })))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("request.password_too_short")),
        "{body}"
    );

    let (status, body, _) = fixture
        .send(confirm(
            serde_json::json!({ "password": "correct horse battery staple" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "bassat");
    assert_eq!(body["modules"][0], "ledger");
    let owner_token = body["token"].as_str().expect("a token").to_owned();
    let (status, tenant, _) = fixture
        .send(
            Request::get("/v1/tenant")
                .header(header::HOST, "bassat.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tenant}");
    // The password is theirs: it signs in on its own.
    fixture
        .token("owner@bassat.sa", "correct horse battery staple")
        .await;

    // On the platform record, under billing's name.
    let (actor, detail): (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_identity_id, detail FROM audit_entry
          WHERE action = 'signup.requested' AND subject_id = 'owner@bassat.sa'",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("the order is on the record");
    assert_eq!(actor, Some(billing.into_uuid()));
    assert_eq!(detail["slug"], "bassat");

    // **An owner whose address already has an account.** Staff cannot prove
    // it for them, so the link asks for that account's password.
    fixture.user("boss@najd.test", "hunter2hunter2").await;
    let (status, body) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/platform/tenants",
            order("najd", "boss@najd.test"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let link = fixture.confirmation("boss@najd.test").await;
    let confirm = |body: serde_json::Value| {
        Request::post(format!("/v1/signups/{link}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (status, body, _) = fixture.send(confirm(serde_json::json!({}))).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("signups.password_required")),
        "{body}"
    );
    let (status, body, _) = fixture
        .send(confirm(
            serde_json::json!({ "password": "not their password" }),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body, _) = fixture
        .send(confirm(serde_json::json!({ "password": "hunter2hunter2" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "najd");
    let accounts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM authenticator WHERE kind = 'password' AND handle = 'boss@najd.test'",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("counts");
    assert_eq!(accounts, 1, "the existing account was reused, not doubled");

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
/// **public** (`security: []`), one of the handful that need a session and no
/// tenant, or on the **platform** surface — which answers to a platform role
/// and has its own matrix, [`every_platform_role_against_every_platform_endpoint`].
/// The first two are written out, because a route that quietly joined them
/// would be a route this matrix stopped checking; the third cannot be joined
/// quietly, because that matrix refuses an untabled route the same way.
fn role_scoped_operations() -> Vec<(String, String, bool)> {
    /// Authenticated, and about the caller rather than a company.
    const NO_TENANT: &[&str] = &["log_out", "change_password"];

    operations()
        .into_iter()
        .filter(|(id, route, _, public)| {
            !public && !NO_TENANT.contains(&id.as_str()) && !is_platform(route)
        })
        .map(|(id, route, body, _)| (id, route, body))
        .collect()
}

/// Every operation on the platform surface, public or not — a public one is
/// exactly what the platform matrix is there to catch.
fn platform_operations() -> Vec<(String, String, bool)> {
    operations()
        .into_iter()
        .filter(|(_, route, _, _)| is_platform(route))
        .map(|(id, route, body, _)| (id, route, body))
        .collect()
}

fn is_platform(route: &str) -> bool {
    route
        .split_once(' ')
        .is_some_and(|(_, path)| path.starts_with("/v1/platform/"))
}

/// `(operationId, "METHOD /path", takes a body, is public)`, from the document.
fn operations() -> Vec<(String, String, bool, bool)> {
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
            found.push((
                id.to_owned(),
                format!("{} {path}", method.to_uppercase()),
                operation["requestBody"].is_object(),
                public,
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
    // Reading the policy is reading; setting it is ManageTenant, like every
    // other decision about who may be here.
    ("second_factor_policy", ALL_ROLES),
    // **Resetting somebody *else's* is not.** It is the owner's, or a member's
    // through `hr:reset_second_factor` — which this fixture grants nobody, so
    // every non-owner here is the 403 that names `manage_tenant`.
    ("reset_member_second_factor", OWNER),
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
    // Statements are reads of the books, like the trial balance.
    ("fiscal_calendar", ALL_ROLES),
    ("periods", ALL_ROLES),
    ("year", ALL_ROLES),
    ("closing_accounts", ALL_ROLES),
    ("balances", ALL_ROLES),
    ("profit_and_loss", ALL_ROLES),
    ("balance_sheet", ALL_ROLES),
    ("list_entries", ALL_ROLES),
    ("journal_entry", ALL_ROLES),
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
    // The forms a tariff can be written from. Shipped strings, so whoever may
    // read the tariff may read what it could have been written with.
    ("tariff_templates", ALL_ROLES),
    // The ready-made tariffs, and what installing one would do. Reading a pack
    // and previewing it are both reads: the preview writes nothing, which is
    // the same argument `preview_chart` makes.
    ("list_packs", ALL_ROLES),
    ("preview_pack", ALL_ROLES),
    // Installing one writes the tariff, so it is the capability that writes the
    // tariff — the same as `set_tariff`.
    ("install_pack", OWNER),
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
    // **What is on the shelf.** A viewer may read it: the person at the counter
    // has to be able to say "we have two left", the lots behind that number are
    // what says which batch and when it goes off, and the movements are what
    // makes the answer worth anything. Both settings are read like every other.
    // **The summary is those lists added up**, so it is read by whoever may read
    // them: a total withheld from somebody who can page through the rows and
    // add them protects nothing.
    ("list_products", ALL_ROLES),
    ("list_stock", ALL_ROLES),
    ("list_lots", ALL_ROLES),
    ("stock_summary", ALL_ROLES),
    ("list_movements", ALL_ROLES),
    ("stock_accounts", ALL_ROLES),
    ("expiry_window", ALL_ROLES),
    // Receiving a delivery, counting a lot and throwing away what has gone off
    // are recording what happened, which is the clerk's job — and a count is
    // what the books get corrected from, so it sits with the rest of
    // `post_entries`. **Writing off is there too, deliberately**: the person who
    // finds the milk past its date is the person holding it, and a loss nobody
    // may record is a loss that goes in the bin unrecorded.
    ("receive_stock", &["owner", "accountant", "clerk"]),
    ("count_stock", &["owner", "accountant", "clerk"]),
    ("write_off_stock", &["owner", "accountant", "clerk"]),
    // **Declaring a product is not.** The unit and the tracking mode are frozen
    // at declaration and every quantity ever recorded is a number in that unit,
    // so this is the shape of the business rather than a day's work in it — the
    // same call as `declare_bookable`.
    ("declare_product", OWNER),
    // Where stock will post is the shape of the books, like every other
    // `set_*_accounts`. How long before an expiry to warn is the shape of the
    // business, like the rest of `manage_tenant`.
    ("set_stock_accounts", &["owner", "accountant"]),
    ("set_expiry_window", OWNER),
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
    // **Reading what a chart *would* do is reading.** The preview writes
    // nothing — it runs the install against a transaction and rolls it back —
    // so it takes `Read`, unlike the install beside it.
    ("preview_chart", ALL_ROLES),
    ("set_posting_accounts", &["owner", "accountant"]),
    // Where a liability is held is the shape of the books, not a day's work.
    ("set_deferral_accounts", &["owner", "accountant"]),
    ("set_loyalty_scheme", &["owner", "accountant"]),
    ("set_till_accounts", &["owner", "accountant"]),
    // Declaring the numbers final is the accountant's call, and not
    // something a clerk posting entries should be able to do to them.
    ("close_period", &["owner", "accountant"]),
    ("reopen_period", &["owner", "accountant"]),
    ("close_year", &["owner", "accountant"]),
    ("reopen_year", &["owner", "accountant"]),
    ("set_closing_accounts", &["owner", "accountant"]),
    // The calendar is the accountant's call, like closing the books.
    ("set_fiscal_calendar", &["owner", "accountant"]),
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
    // Requiring two-step sign-in is a decision about who may be here, which is
    // the same authority as adding somebody or changing their role.
    ("set_second_factor_policy", OWNER),
    ("remove_member", OWNER),
    ("set_module_role", OWNER),
    ("clear_module_role", OWNER),
    // Which branches somebody belongs to is who may be where, which is the
    // same authority as who may be here at all.
    ("set_member_branches", OWNER),
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
    // Not through `Allowed`, and so answering while the tenant is suspended —
    // but the same role decides, and the same 403 names it.
    ("audit_trail", OWNER),
    // About the caller, like the second factor: every role reads their own.
    ("my_audit_trail", ALL_ROLES),
    // Owner-only to read as well as to write (decision 9 of 2026-09-11). No
    // limit narrows either — see `no_limit_locks_the_owner_out_of_its_limits`.
    ("permission_limits", OWNER),
    ("set_permission_limits", OWNER),
    // The same decision for the same kind of setting: how far anybody else
    // may go is the owner's to read and to write.
    ("document_limit", OWNER),
    ("set_document_limit", OWNER),
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
        275,
        "expected two hundred and sixty-nine role-scoped operations"
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
                // Also parsed — and 2026 has an open period, so the owner's
                // year close is a 409 rather than a booking mid-matrix.
                .replace("{year}", "2026")
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

/// `(operationId, the platform power it needs)`. The platform surface's
/// `PERMISSIONS`: every operation under `/v1/platform/` is named here.
const PLATFORM: &[(&str, &str)] = &[
    ("list_staff", "manage_staff"),
    ("grant_staff", "manage_staff"),
    ("change_staff_role", "manage_staff"),
    ("revoke_staff", "manage_staff"),
    ("suspend_tenant", "suspend_tenants"),
    ("reinstate_tenant", "suspend_tenants"),
    ("create_tenant", "create_tenants"),
    ("list_control_dead_letters", "handle_dead_letters"),
    ("requeue_control_dead_letter", "handle_dead_letters"),
    ("dismiss_control_dead_letter", "handle_dead_letters"),
    ("platform_audit_trail", "read_audit_trail"),
    ("reset_any_second_factor", "reset_second_factors"),
];

/// `(platform role, the powers it holds)` — the product owner's matrix of
/// 2026-09-11, typed out rather than read from `PlatformRole::may`, so the
/// doors are checked against what was meant and not against themselves.
const STAFF_POWERS: &[(&str, &[&str])] = &[
    (
        "superadmin",
        &[
            "suspend_tenants",
            "create_tenants",
            "handle_dead_letters",
            "read_audit_trail",
            "enter_for_support",
            "manage_staff",
            "reset_second_factors",
        ],
    ),
    ("billing", &["suspend_tenants", "create_tenants"]),
    (
        "support",
        &[
            "read_audit_trail",
            "handle_dead_letters",
            "enter_for_support",
            "reset_second_factors",
        ],
    ),
];

/// **The platform matrix, over HTTP: every staff role against every platform
/// route** — and the two callers who must never get in: a tenant's owner who is
/// not staff, and a superadmin with no second factor.
///
/// The routes come from the document, as the tenant matrix's do, so a platform
/// route added without a row in `PLATFORM` fails here rather than going
/// unchecked. The expected answer comes from `STAFF_POWERS`, which is what makes
/// this the test that the doors honour the matrix.
#[expect(
    clippy::too_many_lines,
    reason = "one matrix, and the two callers outside it, against the same routes"
)]
#[tokio::test]
async fn every_platform_role_against_every_platform_endpoint() {
    use erp_control::{PlatformPower, PlatformRole};
    use std::collections::BTreeSet;

    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let endpoints = platform_operations();

    let served: BTreeSet<&str> = endpoints.iter().map(|(id, _, _)| id.as_str()).collect();
    let tabled: BTreeSet<&str> = PLATFORM.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        served,
        tabled,
        "the platform table and the routes disagree. Served and untabled: {:?}. \
         Tabled and unserved: {:?}.",
        served.difference(&tabled).collect::<Vec<_>>(),
        tabled.difference(&served).collect::<Vec<_>>(),
    );
    assert_eq!(served.len(), 12, "expected twelve platform operations");
    // The two tables speak the product's vocabulary, all of it.
    let powers: BTreeSet<&str> = PlatformPower::ALL.map(PlatformPower::as_str).into();
    let roles: BTreeSet<&str> = PlatformRole::ALL.map(PlatformRole::as_str).into();
    assert_eq!(
        STAFF_POWERS
            .iter()
            .map(|(role, _)| *role)
            .collect::<BTreeSet<_>>(),
        roles
    );
    for power in PLATFORM.iter().map(|(_, p)| *p).chain(
        STAFF_POWERS
            .iter()
            .flat_map(|(_, held)| held.iter().copied()),
    ) {
        assert!(powers.contains(power), "{power} is not a platform power");
    }

    // Real and not staff, so `{identity}` reaches the handler's own answer — a
    // 404 — rather than the uuid parser, and nothing is changed. `{id}` is the
    // active tenant: an empty suspension is refused for its missing reason,
    // reinstating one that is not suspended is a 409, and as a dead letter's id
    // it is a 400.
    let subject = fixture.user("subject@erp.test", "hunter2hunter2").await;

    let mut callers: Vec<(String, String, &[&str])> = Vec::new();
    for (role, held) in STAFF_POWERS {
        let email = format!("{role}@erp.test");
        let (_, token, _) = fixture
            .staff(&email, role.parse().expect("a platform role"))
            .await;
        callers.push(((*role).to_owned(), token, held));
    }
    // Signed in with both factors, and owner of a tenant: still nobody here.
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let (owner_token, _) = fixture.enrolled_token(owner, "owner@acme.test").await;
    callers.push(("a tenant's owner".to_owned(), owner_token, &[]));

    let request = |route: &str| {
        let (method, template) = route.split_once(' ').expect("method and path");
        (
            method.to_owned(),
            template
                .replace("{identity}", &subject.to_string())
                .replace("{id}", &tenant.to_string()),
        )
    };

    for (who, token, held) in &callers {
        for (id, route, takes_a_body) in &endpoints {
            let (method, path) = request(route);
            let power = PLATFORM
                .iter()
                .find(|(operation, _)| operation == id)
                .map(|(_, power)| *power)
                .expect("every operation is in the table; checked above");
            let may = held.contains(&power);

            let (status, answer) = fixture
                .as_caller(
                    token,
                    &method,
                    &path,
                    takes_a_body.then(|| serde_json::json!({})),
                )
                .await;
            assert_eq!(
                status != StatusCode::FORBIDDEN,
                may,
                "{who} → {method} {path} ({id}) answered {status}, and the table says \
                 {}. Body: {answer}",
                if may { "allowed" } else { "refused" }
            );
            if !may {
                assert_eq!(answer["code"], "access.not_permitted", "{who} → {id}");
                assert_eq!(
                    answer["args"]["capability"]["value"], power,
                    "{who} → {id}: the 403 does not name the power"
                );
            }
        }
    }

    // **Staff cannot turn their factor off**, even proving it: a staff account
    // with no factor takes its next one from whoever enrols first.
    let (_, keeper, recovery) = fixture
        .staff("keeper@erp.test", PlatformRole::Superadmin)
        .await;
    let (status, body) = fixture
        .as_caller(
            &keeper,
            "DELETE",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "code": recovery[0] })),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("auth.staff_keeps_second_factor")
        ),
        "{body}"
    );
    let (status, _) = fixture
        .as_caller(&keeper, "GET", "/v1/platform/staff", None)
        .await;
    assert_eq!(status, StatusCode::OK, "the refusal took the factor anyway");

    // **A superadmin without a second factor is refused everywhere**, with the
    // one code that tells them what to do. `grant_staff` and the refusal above
    // leave one way to be that: `grant_membership` called directly.
    let lapsed_id = fixture.user("lapsed@erp.test", "hunter2hunter2").await;
    fixture
        .control
        .grant_membership(lapsed_id, Scope::Platform, "superadmin", Actor::system())
        .await
        .expect("grants");
    let lapsed = fixture.token("lapsed@erp.test", "hunter2hunter2").await;
    for (id, route, takes_a_body) in &endpoints {
        let (method, path) = request(route);
        let (status, answer) = fixture
            .as_caller(
                &lapsed,
                &method,
                &path,
                takes_a_body.then(|| serde_json::json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "lapsed → {id}: {answer}");
        assert_eq!(
            answer["code"], "access.staff_second_factor_required",
            "lapsed → {id}"
        );
    }

    fixture.cleanup().await;
}

/// **A session from before the factor does not outlive it.** The door asks
/// whether a factor is enrolled, not whether this session went through it, so
/// a password-only session from before enrolment — somebody who phished the
/// password at nine, before the owner enrolled at ten — would pass it once the
/// owner is made staff. Confirming an enrolment ends it; the session that
/// confirmed, which just proved the factor, stays.
#[tokio::test]
async fn a_session_from_before_the_factor_does_not_reach_a_platform_door() {
    let fixture = Fixture::new().await;
    fixture
        .staff("admin@erp.test", erp_control::PlatformRole::Superadmin)
        .await;
    fixture.user("pre@erp.test", "hunter2hunter2").await;
    let phished = fixture.token("pre@erp.test", "hunter2hunter2").await;
    let own = fixture.token("pre@erp.test", "hunter2hunter2").await;

    let (status, started) = fixture
        .as_caller(&own, "POST", "/v1/sessions/second-factor", None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let secret =
        erp_control::totp::unbase32(started["secret"].as_str().expect("a secret")).expect("base32");
    let seconds = u64::try_from(chrono::Utc::now().timestamp()).expect("after 1970");
    let code =
        erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS).expect("a code");
    let (status, body) = fixture
        .as_caller(
            &own,
            "POST",
            "/v1/sessions/second-factor/confirmation",
            Some(serde_json::json!({ "code": code })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    fixture
        .control
        .grant_staff(
            "pre@erp.test",
            erp_control::PlatformRole::Superadmin,
            Actor::system(),
        )
        .await
        .expect("granted, factor and all");

    let (status, body) = fixture
        .as_caller(&phished, "GET", "/v1/platform/staff", None)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a session that never met the factor reached a platform door: {body}"
    );
    let (status, body) = fixture
        .as_caller(&own, "GET", "/v1/platform/staff", None)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the confirming session was ended: {body}"
    );

    fixture.cleanup().await;
}

/// **A second factor a company requires can be replaced, never removed.** An
/// account with a password and no factor takes its next factor from whoever
/// enrols first, so a member who could drop theirs would reopen the gap the
/// owner turned the requirement on to close. The owner removing the member, or
/// switching it off, gives removal back; a company that requires nothing never
/// took it.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one account through every state the rule has: required, replaced, removed from the company, and switched off"
)]
async fn a_second_factor_a_company_requires_is_replaced_never_removed() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let (owner_token, owner_paper) = fixture.enrolled_token(owner, "owner@acme.test").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(clerk, acme, "clerk").await;
    fixture.join_as(clerk, globex, "viewer").await;
    let (clerk_token, paper) = fixture.enrolled_token(clerk, "clerk@acme.test").await;
    let off = |code: &str| Some(serde_json::json!({ "code": code }));
    let policy = |required: bool| Some(serde_json::json!({ "required": required }));

    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "PUT",
            "/v1/members/second-factor-policy",
            policy(true),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Refused with a code that is right, and the code is not spent on it.
    let (status, body) = fixture
        .as_caller(
            &clerk_token,
            "DELETE",
            "/v1/sessions/second-factor",
            off(&paper[0]),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("auth.tenant_keeps_second_factor")
        ),
        "{body}"
    );
    let (_, view) = fixture
        .as_caller(&clerk_token, "GET", "/v1/sessions/second-factor", None)
        .await;
    assert_eq!(
        (
            view["enrolled"].as_bool(),
            view["recovery_codes_left"].as_i64()
        ),
        (Some(true), Some(10)),
        "the refusal took the factor or spent the code: {view}"
    );

    // Replacing it is open — proved by the code the refusal left unspent.
    let (status, started) = fixture
        .as_caller(&clerk_token, "POST", "/v1/sessions/second-factor", None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let secret =
        erp_control::totp::unbase32(started["secret"].as_str().expect("a secret")).expect("base32");
    let seconds = u64::try_from(chrono::Utc::now().timestamp()).expect("after 1970");
    let code =
        erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS).expect("a code");
    let (status, replaced) = fixture
        .as_caller(
            &clerk_token,
            "POST",
            "/v1/sessions/second-factor/confirmation",
            Some(serde_json::json!({ "code": code, "previous": paper[0] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{replaced}");
    let new_paper = replaced["recovery_codes"][0]
        .as_str()
        .expect("new recovery codes")
        .to_owned();

    // The owner is a member too, and holds to their own rule.
    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "DELETE",
            "/v1/sessions/second-factor",
            off(&owner_paper[0]),
        )
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("auth.tenant_keeps_second_factor")
        ),
        "{body}"
    );

    // Out of acme, the clerk is left in globex, which requires nothing.
    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "DELETE",
            &format!("/v1/members/{clerk}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = fixture
        .as_caller(
            &clerk_token,
            "DELETE",
            "/v1/sessions/second-factor",
            off(&new_paper),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "out of acme, left only in globex, which requires nothing, and still refused: {body}"
    );

    // The owner stops requiring it, and may drop their own.
    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "PUT",
            "/v1/members/second-factor-policy",
            policy(false),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "DELETE",
            "/v1/sessions/second-factor",
            off(&owner_paper[0]),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "a requirement switched off still binds: {body}"
    );

    fixture.cleanup().await;
}

/// **A person's factor is reset at most three times an hour, whoever asks.**
///
/// Every reset ends every session the person holds and mails them, so an
/// unlimited route was a way to keep a colleague signed out and fill their
/// inbox — by their owner, or by support. One budget per target, charged by
/// the tenant route and the platform route alike; the fourth is 429 with the
/// seconds to wait.
#[tokio::test]
async fn a_persons_factor_is_reset_at_most_three_times_an_hour() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let (owner_token, _) = fixture.enrolled_token(owner, "owner@acme.test").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(clerk, acme, "clerk").await;
    let (_, _paper) = fixture.enrolled_token(clerk, "clerk@acme.test").await;

    let reset = format!("/v1/members/{clerk}/second-factor-reset");
    for attempt in 1..=3 {
        let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "reset {attempt}: {body}");
    }
    let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "request.too_many_requests", "{body}");
    assert!(
        body["args"]["seconds"]["value"]
            .as_i64()
            .is_some_and(|s| s > 0),
        "the refusal does not say how long: {body}"
    );

    // **The same budget from the platform.** Support resetting the same person
    // a moment later is the fourth reset of that person, not the first of
    // support's.
    let (_, support, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    let (status, body) = fixture
        .as_caller(
            &support,
            "POST",
            &format!("/v1/platform/identities/{clerk}/second-factor-reset"),
            Some(serde_json::json!({ "reason": "Ticket 4472: still cannot sign in." })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the platform route has its own budget for the same person: {body}"
    );

    fixture.cleanup().await;
}

/// **The owner resets a member who lost both phone and paper, and the emailed
/// link is the only way back.**
///
/// Everything the reset promises, in one account's life: the factor and the
/// recovery codes go, every session ends, the tenant that requires a factor
/// refuses them until they enrol again, and **the password alone cannot enrol**
/// — not at `begin`, not at `confirm`. The link works once; a second reset
/// mints another, and the first one is dead.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one account from lost phone to enrolled again, and every door it meets on the way"
)]
async fn an_owner_resets_a_members_factor_and_the_link_is_the_only_way_back() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let (owner_token, _) = fixture.enrolled_token(owner, "owner@acme.test").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(clerk, acme, "clerk").await;
    let (lost, _paper) = fixture.enrolled_token(clerk, "clerk@acme.test").await;

    // The company requires two-step sign-in, which is the case this is for: a
    // member who cannot present a factor cannot work until they have one.
    let (status, body) = fixture
        .as_caller(
            &owner_token,
            "PUT",
            "/v1/members/second-factor-policy",
            Some(serde_json::json!({ "required": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = fixture.as_caller(&lost, "GET", "/v1/members", None).await;
    assert_eq!(status, StatusCode::OK, "the clerk was working before this");

    let reset = format!("/v1/members/{clerk}/second-factor-reset");
    let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // **The session really ended**, here and on every other node — there is one
    // node in a test, so this is the database and the cache agreeing.
    let (status, body) = fixture.as_caller(&lost, "GET", "/v1/members", None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the reset left a session alive: {body}"
    );

    // The password still signs in — this is not a password reset — and the
    // factor is gone.
    let token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    let (_, view) = fixture
        .as_caller(&token, "GET", "/v1/sessions/second-factor", None)
        .await;
    assert_eq!(
        (
            view["enrolled"].as_bool(),
            view["recovery_codes_left"].as_i64()
        ),
        (Some(false), Some(0)),
        "the factor or its recovery codes survived the reset: {view}"
    );

    // **And the company will not have them until they enrol again** — the same
    // answer a member who never enrolled gets, which is what it now is.
    let (status, body) = fixture.as_caller(&token, "GET", "/v1/members", None).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("auth.tenant_requires_second_factor")
        ),
        "{body}"
    );

    // **The password alone enrols nothing**, at either half.
    for (method, path, body) in [
        ("POST", "/v1/sessions/second-factor", serde_json::json!({})),
        (
            "POST",
            "/v1/sessions/second-factor/confirmation",
            serde_json::json!({ "code": "000000" }),
        ),
    ] {
        let (status, answer) = fixture.as_caller(&token, method, path, Some(body)).await;
        assert_eq!(
            (status, answer["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("auth.enrolment_link_required")),
            "{method} {path}: {answer}"
        );
    }

    // The mail is the mailbox's, and the token is in it.
    let link = fixture.enrolment_link("clerk@acme.test").await;
    let (status, started) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "link": link })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let secret =
        erp_control::totp::unbase32(started["secret"].as_str().expect("a secret")).expect("base32");
    let seconds = u64::try_from(chrono::Utc::now().timestamp()).expect("after 1970");
    let code =
        erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS).expect("a code");

    // Confirming without the link is still refused, even holding the code.
    let (status, answer) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor/confirmation",
            Some(serde_json::json!({ "code": code })),
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("auth.enrolment_link_required")),
        "{answer}"
    );

    let (status, enrolled) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor/confirmation",
            Some(serde_json::json!({ "code": code, "link": link })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{enrolled}");
    assert_eq!(
        enrolled["recovery_codes"].as_array().map(Vec::len),
        Some(10),
        "a fresh sheet of recovery codes: {enrolled}"
    );

    // Back at work, with both factors.
    let code =
        erp_control::totp::code_at(&secret, seconds, erp_control::totp::DIGITS).expect("a code");
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "handle": "clerk@acme.test",
                        "password": "hunter2hunter2",
                        "code": code
                    })
                    .to_string(),
                ))
                .expect("request builds"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let back = body["token"].as_str().expect("a token").to_owned();
    let (status, body) = fixture.as_caller(&back, "GET", "/v1/members", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // **And the account is on ordinary rules again.** Enrolling cleared the
    // link-only state, so starting a replacement needs no link — only the old
    // code, at the confirmation, as it always did.
    let (status, started) = fixture
        .as_caller(&back, "POST", "/v1/sessions/second-factor", None)
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the link-only state outlived the enrolment that was supposed to clear it: {started}"
    );

    // **A link is good once.** A second reset mints another; the first is dead
    // whichever way round the two are tried.
    let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let again = fixture.enrolment_link("clerk@acme.test").await;
    assert_ne!(again, link, "the same token was mailed twice");
    let token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    let (status, answer) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "link": link })),
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("auth.enrolment_link_required")),
        "a spent link enrolled again: {answer}"
    );
    let (status, started) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "link": again })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "the fresh link: {started}");

    // **The trail names the resetter and the person**, in the tenant's own.
    let (status, trail) = fixture
        .as_caller(&owner_token, "GET", "/v1/audit", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{trail}");
    let entries: Vec<&serde_json::Value> = trail["items"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["action"] == "second_factor.reset")
        .collect();
    assert_eq!(entries.len(), 2, "two resets: {trail}");
    assert_eq!(entries[0]["actor"], owner.to_string(), "{trail}");
    assert_eq!(entries[0]["subject_id"], clerk.to_string(), "{trail}");
    assert_eq!(entries[0]["detail"]["by"], "member", "{trail}");

    fixture.cleanup().await;
}

/// **Waiting the link out does not give the password its old power back.**
///
/// The whole reason the link-only state is a fact about the account rather than
/// the life of a row: if it lapsed with the link, the move for somebody holding
/// a stolen password would be to wait an hour and then enrol. It does not, and
/// the sweep that deletes the expired row does not change that — it only means
/// asking for another.
#[tokio::test]
async fn waiting_out_an_enrolment_link_does_not_reopen_password_only_enrolment() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(clerk, acme, "clerk").await;
    fixture.enrolled_token(clerk, "clerk@acme.test").await;

    let reset = format!("/v1/members/{clerk}/second-factor-reset");
    let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let stale = fixture.enrolment_link("clerk@acme.test").await;

    // An hour later. The clock is wound back rather than waited out, the way
    // `an_expired_session_is_swept` does it.
    sqlx::query("UPDATE second_factor_reset SET expires_at = now() - interval '1 minute'")
        .execute(fixture.control.pool())
        .await
        .expect("winds the link back");
    assert_eq!(
        fixture
            .control
            .sweep_enrolment_links()
            .await
            .expect("sweeps"),
        1,
        "the expired link"
    );

    let token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    for body in [serde_json::json!({}), serde_json::json!({ "link": stale })] {
        let (status, answer) = fixture
            .as_caller(&token, "POST", "/v1/sessions/second-factor", Some(body))
            .await;
        assert_eq!(
            (status, answer["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("auth.enrolment_link_required")),
            "expiring the link reopened enrolment: {answer}"
        );
    }

    // A fresh one is the owner running the same route again. There is no
    // second route, because a fresh link is the same act.
    let (status, body) = fixture.as_caller(&owner_token, "POST", &reset, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let fresh = fixture.enrolment_link("clerk@acme.test").await;
    // Signed in again, because the second reset ended this session too.
    let token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    let (status, started) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "link": fresh })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");

    fixture.cleanup().await;
}

/// **The claim lets somebody other than the owner reset a colleague's factor,
/// and it travels up the chart.**
///
/// Boss ← supervisor ← clerk, as §68's claim test has it: a grant to the
/// supervisor reaches the boss and not the clerk. A member with no employee
/// record holds nothing and is refused, and neither the owner's factor nor
/// your own can be reset here whoever you are.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "three places on one org chart, and the four refusals, against one route"
)]
async fn the_claim_lets_somebody_other_than_the_owner_reset_a_factor() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    fixture.enable_module(acme, hr::setup()).await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let mut people = std::collections::BTreeMap::new();
    for (who, employee, above) in [
        ("boss", "EMP-1", None),
        ("supervisor", "EMP-2", Some("EMP-1")),
        ("clerk", "EMP-3", Some("EMP-2")),
    ] {
        let email = format!("{who}@acme.test");
        let identity = fixture.user(&email, "hunter2hunter2").await;
        fixture.join_as(identity, acme, "clerk").await;
        fixture.hire(acme, employee, &email, identity, above).await;
        let token = fixture.token(&email, "hunter2hunter2").await;
        people.insert(who, (identity, token));
    }
    // The person who lost their phone. Not on the chart: this is a control
    // about who may *do* the reset, not about who may be reset.
    let victim = fixture.user("victim@acme.test", "hunter2hunter2").await;
    fixture.join_as(victim, acme, "clerk").await;
    let (victim_session, _) = fixture.enrolled_token(victim, "victim@acme.test").await;
    let reset = format!("/v1/members/{victim}/second-factor-reset");

    // **A key is not a person, whatever it is scoped for** — and the dangerous
    // scope is the *narrow* one. The scope gate asks for the door's capability,
    // and this door is `Allowed<Read>`, so a reporting credential issued
    // `*:read` with the owner's role walks through the gate that answers
    // `keys.out_of_scope` at `DELETE /v1/members/…`, and the owner's role its
    // machine identity holds is all the handler would otherwise ask for. It is
    // stopped here instead. A key scoped `*:manage_tenant` never reaches that
    // far: `Read` is not its capability, so the gate refuses it first. No scope
    // is the way in, because this is not a scope question.
    for (scopes, code) in [
        (["*:read"], "keys.not_a_person"),
        (["*:manage_tenant"], "keys.out_of_scope"),
    ] {
        let (status, key) = fixture
            .as_caller(
                &owner_token,
                "POST",
                "/v1/keys",
                Some(serde_json::json!({
                    "name": format!("Reporting {}", scopes[0]),
                    "scopes": scopes,
                    "role": "owner"
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{key}");
        let secret = key["secret"].as_str().expect("a secret").to_owned();
        let (status, answer) = fixture.as_caller(&secret, "POST", &reset, None).await;
        assert_eq!(
            (status, answer["code"].as_str()),
            (StatusCode::FORBIDDEN, Some(code)),
            "a key scoped {scopes:?} reset a colleague's factor: {answer}"
        );
    }
    // And nothing happened to them on the way: the session a reset would have
    // ended is still working.
    let (status, body) = fixture
        .as_caller(&victim_session, "GET", "/v1/members", None)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a key's refused reset still ended the target's session: {body}"
    );

    // Nobody has granted anything, so nobody but the owner may.
    for who in ["boss", "supervisor", "clerk"] {
        let (status, answer) = fixture
            .as_caller(&people[who].1, "POST", &reset, None)
            .await;
        assert_eq!(
            (
                status,
                answer["code"].as_str(),
                answer["args"]["capability"]["value"].as_str()
            ),
            (
                StatusCode::FORBIDDEN,
                Some("access.not_permitted"),
                Some("manage_tenant")
            ),
            "{who} reset a factor with no claim: {answer}"
        );
    }

    // Granted to the supervisor. It reaches the boss above them and stops
    // above the clerk beneath.
    fixture
        .grant_claim(acme, "EMP-2", hr::RESET_SECOND_FACTOR)
        .await;
    for who in ["supervisor", "boss"] {
        let (status, body) = fixture
            .as_caller(&people[who].1, "POST", &reset, None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{who} was refused: {body}");
        fixture.enrolment_link("victim@acme.test").await;
    }
    let (status, answer) = fixture
        .as_caller(&people["clerk"].1, "POST", &reset, None)
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.not_permitted")),
        "a claim travelled down the chart: {answer}"
    );

    // A member with no employee record can hold no claim, whatever else they
    // are — and the owner is exempt from needing one.
    let outsider = fixture.user("outsider@acme.test", "hunter2hunter2").await;
    fixture.join_as(outsider, acme, "accountant").await;
    let outsider_token = fixture.token("outsider@acme.test", "hunter2hunter2").await;
    let (status, answer) = fixture
        .as_caller(&outsider_token, "POST", &reset, None)
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.not_permitted")),
        "somebody off the org chart held a claim: {answer}"
    );

    // **The owner's factor is not a member's to reset**, claim or not.
    let (status, answer) = fixture
        .as_caller(
            &people["supervisor"].1,
            "POST",
            &format!("/v1/members/{owner}/second-factor-reset"),
            None,
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("second_factor.reset_the_owner")
        ),
        "{answer}"
    );

    // **Nor your own**, which would be a way round the rule that removing a
    // factor costs a code.
    for (who, token) in [
        ("the supervisor", &people["supervisor"].1),
        ("the owner", &owner_token),
    ] {
        let subject = if who == "the owner" {
            owner
        } else {
            people["supervisor"].0
        };
        let (status, answer) = fixture
            .as_caller(
                token,
                "POST",
                &format!("/v1/members/{subject}/second-factor-reset"),
                None,
            )
            .await;
        assert_eq!(
            (status, answer["code"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("second_factor.reset_yourself")
            ),
            "{who} reset their own: {answer}"
        );
    }

    // Somebody who is not here at all is the 404 every member route gives.
    let stranger = fixture
        .user("stranger@nowhere.test", "hunter2hunter2")
        .await;
    let (status, answer) = fixture
        .as_caller(
            &owner_token,
            "POST",
            &format!("/v1/members/{stranger}/second-factor-reset"),
            None,
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("members.not_a_member")),
        "{answer}"
    );

    fixture.cleanup().await;
}

/// **Somebody who works for two companies is platform support's to reset**, and
/// neither company's — two-step sign-in is their account's everywhere, and one
/// company weakening it would weaken it at the other.
///
/// The platform route also holds the line inside the staff: support may reset
/// anybody but staff, and staff takes `manage_staff`, which only a superadmin
/// has. Otherwise the narrower role would be the way to the wider one.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the cross-tenant refusal and the platform route that answers it, including its own escalation"
)]
async fn somebody_who_works_for_two_companies_is_platform_supports_to_reset() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Two jobs, one account.
    let both = fixture.user("both@acme.test", "hunter2hunter2").await;
    fixture.join_as(both, acme, "clerk").await;
    fixture.join_as(both, globex, "viewer").await;
    fixture.enrolled_token(both, "both@acme.test").await;

    let (status, answer) = fixture
        .as_caller(
            &owner_token,
            "POST",
            &format!("/v1/members/{both}/second-factor-reset"),
            None,
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("second_factor.reset_another_company")
        ),
        "{answer}"
    );
    assert!(
        answer["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("support"),
        "the refusal does not send them to support: {answer}"
    );

    let (support_id, support, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    let (admin_id, admin, _) = fixture
        .staff("admin@erp.test", erp_control::PlatformRole::Superadmin)
        .await;
    let platform =
        |identity: IdentityId| format!("/v1/platform/identities/{identity}/second-factor-reset");

    // The reason is not optional, and it is not blank either. Missing is the
    // body layer's 422; blank is this route's own 400, which is the one that
    // says what a reason is for.
    let (status, answer) = fixture
        .as_caller(
            &support,
            "POST",
            &platform(both),
            Some(serde_json::json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    let (status, answer) = fixture
        .as_caller(
            &support,
            "POST",
            &platform(both),
            Some(serde_json::json!({ "reason": "   " })),
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("second_factor.reset_reason")),
        "{answer}"
    );

    let (status, body) = fixture
        .as_caller(
            &support,
            "POST",
            &platform(both),
            Some(serde_json::json!({ "reason": "Ticket 4471: lost phone, identity checked by video call." })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let link = fixture.enrolment_link("both@acme.test").await;
    let token = fixture.token("both@acme.test", "hunter2hunter2").await;
    let (status, started) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/sessions/second-factor",
            Some(serde_json::json!({ "link": link })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");

    // **The reason is on the record, under the staff member's name**, and the
    // entry belongs to no tenant.
    let (status, trail) = fixture
        .as_caller(&admin, "GET", "/v1/platform/audit", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{trail}");
    let entry = trail["items"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["action"] == "second_factor.reset")
        .unwrap_or_else(|| panic!("no reset in the platform trail: {trail}"));
    assert_eq!(entry["actor"], support_id.to_string(), "{entry}");
    assert_eq!(entry["subject_id"], both.to_string(), "{entry}");
    assert_eq!(entry["tenant"], serde_json::Value::Null, "{entry}");
    assert_eq!(
        entry["detail"]["reason"], "Ticket 4471: lost phone, identity checked by video call.",
        "{entry}"
    );

    // **Support cannot reset a superadmin's**, which is the escalation this
    // power would otherwise be.
    let (status, answer) = fixture
        .as_caller(
            &support,
            "POST",
            &platform(admin_id),
            Some(serde_json::json!({ "reason": "Ticket 4472." })),
        )
        .await;
    assert_eq!(
        (
            status,
            answer["code"].as_str(),
            answer["args"]["capability"]["value"].as_str()
        ),
        (
            StatusCode::FORBIDDEN,
            Some("access.not_permitted"),
            Some("manage_staff")
        ),
        "{answer}"
    );
    // A superadmin can, which is what makes the refusal about the power and
    // not about staff being untouchable.
    let (status, body) = fixture
        .as_caller(
            &admin,
            "POST",
            &platform(support_id),
            Some(serde_json::json!({ "reason": "Ticket 4473: support's own phone." })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // **Nobody resets their own here either**, or staff would have the way
    // round `auth.staff_keeps_second_factor` that route refuses them.
    let (status, answer) = fixture
        .as_caller(
            &admin,
            "POST",
            &platform(admin_id),
            Some(serde_json::json!({ "reason": "Ticket 4474." })),
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("second_factor.reset_yourself")
        ),
        "{answer}"
    );

    // **An account with no email login is refused, not reset.** There would be
    // nowhere to send the link, and a reset with no link is a lockout (L6).
    let voiceless = fixture
        .control
        .create_identity(Actor::system())
        .await
        .expect("an identity")
        .id;
    let (status, answer) = fixture
        .as_caller(
            &admin,
            "POST",
            &platform(voiceless),
            Some(serde_json::json!({ "reason": "Ticket 4475." })),
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("second_factor.reset_no_login")
        ),
        "{answer}"
    );

    // And a tenant cannot reach staff through its own route, even for
    // somebody who is one of its members.
    let staffer = fixture.user("staffer@acme.test", "hunter2hunter2").await;
    fixture.join_as(staffer, acme, "clerk").await;
    fixture.enrolled_token(staffer, "staffer@acme.test").await;
    fixture
        .control
        .grant_staff(
            "staffer@acme.test",
            erp_control::PlatformRole::Support,
            Actor::system(),
        )
        .await
        .expect("staff are granted");
    let (status, answer) = fixture
        .as_caller(
            &owner_token,
            "POST",
            &format!("/v1/members/{staffer}/second-factor-reset"),
            None,
        )
        .await;
    assert_eq!(
        (status, answer["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("second_factor.reset_platform_staff")
        ),
        "{answer}"
    );

    fixture.cleanup().await;
}

/// **Billing suspends a tenant over HTTP, and its owner is shut out on the next
/// request** — the same `access.tenant_unavailable` anybody gets — until it is
/// reinstated. The suspension is on the record under the staff member's name,
/// with the reason; a repeat is a 409 naming the status, not a quiet success.
#[tokio::test]
async fn billing_suspends_and_reinstates_a_tenant_under_their_own_name() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (billing, token, _) = fixture
        .staff("billing@erp.test", erp_control::PlatformRole::Billing)
        .await;
    let suspend = format!("/v1/platform/tenants/{tenant}/suspend");
    let reinstate = format!("/v1/platform/tenants/{tenant}/reinstate");
    let reason = |text: &str| Some(serde_json::json!({ "reason": text }));

    let (status, _) = fixture
        .as_caller(&owner_token, "GET", "/v1/tenant", None)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner is in, and acme is cached"
    );

    let (status, body) = fixture
        .as_caller(&token, "POST", &suspend, reason("  "))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("tenants.suspension_reason")),
        "{body}"
    );

    let (status, body) = fixture
        .as_caller(
            &token,
            "POST",
            &suspend,
            reason("The August invoice is unpaid."),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = fixture
        .as_caller(&owner_token, "GET", "/v1/tenant", None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("access.tenant_unavailable")
        ),
        "{body}"
    );

    let (status, body) = fixture
        .as_caller(&token, "POST", &suspend, reason("again"))
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("tenants.wrong_status")),
        "{body}"
    );
    // Still draining: the worker has not found its documents reported yet.
    assert_eq!(body["args"]["status"]["value"], "suspending", "{body}");

    let (actor, detail): (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_identity_id, detail FROM audit_entry
          WHERE action = 'tenant.suspended' AND subject_id = $1",
    )
    .bind(tenant.to_string())
    .fetch_one(fixture.control.pool())
    .await
    .expect("the suspension is on the record");
    assert_eq!(actor, Some(billing.into_uuid()));
    assert_eq!(detail["reason"], "The August invoice is unpaid.");

    let (status, body) = fixture.as_caller(&token, "POST", &reinstate, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = fixture
        .as_caller(&owner_token, "GET", "/v1/tenant", None)
        .await;
    assert_eq!(status, StatusCode::OK, "reinstated, and still shut: {body}");
    let (status, body) = fixture.as_caller(&token, "POST", &reinstate, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = fixture
        .as_caller(
            &token,
            "POST",
            &format!("/v1/platform/tenants/{}/reinstate", TenantId::new()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    fixture.cleanup().await;
}

/// A relay that refuses every email, so one dispatch makes real dead letters.
struct RefusingRelay;

#[async_trait::async_trait]
impl erp_eventlog::EffectHandler for RefusingRelay {
    fn kind(&self) -> erp_types::EffectKind {
        erp_control::mail::email_kind()
    }

    async fn deliver(
        &self,
        _effect: &erp_eventlog::PendingEffect,
    ) -> Result<(), erp_eventlog::DeliveryError> {
        Err(erp_eventlog::DeliveryError::Permanent(
            "550 no such mailbox".to_owned(),
        ))
    }
}

/// **Support deals with the control plane's dead letters over HTTP**, under
/// their own name. A requeue puts one back and a dismissal deletes one; neither
/// touches an effect that is not dead; and each is on the record by kind and
/// key, never by what the message said — a reset email's body is the link.
#[expect(
    clippy::too_many_lines,
    reason = "two dead letters and a live one, through every answer the routes give"
)]
#[tokio::test]
async fn support_requeues_and_dismisses_the_control_planes_dead_letters_under_their_own_name() {
    let fixture = Fixture::new().await;
    let (support, token, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    let forget = |email: &'static str| {
        let fixture = &fixture;
        async move {
            fixture.user(email, "hunter2hunter2").await;
            let (status, body, _) = fixture
                .send(
                    Request::post("/v1/password-resets")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::json!({ "email": email }).to_string(),
                        ))
                        .unwrap(),
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        }
    };
    forget("requeued@erp.test").await;
    forget("dismissed@erp.test").await;
    let run = erp_eventlog::Dispatcher::new(erp_eventlog::RetryPolicy::default())
        .register(Arc::new(RefusingRelay))
        .dispatch_once(fixture.control.pool(), 10)
        .await
        .expect("dispatches");
    assert_eq!(run.dead, 2);
    forget("pending@erp.test").await;

    let (status, listed) = fixture
        .as_caller(&token, "GET", "/v1/platform/effects/dead", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let mut dead: Vec<(i64, String)> = listed
        .as_array()
        .expect("a list")
        .iter()
        .map(|d| {
            assert_eq!(d["kind"], "email.send");
            (
                d["id"].as_i64().expect("an id"),
                d["idempotency_key"].as_str().expect("a key").to_owned(),
            )
        })
        .collect();
    dead.sort();
    let [(requeued, requeued_key), (dismissed, dismissed_key)] = &dead[..] else {
        panic!("two dead letters, not {listed}");
    };
    assert!(requeued_key.starts_with("reset:"), "{requeued_key}");
    let pending: i64 =
        sqlx::query_scalar("SELECT id FROM outbox WHERE dead_at IS NULL AND delivered_at IS NULL")
            .fetch_one(fixture.control.pool())
            .await
            .expect("the third is pending");

    let at = |method: &'static str, path: String| {
        let (fixture, token) = (&fixture, &token);
        async move { fixture.as_caller(token, method, &path, None).await }
    };
    let requeue = |id: i64| at("POST", format!("/v1/platform/effects/dead/{id}/requeue"));
    let dismiss = |id: i64| at("DELETE", format!("/v1/platform/effects/dead/{id}"));
    let not_dead = |(status, body): (StatusCode, serde_json::Value)| {
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("request.no_such_dead_letter")),
            "{body}"
        );
    };

    // Nothing that is not dead is touched, by either.
    not_dead(requeue(pending).await);
    not_dead(dismiss(pending).await);

    assert_eq!(requeue(*requeued).await.0, StatusCode::NO_CONTENT);
    not_dead(requeue(*requeued).await);
    not_dead(dismiss(*requeued).await);
    assert_eq!(dismiss(*dismissed).await.0, StatusCode::NO_CONTENT);
    not_dead(dismiss(*dismissed).await);
    let (status, body) = requeue(*dismissed).await;
    // The reason a second click reads must cover the click that was made.
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("dismissed")),
        "{body}"
    );
    not_dead((status, body));
    let (_, body, _) = fixture
        .send(
            Request::delete(format!("/v1/platform/effects/dead/{dismissed}"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(
        body["detail"].as_str().is_some_and(|d| d.contains("حُذفت")),
        "{body}"
    );

    let (status, body) = fixture
        .as_caller(&token, "POST", "/v1/platform/effects/dead/x/requeue", None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("request.invalid_id"))
    );

    let (_, listed) = fixture
        .as_caller(&token, "GET", "/v1/platform/effects/dead", None)
        .await;
    assert_eq!(listed, serde_json::json!([]));
    let left: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM outbox WHERE dead_at IS NULL AND delivered_at IS NULL ORDER BY id",
    )
    .fetch_all(fixture.control.pool())
    .await
    .expect("reads");
    assert_eq!(
        left,
        [*requeued, pending],
        "the requeued one waits to be sent, the dismissed one is gone"
    );

    let record: Vec<(String, Option<uuid::Uuid>, String, serde_json::Value)> = sqlx::query_as(
        "SELECT action, actor_identity_id, subject_id, detail FROM audit_entry
          WHERE subject_type = 'effect' ORDER BY id",
    )
    .fetch_all(fixture.control.pool())
    .await
    .expect("reads");
    assert_eq!(
        record,
        [
            (
                "effect.requeued".to_owned(),
                Some(support.into_uuid()),
                requeued.to_string(),
                serde_json::json!({ "kind": "email.send", "idempotency_key": requeued_key }),
            ),
            (
                "effect.dismissed".to_owned(),
                Some(support.into_uuid()),
                dismissed.to_string(),
                serde_json::json!({ "kind": "email.send", "idempotency_key": dismissed_key }),
            ),
        ]
    );
    // No tenant's business: the insert trigger leaves this build's `None` be.
    let filed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_entry WHERE subject_type = 'effect' AND tenant_id IS NOT NULL",
    )
    .fetch_one(fixture.control.pool())
    .await
    .expect("reads");
    assert_eq!(filed, 0, "a dead letter was filed under a tenant");

    fixture.cleanup().await;
}

/// A page of the audit trail, as whoever holds `token`, on `host`: the
/// entries, and the cursor to the next page if there is one.
async fn trail(
    fixture: &Fixture,
    token: &str,
    host: &'static str,
    path: &str,
) -> (Vec<serde_json::Value>, Option<String>) {
    let (status, body, _) = fixture
        .send(
            Request::get(path)
                .header(header::HOST, host)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    (
        body["items"].as_array().expect("a page").clone(),
        body["next"].as_str().map(str::to_owned),
    )
}

/// The first entry with this action, or a failure that shows what was there.
fn entry<'a>(entries: &'a [serde_json::Value], action: &str) -> &'a serde_json::Value {
    entries
        .iter()
        .find(|e| e["action"] == action)
        .unwrap_or_else(|| panic!("no {action} in {entries:#?}"))
}

/// **An owner reads their tenant's audit trail, and only theirs** — what they
/// and their colleagues changed, named by login; what support did there, by id
/// alone; and nothing of the tenant next door.
///
/// `api_key.revoked` names only the key, in its subject and its detail. Before
/// the tenant was a column, no query could have put it in this list.
#[expect(
    clippy::too_many_lines,
    reason = "two tenants, a key at each scope, and support, before one read"
)]
#[tokio::test]
async fn an_owner_reads_their_tenants_trail_and_nobody_elses() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let member = fixture.user("member@acme.test", "hunter2hunter2").await;
    fixture.join_as(member, acme, "viewer").await;
    let rival = fixture.user("owner@globex.test", "hunter2hunter2").await;
    fixture.join(rival, globex).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let rival_token = fixture.token("owner@globex.test", "hunter2hunter2").await;

    let (status, body) = fixture
        .as_caller(
            &token,
            "PATCH",
            &format!("/v1/members/{member}"),
            Some(serde_json::json!({ "role": "clerk" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, key) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/keys",
            Some(serde_json::json!({ "name": "Till", "scopes": ["*:read"], "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let key = key["id"].as_str().expect("an id").to_owned();
    let (status, body) = fixture
        .as_caller(
            &token,
            "DELETE",
            &format!("/v1/keys/{key}"),
            Some(serde_json::json!({ "why": "left on a till receipt" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // The same next door, where acme's owner has no business.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/keys")
                .header(header::HOST, "globex.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {rival_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "name": "Till", "scopes": ["*:read"], "role": "viewer" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // A key with the owner's role reads it only with the scope for it: the
    // route skips `Allowed`, and not its gate on a key.
    for (scope, answer) in [
        ("*:read", StatusCode::FORBIDDEN),
        ("*:manage_tenant", StatusCode::OK),
    ] {
        let (status, key) = fixture
            .as_caller(
                &token,
                "POST",
                "/v1/keys",
                Some(serde_json::json!({ "name": scope, "scopes": [scope], "role": "owner" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{key}");
        let secret = key["secret"].as_str().expect("a secret");
        let (status, body) = fixture.as_caller(secret, "GET", "/v1/audit", None).await;
        assert_eq!(status, answer, "{scope}: {body}");
        if status == StatusCode::FORBIDDEN {
            assert_eq!(body["code"], "keys.out_of_scope", "{body}");
        }
    }

    let (support, _, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    fixture
        .control
        .enter_for_support(support, acme, "ticket #42")
        .await
        .expect("support gets in");

    let (entries, next) = trail(&fixture, &token, "acme.localhost", "/v1/audit?limit=200").await;
    assert_eq!(next, None, "{entries:#?}");
    for e in &entries {
        assert_eq!(e["tenant"], acme.to_string(), "not acme's: {e}");
    }
    assert_eq!(
        entries[0]["action"], "tenant.support_access",
        "newest first: {entries:#?}"
    );
    for action in [
        "membership.role_changed",
        "api_key.issued",
        "api_key.revoked",
    ] {
        assert_eq!(entry(&entries, action)["actor_handle"], "owner@acme.test");
    }
    assert_eq!(entry(&entries, "api_key.revoked")["subject_id"], key);
    let access = entry(&entries, "tenant.support_access");
    assert_eq!(access["actor"], support.to_string());
    assert_eq!(access["detail"]["reason"], "ticket #42");
    assert_eq!(
        access["actor_handle"],
        serde_json::Value::Null,
        "a customer was shown a staff member's address"
    );

    fixture.cleanup().await;
}

/// **The owner of a suspended tenant reads why** (decision 12). Every other
/// route on the tenant's host answers the `503` everybody gets; this one is
/// the trail, which is the control plane's, and so needs no tenant to be
/// serving. A colleague who is not the owner is still refused.
#[tokio::test]
async fn the_owner_of_a_suspended_tenant_reads_why() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    fixture.join_as(clerk, tenant, "clerk").await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let clerk_token = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    let (billing, token, _) = fixture
        .staff("billing@erp.test", erp_control::PlatformRole::Billing)
        .await;

    let (status, body) = fixture
        .as_caller(
            &token,
            "POST",
            &format!("/v1/platform/tenants/{tenant}/suspend"),
            Some(serde_json::json!({ "reason": "The August invoice is unpaid." })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = fixture
        .as_caller(&owner_token, "GET", "/v1/tenant", None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    let (entries, _) = trail(&fixture, &owner_token, "acme.localhost", "/v1/audit").await;
    let suspended = &entries[0];
    assert_eq!(suspended["action"], "tenant.suspended", "{entries:#?}");
    assert_eq!(
        suspended["detail"]["reason"],
        "The August invoice is unpaid."
    );
    assert_eq!(suspended["actor"], billing.to_string());
    assert_eq!(suspended["actor_handle"], serde_json::Value::Null);

    let (status, body) = fixture
        .as_caller(&clerk_token, "GET", "/v1/audit", None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access.not_permitted")),
        "{body}"
    );
    assert_eq!(body["args"]["capability"]["value"], "manage_tenant");

    fixture.cleanup().await;
}

/// **A pod still on the build before `0019` files its entries under their
/// tenant.** During a deploy it inserts in the shape it knows, with no
/// `tenant_id`. The raw `INSERT` below is that build's `record()` word for word,
/// and the only SQL write here: it simulates the old build's write, which this
/// build cannot make. The insert trigger fills the tenant by the backfill's
/// rules — the subject, `detail`'s `tenant`, and for `api_key.revoked`, which
/// names only the key, the key's tenant. An entry about a person stays out.
#[tokio::test]
async fn a_pod_on_the_build_before_0019_files_its_entries_under_their_tenant() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let member = fixture.user("member@acme.test", "hunter2hunter2").await;
    fixture.join_as(member, acme, "viewer").await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, key) = fixture
        .as_caller(
            &token,
            "POST",
            "/v1/keys",
            Some(serde_json::json!({ "name": "Till", "scopes": ["*:read"], "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let key = key["id"].as_str().expect("an id").to_owned();

    for (action, subject_type, subject_id, detail) in [
        (
            "tenant.origin_revoked",
            "tenant",
            acme.to_string(),
            serde_json::json!({ "origin": "https://shop.acme.test" }),
        ),
        (
            "membership.role_changed",
            "identity",
            member.to_string(),
            serde_json::json!({ "tenant": acme.to_string(), "role": "clerk" }),
        ),
        (
            "api_key.revoked",
            "api_key",
            key.clone(),
            serde_json::json!({ "why": "left on a till receipt" }),
        ),
        (
            "identity.suspended",
            "identity",
            member.to_string(),
            serde_json::json!({ "reason": "policy violation" }),
        ),
    ] {
        sqlx::query(
            "INSERT INTO audit_entry
                (actor_identity_id, on_behalf_of_identity_id, action, subject_type, subject_id, detail)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Some(owner.into_uuid()))
        .bind(None::<uuid::Uuid>)
        .bind(action)
        .bind(subject_type)
        .bind(subject_id)
        .bind(detail)
        .execute(fixture.control.pool())
        .await
        .expect("the old build's insert");
    }

    let (entries, _) = trail(&fixture, &token, "acme.localhost", "/v1/audit?limit=200").await;
    for action in [
        "tenant.origin_revoked",
        "membership.role_changed",
        "api_key.revoked",
    ] {
        let found = entry(&entries, action);
        assert_eq!(found["tenant"], acme.to_string(), "{found}");
        assert_eq!(found["actor_handle"], "owner@acme.test", "{found}");
    }
    assert_eq!(entry(&entries, "api_key.revoked")["subject_id"], key);
    assert!(
        entries.iter().all(|e| e["action"] != "identity.suspended"),
        "an entry about a person was filed under a tenant: {entries:#?}"
    );

    fixture.cleanup().await;
}

/// **The trigger fills only what the writer left empty,** and this build's
/// `None` is one it leaves empty. A tenant `record()` is given is kept, even
/// where the subject and detail name another. A staff change about a person who
/// is a member of a tenant stays in no tenant's trail: an account's platform
/// role is not the company's business (§62).
#[tokio::test]
async fn the_database_keeps_the_tenant_its_writer_gave_and_the_none() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, acme, "clerk").await;
    fixture.enrolled_token(sara, "sara@acme.test").await;
    let (_, admin, _) = fixture
        .staff("admin@erp.test", erp_control::PlatformRole::Superadmin)
        .await;
    let (status, body) = fixture
        .as_caller(
            &admin,
            "POST",
            "/v1/platform/staff",
            Some(serde_json::json!({ "email": "sara@acme.test", "platform_role": "support" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = fixture
        .as_caller(
            &admin,
            "PATCH",
            &format!("/v1/platform/staff/{sara}"),
            Some(serde_json::json!({ "platform_role": "billing" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let mut conn = fixture.control.pool().acquire().await.expect("connection");
    fixture
        .control
        .record(
            &mut conn,
            Actor::system(),
            Some(acme),
            "test.filed",
            "tenant",
            &globex.to_string(),
            serde_json::json!({ "tenant": globex.to_string() }),
        )
        .await
        .expect("records");
    drop(conn);

    let filed: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT action, tenant_id FROM audit_entry
          WHERE action = 'test.filed' OR (subject_id = $1 AND detail ->> 'scope' = 'platform')
          ORDER BY id",
    )
    .bind(sara.to_string())
    .fetch_all(fixture.control.pool())
    .await
    .expect("reads");
    assert_eq!(
        filed,
        [
            ("membership.granted".to_owned(), None),
            ("membership.role_changed".to_owned(), None),
            ("test.filed".to_owned(), Some(acme.into_uuid())),
        ]
    );

    fixture.cleanup().await;
}

/// **A person reads what was done to them and what they did** (decision 6),
/// and nothing about the colleague beside them. Somebody else in an entry is
/// named only as a member of the tenant it concerns: the owner who changed
/// their role is, the superadmin who made them staff is not — and the
/// platform's own reader names that superadmin.
#[tokio::test]
async fn a_person_reads_what_was_done_to_them_and_by_them() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let sara = fixture.user("sara@acme.test", "hunter2hunter2").await;
    fixture.join_as(sara, tenant, "clerk").await;
    let omar = fixture.user("omar@acme.test", "hunter2hunter2").await;
    fixture.join_as(omar, tenant, "clerk").await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    for (who, role) in [(sara, "accountant"), (omar, "viewer")] {
        let (status, body) = fixture
            .as_caller(
                &owner_token,
                "PATCH",
                &format!("/v1/members/{who}"),
                Some(serde_json::json!({ "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let (admin, admin_token, _) = fixture
        .staff("admin@erp.test", erp_control::PlatformRole::Superadmin)
        .await;
    let (sara_token, _) = fixture.enrolled_token(sara, "sara@acme.test").await;
    let (status, body) = fixture
        .as_caller(
            &admin_token,
            "POST",
            "/v1/platform/staff",
            Some(serde_json::json!({ "email": "sara@acme.test", "platform_role": "support" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (mine, _) = trail(
        &fixture,
        &sara_token,
        "acme.localhost",
        "/v1/sessions/current/audit",
    )
    .await;
    for e in &mine {
        assert!(
            e["subject_id"] == sara.to_string() || e["actor"] == sara.to_string(),
            "not about sara, nor by her: {e}"
        );
    }
    let changed = entry(&mine, "membership.role_changed");
    assert_eq!(changed["detail"]["role"], "accountant");
    assert_eq!(changed["actor_handle"], "owner@acme.test");
    entry(&mine, "identity.created");
    let made_staff = mine
        .iter()
        .find(|e| e["action"] == "membership.granted" && e["detail"]["scope"] == "platform")
        .unwrap_or_else(|| panic!("not made staff: {mine:#?}"));
    assert_eq!(made_staff["actor"], admin.to_string());
    assert_eq!(
        made_staff["actor_handle"],
        serde_json::Value::Null,
        "a staff member's address was shown to a customer"
    );

    // The owner made both changes, so both are theirs.
    let (theirs, _) = trail(
        &fixture,
        &owner_token,
        "acme.localhost",
        "/v1/sessions/current/audit",
    )
    .await;
    let subjects: Vec<&str> = theirs
        .iter()
        .filter(|e| e["action"] == "membership.role_changed")
        .filter_map(|e| e["subject_id"].as_str())
        .collect();
    assert_eq!(subjects, [omar.to_string(), sara.to_string()]);

    // Staff read the same entry with the superadmin named.
    let (all, _) = trail(
        &fixture,
        &admin_token,
        "acme.localhost",
        &format!("/v1/platform/audit?identity={sara}&limit=200"),
    )
    .await;
    let made_staff = all
        .iter()
        .find(|e| e["action"] == "membership.granted" && e["detail"]["scope"] == "platform")
        .unwrap_or_else(|| panic!("not made staff: {all:#?}"));
    assert_eq!(made_staff["actor_handle"], "admin@erp.test");

    fixture.cleanup().await;
}

/// **An API key is not a person**, so it reads no personal trail, whatever its
/// scopes. Its own would be its issuing, which names the owner who issued it —
/// an address those scopes deny it everywhere else.
#[tokio::test]
async fn a_key_reads_no_personal_trail() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let owner_token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let (status, key) = fixture
        .as_caller(
            &owner_token,
            "POST",
            "/v1/keys",
            Some(serde_json::json!({ "name": "Widget", "scopes": ["*:read"], "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let secret = key["secret"].as_str().expect("a secret");
    let (status, body) = fixture
        .as_caller(secret, "GET", "/v1/sessions/current/audit", None)
        .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("keys.not_a_person")),
        "a key read a person's trail: {body}"
    );

    fixture.cleanup().await;
}

/// **Staff read the whole trail**, narrowed by tenant, by person, or both,
/// with every actor named — and without a filter, what concerns no tenant at
/// all is there too.
#[tokio::test]
async fn support_reads_the_whole_trail_narrowed_by_tenant_or_person() {
    let mut fixture = Fixture::new().await;
    let acme = fixture.provision("acme").await;
    let globex = fixture.provision("globex").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, acme).await;
    let (_, billing, _) = fixture
        .staff("billing@erp.test", erp_control::PlatformRole::Billing)
        .await;
    let (status, body) = fixture
        .as_caller(
            &billing,
            "POST",
            &format!("/v1/platform/tenants/{globex}/suspend"),
            Some(serde_json::json!({ "reason": "Unpaid." })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, token, _) = fixture
        .staff("support@erp.test", erp_control::PlatformRole::Support)
        .await;
    let read = |query: String| {
        let (fixture, token) = (&fixture, &token);
        async move {
            trail(
                fixture,
                token,
                "acme.localhost",
                &format!("/v1/platform/audit?limit=200{query}"),
            )
            .await
            .0
        }
    };

    let everything = read(String::new()).await;
    let tenants: std::collections::BTreeSet<String> = everything
        .iter()
        .map(|e| e["tenant"].as_str().unwrap_or("none").to_owned())
        .collect();
    assert_eq!(
        tenants,
        [acme.to_string(), globex.to_string(), "none".to_owned()].into(),
        "{everything:#?}"
    );

    let globex_only = read(format!("&tenant={globex}")).await;
    for e in &globex_only {
        assert_eq!(e["tenant"], globex.to_string(), "{e}");
    }
    let suspended = entry(&globex_only, "tenant.suspended");
    assert_eq!(suspended["actor_handle"], "billing@erp.test");

    let owners = read(format!("&tenant={acme}&identity={owner}")).await;
    assert_eq!(
        owners
            .iter()
            .map(|e| (e["action"].as_str(), e["subject_id"].as_str()))
            .collect::<Vec<_>>(),
        [(Some("membership.granted"), Some(owner.to_string().as_str()))],
        "{owners:#?}"
    );

    let (status, body) = fixture
        .as_caller(&token, "GET", "/v1/platform/audit?tenant=acme", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    fixture.cleanup().await;
}

/// **The trail pages without losing or repeating an entry**, and a cursor it
/// did not hand out is a `400` — never the first page again, which would look
/// like the trail starting over.
#[tokio::test]
async fn the_audit_trail_pages_without_losing_or_repeating_entries() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.provision("acme").await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    fixture.join(owner, tenant).await;
    let member = fixture.user("member@acme.test", "hunter2hunter2").await;
    fixture.join_as(member, tenant, "viewer").await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    for role in ["clerk", "viewer", "clerk", "viewer"] {
        let (status, body) = fixture
            .as_caller(
                &token,
                "PATCH",
                &format!("/v1/members/{member}"),
                Some(serde_json::json!({ "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    let (whole, next) = trail(&fixture, &token, "acme.localhost", "/v1/audit?limit=200").await;
    assert_eq!(next, None);
    assert!(whole.len() > 5, "too few entries to page: {whole:#?}");

    let mut walked = Vec::new();
    let mut path = "/v1/audit?limit=2".to_owned();
    for _ in 0..whole.len() {
        let (page, next) = trail(&fixture, &token, "acme.localhost", &path).await;
        walked.extend(page);
        let Some(next) = next else { break };
        path = format!("/v1/audit?limit=2&after={next}");
    }
    assert_eq!(walked, whole, "paging lost, repeated or reordered an entry");

    for (what, cursor) in [
        ("not a number", erp_types::Cursor::over(&["x"]).to_string()),
        (
            "another list's",
            erp_types::Cursor::over(&["1", "2"]).to_string(),
        ),
        ("not a cursor", "zz".to_owned()),
    ] {
        let (status, body) = fixture
            .as_caller(&token, "GET", &format!("/v1/audit?after={cursor}"), None)
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("request.invalid_cursor")),
            "{what}: {body}"
        );
    }

    fixture.cleanup().await;
}

/// **Staff are managed over HTTP**, a change reaches the door at once, and every
/// grant, change and revocation is on the record under the superadmin who
/// made it.
#[expect(
    clippy::too_many_lines,
    reason = "one staff member's life, from grant to revocation, and the record it leaves"
)]
#[tokio::test]
async fn staff_are_managed_over_http_and_every_change_names_who_made_it() {
    let fixture = Fixture::new().await;
    let (admin, admin_token, _) = fixture
        .staff("admin@erp.test", erp_control::PlatformRole::Superadmin)
        .await;
    let noura = fixture.user("noura@erp.test", "hunter2hunter2").await;
    let (noura_token, _) = fixture.enrolled_token(noura, "noura@erp.test").await;
    fixture.user("nofactor@erp.test", "hunter2hunter2").await;

    let grant = |email: &str, role: &str| {
        fixture.as_caller(
            &admin_token,
            "POST",
            "/v1/platform/staff",
            Some(serde_json::json!({ "email": email, "platform_role": role })),
        )
    };
    for (email, role, status, code) in [
        (
            "nofactor@erp.test",
            "support",
            StatusCode::UNPROCESSABLE_ENTITY,
            "staff.no_second_factor",
        ),
        (
            "nobody@erp.test",
            "support",
            StatusCode::UNPROCESSABLE_ENTITY,
            "staff.no_such_account",
        ),
        (
            "noura@erp.test",
            "owner",
            StatusCode::BAD_REQUEST,
            "request.unknown_staff_role",
        ),
    ] {
        let (answered, body) = grant(email, role).await;
        assert_eq!(
            (answered, body["code"].as_str()),
            (status, Some(code)),
            "{body}"
        );
    }

    let (status, body) = grant("Noura@erp.test ", "support").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["identity"], noura.to_string());
    let (status, body) = grant("noura@erp.test", "support").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("staff.already_staff"))
    );

    // Support may not manage staff — and asking puts her role in the cache.
    let list = || fixture.as_caller(&noura_token, "GET", "/v1/platform/staff", None);
    assert_eq!(list().await.0, StatusCode::FORBIDDEN);

    // **At once, not after the cache's five seconds.**
    let staff_path = format!("/v1/platform/staff/{noura}");
    let change = |role: &str| {
        fixture.as_caller(
            &admin_token,
            "PATCH",
            &staff_path,
            Some(serde_json::json!({ "platform_role": role })),
        )
    };
    assert_eq!(change("superadmin").await.0, StatusCode::NO_CONTENT);
    let (status, listed) = list().await;
    assert_eq!(status, StatusCode::OK, "a promotion waited for the cache");
    let noura_row = listed
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["identity"] == noura.to_string())
        .expect("she is listed");
    assert_eq!(noura_row["platform_role"], "superadmin");
    assert_eq!(noura_row["second_factor"], true);

    let (status, _) = fixture
        .as_caller(&admin_token, "DELETE", &staff_path, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        list().await.0,
        StatusCode::FORBIDDEN,
        "a revocation waited for the cache"
    );
    for (status, body) in [
        change("support").await,
        fixture
            .as_caller(&admin_token, "DELETE", &staff_path, None)
            .await,
    ] {
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("staff.not_staff"))
        );
    }

    // On the record, under the superadmin's name.
    let rows: Vec<(String, Option<uuid::Uuid>, serde_json::Value)> = sqlx::query_as(
        "SELECT action, actor_identity_id, detail FROM audit_entry
          WHERE subject_type = 'identity' AND subject_id = $1
            AND action LIKE 'membership.%'
          ORDER BY id",
    )
    .bind(noura.to_string())
    .fetch_all(fixture.control.pool())
    .await
    .expect("reads the trail");
    let admin = Some(*admin.as_uuid());
    assert_eq!(
        rows,
        vec![
            (
                "membership.granted".to_owned(),
                admin,
                serde_json::json!({ "scope": "platform", "tenant": null, "role": "support" })
            ),
            (
                "membership.role_changed".to_owned(),
                admin,
                serde_json::json!({ "scope": "platform", "role": "superadmin" })
            ),
            (
                "membership.revoked".to_owned(),
                admin,
                serde_json::json!({ "scope": "platform", "tenant": null })
            ),
        ]
    );

    fixture.cleanup().await;
}

/// **The last live superadmin cannot leave over HTTP** — neither removed nor
/// demoted, by themselves or anybody — and a suspended superadmin does not count
/// as one. `operator revoke-staff` can, which is what it is for.
#[tokio::test]
async fn the_last_superadmin_cannot_leave_over_http_and_the_operator_can() {
    use erp_control::PlatformRole::Superadmin;

    let fixture = Fixture::new().await;
    let (a, a_token, _) = fixture.staff("a@erp.test", Superadmin).await;
    let refused = |token: String, method: &'static str, who: IdentityId| {
        let fixture = &fixture;
        async move {
            let (status, body) = fixture
                .as_caller(
                    &token,
                    method,
                    &format!("/v1/platform/staff/{who}"),
                    (method == "PATCH").then(|| serde_json::json!({ "platform_role": "support" })),
                )
                .await;
            assert_eq!(
                (status, body["code"].as_str()),
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Some("staff.last_superadmin")
                ),
                "{method} {who}"
            );
        }
    };
    refused(a_token.clone(), "PATCH", a).await;
    refused(a_token.clone(), "DELETE", a).await;

    // Another superadmin, suspended: not live, so `a` is still the last.
    let (b, _, _) = fixture.staff("b@erp.test", Superadmin).await;
    fixture
        .control
        .suspend_identity(b, "left", Actor::system())
        .await
        .expect("suspends");
    refused(a_token.clone(), "DELETE", a).await;

    // A live one, and `a` may go.
    let (c, c_token, _) = fixture.staff("c@erp.test", Superadmin).await;
    let (status, body) = fixture
        .as_caller(&a_token, "DELETE", &format!("/v1/platform/staff/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    refused(c_token.clone(), "PATCH", c).await;

    // Break glass: `operator revoke-staff`, which is `revoke_membership` with
    // the platform scope and no guard. The door closes on the next request.
    assert!(
        fixture
            .control
            .revoke_membership(c, Scope::Platform, Actor::system())
            .await
            .expect("revokes")
    );
    let (status, _) = fixture
        .as_caller(&c_token, "GET", "/v1/platform/staff", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    fixture.cleanup().await;
}

/// Two superadmins removing each other at the same moment leave one, not
/// none: each sees the other still standing unless the guard locks first.
///
/// Below the door, on purpose. Over HTTP one request usually finishes before
/// the other passes its door, and a test that races only sometimes proves
/// nothing; here both transactions are open at once every time.
#[tokio::test]
async fn two_superadmins_removing_each_other_at_once_leave_one() {
    use erp_control::PlatformRole::Superadmin;

    let fixture = Fixture::new().await;
    let (a, _, _) = fixture.staff("a@erp.test", Superadmin).await;
    let (b, _, _) = fixture.staff("b@erp.test", Superadmin).await;

    fixture.warm().await;

    let (one, other) = tokio::join!(
        fixture.control.revoke_staff(b, Actor::identity(a)),
        fixture.control.revoke_staff(a, Actor::identity(b)),
    );
    assert!(
        matches!(
            (&one, &other),
            (Ok(()), Err(erp_control::StaffError::LastSuperadmin))
                | (Err(erp_control::StaffError::LastSuperadmin), Ok(()))
        ),
        "{one:?} / {other:?}"
    );
    let left = fixture.control.staff().await.expect("lists");
    assert_eq!(left.len(), 1, "{left:?}");

    fixture.cleanup().await;
}

/// Two superadmins granting one person two different roles at once: one is
/// told it worked, the other that it was too late — never both told "created"
/// while one of them set nothing.
#[tokio::test]
async fn two_grants_of_one_person_at_once_tell_the_loser() {
    use erp_control::PlatformRole::{Billing, Support};

    let fixture = Fixture::new().await;
    let noura = fixture.user("noura@erp.test", "hunter2hunter2").await;
    fixture.enrolled_token(noura, "noura@erp.test").await;

    fixture.warm().await;

    let (support, billing) = tokio::join!(
        fixture
            .control
            .grant_staff("noura@erp.test", Support, Actor::system()),
        fixture
            .control
            .grant_staff("noura@erp.test", Billing, Actor::system()),
    );
    let won = match (&support, &billing) {
        (Ok(_), Err(erp_control::StaffError::AlreadyStaff(_))) => Support,
        (Err(erp_control::StaffError::AlreadyStaff(_)), Ok(_)) => Billing,
        _ => panic!("both told the same thing: {support:?} / {billing:?}"),
    };
    let staff = fixture.control.staff().await.expect("lists");
    assert_eq!(staff.len(), 1);
    assert_eq!(
        staff[0].role, won,
        "the one told it worked is the one that did"
    );

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
            sales::Authority::System,
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

/// **A price band filled into a form reads back as the form and as the band.**
///
/// The round trip the four authoring levels exist for: a business writes
/// "Thursday, from 17:00, 25% dearer" and never meets a bitmask of weekdays, a
/// minute count past midnight or a basis point. What comes back carries both —
/// the answers so the form can be reopened, and the band so a calendar can be
/// drawn without holding the templates.
#[tokio::test]
async fn a_price_band_can_be_filled_into_a_form_and_reads_back_as_both() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // The templates a settings screen would draw the form from.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/tariff/templates")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::ACCEPT_LANGUAGE, "ar")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let templates = body.as_array().expect("a list");
    let evening = templates
        .iter()
        .find(|t| t["id"] == "weekday_evening")
        .expect("the shipped evening template");
    assert!(
        evening["name"]
            .as_str()
            .is_some_and(|n| n.chars().any(|c| ('\u{600}'..='\u{6ff}').contains(&c))),
        "a template names itself in the caller's language: {evening}"
    );
    assert_eq!(
        evening["fields"]
            .as_array()
            .expect("blanks")
            .iter()
            .map(|f| f["key"].as_str().expect("a key"))
            .collect::<Vec<_>>(),
        vec!["name", "weekday", "from_hour", "percent"]
    );

    let put = |band: serde_json::Value| {
        Request::put("/v1/booking/tariff")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "bands": [band] }).to_string(),
            ))
            .unwrap()
    };
    let filled = serde_json::json!({
        "level": "form",
        "template": "weekday_evening",
        "answers": { "name": "ذروة الخميس", "weekday": 4, "from_hour": 17, "percent": 25 },
    });

    let (status, body, _) = fixture.send(put(filled)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let band = &body["bands"][0];
    // The form, so the screen can reopen it exactly as it was saved.
    assert_eq!(band["level"], "form");
    assert_eq!(band["template"], "weekday_evening");
    assert_eq!(band["answers"]["percent"], 25);
    assert_eq!(band["answers"]["name"], "ذروة الخميس");
    // And the band, derived from those answers rather than stored beside them.
    assert_eq!(band["name"], "ذروة الخميس");
    assert_eq!(band["uplift"], 2_500, "25 percent in basis points");
    assert_eq!(band["hours"]["weekdays"], serde_json::json!([4]));
    assert_eq!(band["hours"]["opens_at"], 17 * 60);
    assert_eq!(band["hours"]["closes_at"], 24 * 60);

    // **Hand-editing drops the form.** A band sent back as `raw` is no longer
    // that template's band, and says so rather than showing a form whose
    // answers no longer describe it.
    let (status, body, _) = fixture
        .send(put(serde_json::json!({
            "level": "raw",
            "name": "ذروة الخميس",
            "uplift": 4_000,
            "hours": { "weekdays": [4], "opens_at": 1020, "closes_at": 1440 },
        })))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let band = &body["bands"][0];
    assert_eq!(band["level"], "raw");
    assert!(band.get("template").is_none(), "the form is gone: {band}");
    assert!(
        band.get("answers").is_none(),
        "and so are its answers: {band}"
    );
    assert_eq!(band["uplift"], 4_000);

    fixture.cleanup().await;
}

/// **A ready-made tariff: browse it, see exactly what it would do, take it.**
///
/// The blueprint story for rules — the third kind this build ships, after
/// charts of accounts and trades. A pack writes ordinary form-authored bands,
/// so what it installs is editable on the same screen a business types bands
/// into, and installing twice is not an error.
#[tokio::test]
async fn a_ready_made_tariff_can_be_previewed_and_then_installed() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let get = |path: &'static str| {
        Request::get(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT_LANGUAGE, "ar")
            .body(Body::empty())
            .unwrap()
    };
    let post = |path: &'static str, pack: &str| {
        Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT_LANGUAGE, "ar")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::json!({ "pack": pack }).to_string()))
            .unwrap()
    };

    // Browse. A pack shows every band it would write, and the answers it would
    // fill in — which is what makes it readable before it is taken.
    let (status, body, _) = fixture.send(get("/v1/booking/tariff/packs")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let shipped = body.as_array().expect("a list");
    let barbershop = shipped
        .iter()
        .find(|p| p["id"] == "barbershop")
        .expect("the barbershop pack");
    assert!(
        barbershop["name"]
            .as_str()
            .is_some_and(|n| n.chars().any(|c| ('\u{600}'..='\u{6ff}').contains(&c))),
        "a pack names itself in the caller's language: {barbershop}"
    );
    assert_eq!(barbershop["bands"][0]["template"], "weekday_evening");
    assert_eq!(barbershop["bands"][0]["answers"]["percent"], 15);

    // Preview. Says what it would do, and writes nothing.
    let (status, would, _) = fixture
        .send(post("/v1/booking/tariff/packs/preview", "barbershop"))
        .await;
    assert_eq!(status, StatusCode::OK, "{would}");
    assert_eq!(would["added"].as_array().map(Vec::len), Some(2));
    assert_eq!(would["skipped"], serde_json::json!([]));
    assert_eq!(would["bands"].as_array().map(Vec::len), Some(2));

    let (status, body, _) = fixture.send(get("/v1/booking/tariff")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bands"], serde_json::json!([]), "the preview wrote");

    // Install. The same answer, from the same run.
    let (status, did, _) = fixture
        .send(post("/v1/booking/tariff/packs", "barbershop"))
        .await;
    assert_eq!(status, StatusCode::OK, "{did}");
    assert_eq!(
        did["added"], would["added"],
        "install and preview disagreed"
    );
    assert_eq!(did["bands"], would["bands"]);

    // And what it wrote is an ordinary form-authored band, editable on the
    // tariff screen like one somebody typed in.
    let (status, body, _) = fixture.send(get("/v1/booking/tariff")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let band = &body["bands"][0];
    assert_eq!(band["level"], "form");
    assert_eq!(band["template"], "weekday_evening");
    assert_eq!(band["answers"]["percent"], 15);
    assert_eq!(band["uplift"], 1_500);

    // Twice is not an error, and changes nothing.
    let (status, again, _) = fixture
        .send(post("/v1/booking/tariff/packs", "barbershop"))
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["added"], serde_json::json!([]));
    assert_eq!(again["skipped"], did["added"]);
    assert_eq!(again["bands"], did["bands"]);

    let (status, body, _) = fixture
        .send(post("/v1/booking/tariff/packs", "seasonal"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.no_such_pack");

    fixture.cleanup().await;
}

/// **A pack goes underneath what the business already wrote, and refuses if
/// the tariff moved while it was being read.**
///
/// First match wins, so appending is the only position that cannot reprice an
/// hour somebody already decided. And installing is a read-modify-write, so an
/// unconditional write would lose whatever landed in between — which is a
/// second admin's whole tariff, with both requests answering success.
#[tokio::test]
async fn a_pack_goes_under_what_the_business_wrote_and_refuses_a_tariff_that_moved() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Their own Thursday evening, at a discount they chose.
    let (status, body, _) = fixture
        .send(
            Request::put("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "bands": [{
                        "level": "raw",
                        "name": "عرض الخميس",
                        "uplift": -2_000,
                        // 17:00, deliberately not the 19:00 the pack's own
                        // Thursday band opens at: an identical window would be
                        // *skipped*, which is a different property and has its
                        // own test.
                        "hours": { "weekdays": [4], "opens_at": 1020, "closes_at": 1440 },
                    }] })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let response = fixture
        .raw(
            Request::get("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let stale = response.headers()[header::ETAG]
        .to_str()
        .expect("ascii")
        .to_owned();

    let install = |if_match: Option<&str>| {
        let mut request = Request::post("/v1/booking/tariff/packs")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(version) = if_match {
            request = request.header(header::IF_MATCH, version);
        }
        request
            .body(Body::from(
                serde_json::json!({ "pack": "restaurant" }).to_string(),
            ))
            .unwrap()
    };

    let (status, did, _) = fixture.send(install(None)).await;
    assert_eq!(status, StatusCode::OK, "{did}");
    assert_eq!(
        did["bands"][0]["name"], "عرض الخميس",
        "the pack did not go underneath: {did}"
    );
    assert_eq!(did["bands"][0]["uplift"], -2_000);
    assert_eq!(did["bands"].as_array().map(Vec::len), Some(3));

    // That install moved the tariff, so the version read before it is stale.
    let (status, body, _) = fixture.send(install(Some(&stale))).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");

    fixture.cleanup().await;
}

/// **A band whose template this build stopped shipping is a `500`, not a
/// quietly shorter tariff.**
///
/// Two wrong answers avoided at once. It is not a `400`: the caller sent
/// nothing, and a template withdrawn by a deploy is our doing. And it is not a
/// `200` with the band missing: a tariff silently short of its peak rate is a
/// month of underbilling nobody notices, which is what L6 refuses on behalf of.
#[tokio::test]
async fn a_stored_band_whose_template_is_gone_is_refused_rather_than_dropped() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // Written straight into the setting, which is what a deploy that withdrew
    // `seasonal` would leave behind.
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        booking::TariffAsWritten::KEY,
        &serde_json::json!({ "bands": [{ "level": "preset", "template": "seasonal" }] }),
        None,
        None,
    )
    .await
    .expect("stored");
    drop(conn);

    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");

    fixture.cleanup().await;
}

/// **Answers that describe no band are refused while their author is still
/// looking at them**, rather than at the next booking.
#[tokio::test]
async fn a_price_band_form_refuses_answers_that_describe_no_band() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let put = |band: serde_json::Value| {
        Request::put("/v1/booking/tariff")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "bands": [band] }).to_string(),
            ))
            .unwrap()
    };
    let filled = serde_json::json!({
        "level": "form",
        "template": "weekday_evening",
        "answers": { "name": "ذروة الخميس", "weekday": 4, "from_hour": 17, "percent": 25 },
    });

    // Answers that describe no band are refused while the person who wrote them
    // is still looking at the screen.
    let mut absurd = filled.clone();
    absurd["answers"]["weekday"] = serde_json::json!(9);
    let (status, body, _) = fixture.send(put(absurd)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.not_a_weekday");

    // A blank left blank names itself and the template that asked.
    let mut short = filled.clone();
    short["answers"]
        .as_object_mut()
        .expect("answers")
        .remove("percent");
    let (status, body, _) = fixture.send(put(short)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.unanswered");

    // An answer of the wrong shape is not silently coerced.
    let mut mistyped = filled.clone();
    mistyped["answers"]["percent"] = serde_json::json!("twenty five");
    let (status, body, _) = fixture.send(put(mistyped)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.not_an_answer");

    // A template this build does not ship.
    let (status, body, _) = fixture
        .send(put(
            serde_json::json!({ "level": "preset", "template": "seasonal" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.no_such_template");

    // An answer nobody asked for is a mistake, not spare data. It is how a
    // renamed field goes unnoticed: the old answer sits there unread and the
    // new one is missing, and only one of those is otherwise reported.
    let mut spare = filled.clone();
    spare["answers"]["percentage"] = serde_json::json!(25);
    let (status, body, _) = fixture.send(put(spare)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.not_an_answer");

    // **A tariff is set whole.** One bad band among good ones refuses the lot
    // rather than storing the ones that happened to parse — a tenant who fixes
    // the typo and resends must not find their first band written twice.
    let mut bad = filled.clone();
    bad["answers"]["from_hour"] = serde_json::json!(24);
    let (status, body, _) = fixture
        .send(
            Request::put("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "bands": [filled, bad] }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "booking.not_an_hour");

    // So none of that wrote anything.
    let (status, body, _) = fixture
        .send(
            Request::get("/v1/booking/tariff")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bands"], serde_json::json!([]));

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

/// A permission-limit rule as `PUT /v1/tenant/permission-limits` takes it:
/// refused when every one of `conditions` holds.
fn refuse_when(name: &str, conditions: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({ "name": name, "when": { "when": "all", "of": conditions }, "then": "refuse" })
}

/// `fact == value`, for a text fact.
fn fact_is(fact: &str, value: &str) -> serde_json::Value {
    serde_json::json!({ "when": "is", "fact": fact, "op": "eq", "value": { "type": "text", "of": value } })
}

/// Sets this tenant's permission limits as whoever holds `token`.
async fn set_limits(
    fixture: &Fixture,
    token: &str,
    rules: serde_json::Value,
    if_match: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::put("/v1/tenant/permission-limits")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(version) = if_match {
        request = request.header(header::IF_MATCH, version);
    }
    let (status, body, _) = fixture
        .send(
            request
                .body(Body::from(
                    serde_json::json!({ "rules": rules }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    (status, body)
}

/// Reads them: status, `ETag`, body.
async fn limits(fixture: &Fixture, token: &str) -> (StatusCode, String, serde_json::Value) {
    let response = fixture
        .raw(
            Request::get("/v1/tenant/permission-limits")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let status = response.status();
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .unwrap_or(serde_json::Value::Null);
    (status, etag, body)
}

/// **Permission limits are a versioned setting, and a rule that could never be
/// true is refused when it is written** — by name, with a code a screen can
/// branch on — rather than stored to never fire.
#[tokio::test]
async fn permission_limits_are_a_versioned_setting_that_refuses_impossible_rules() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, etag, body) = limits(&fixture, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rules"], serde_json::json!([]), "roles decide alone");
    assert_eq!(etag, "\"0\"", "nothing set, and known to be");

    for (rule, code) in [
        (
            fact_is("phase_of_the_moon", "waxing"),
            "request.no_such_fact",
        ),
        (
            fact_is("capability", "post_entires"),
            "request.no_such_fact_value",
        ),
        (
            fact_is("capability", "manage_tenant"),
            "request.no_such_fact_value",
        ),
        (fact_is("amount", "lots"), "request.rule_cannot_compare"),
    ] {
        let (status, body) = set_limits(
            &fixture,
            &token,
            serde_json::json!([refuse_when("Never true", std::slice::from_ref(&rule))]),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}: {body}");
        assert_eq!(body["code"], code, "{rule}: {body}");
        assert_eq!(body["args"]["rule"]["value"], "Never true", "named: {body}");
    }
    let (_, etag, _) = limits(&fixture, &token).await;
    assert_eq!(etag, "\"0\"", "nothing refused was stored");

    let rules = serde_json::json!([refuse_when(
        "Clerks do not post",
        &[
            fact_is("role", "clerk"),
            fact_is("capability", "post_entries")
        ]
    )]);
    let (status, body) = set_limits(&fixture, &token, rules.clone(), Some("\"0\"")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = set_limits(&fixture, &token, rules.clone(), Some("\"0\"")).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "a stale If-Match: {body}"
    );

    let (status, etag, body) = limits(&fixture, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(etag, "\"0\"");
    assert_eq!(body["rules"], rules, "reads back as written");

    fixture.cleanup().await;
}

/// A request to post `body`, keyed by who sends what.
fn posting(token: &str, path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", idem(&format!("{token}{body}")))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// `minor` debited to `debit` and credited to `credit`.
fn an_entry(debit: &str, credit: &str, minor: i64, currency: &str) -> serde_json::Value {
    serde_json::json!({
        "occurred_on": "2026-01-15T00:00:00Z",
        "memo": format!("{minor} {currency}"),
        "lines": [
            { "account": debit, "amount": { "minor": minor, "currency": currency } },
            { "account": credit, "amount": { "minor": -minor, "currency": currency } }
        ]
    })
}

fn riyal_entry(riyals: i64) -> serde_json::Value {
    an_entry("1000", "4000", riyals * 100, "SAR")
}

/// An owner, a bookkeeper (the `accountant` role), riyal accounts `1000` and
/// `4000`, and the limit `roles.rs` names, written by the owner through the
/// product. Answers the fixture and both tokens.
async fn a_bookkeeper_limited_to_ten_thousand() -> (Fixture, String, String) {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let bookkeeper = fixture.user("books@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.join_as(bookkeeper, tenant, "accountant").await;
    fixture.enable_ledger(tenant).await;
    let owner = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let bookkeeper = fixture.token("books@acme.test", "hunter2hunter2").await;

    for (code, kind) in [("1000", "asset"), ("4000", "revenue")] {
        let (status, body, _) = fixture
            .send(posting(
                &owner,
                "/v1/ledger/accounts",
                &serde_json::json!({ "code": code, "name": code, "kind": kind, "currency": "SAR" }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = set_limits(
        &fixture,
        &owner,
        serde_json::json!([refuse_when(
            "A bookkeeper posts under ten thousand",
            &[
                fact_is("role", "accountant"),
                fact_is("capability", "post_entries"),
                serde_json::json!({ "when": "is", "fact": "amount", "op": "gte",
                  "value": { "type": "money", "of": { "minor": 1_000_000, "currency": "SAR" } } })
            ]
        )]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    (fixture, owner, bookkeeper)
}

/// **"A bookkeeper may post entries under ten thousand riyals"**, written by
/// the owner through the product and enforced on the ledger — the example
/// `erp_tenant::roles` named in Phase 1. The bookkeeper is the `accountant`
/// role. The owner's entry of the same size is the contrast: without the role
/// fact the rule refuses everybody.
#[tokio::test]
async fn a_bookkeeper_is_refused_an_entry_over_the_limit_the_owner_wrote() {
    let (fixture, owner, bookkeeper) = a_bookkeeper_limited_to_ten_thousand().await;
    let entries = "/v1/ledger/entries";

    let (status, body, _) = fixture
        .send(posting(&bookkeeper, entries, &riyal_entry(20_000)))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "over the limit: {body}");
    assert_eq!(body["code"], "access.not_permitted");
    assert_eq!(
        body["args"]["capability"]["value"], "post_entries",
        "{body}"
    );

    let (status, body, _) = fixture
        .send(posting(&bookkeeper, entries, &riyal_entry(5_000)))
        .await;
    assert_eq!(status, StatusCode::OK, "under it: {body}");

    let (status, body, _) = fixture
        .send(posting(&owner, entries, &riyal_entry(20_000)))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the rule is about the bookkeeper: {body}"
    );

    fixture.cleanup().await;
}

/// **Nor can the bookkeeper walk around it**, the two ways review found.
///
/// A reversal is an entry the size of the one it undoes, so the owner's 20,000
/// cannot be posted backwards; the bookkeeper undoing their own 4,000 is the
/// contrast. And the bookkeeper may open accounts, but 5,000,000 dollars is
/// not *under* 10,000 riyals: a limit that cannot judge an amount refuses it.
#[tokio::test]
async fn a_bookkeeper_cannot_walk_around_the_limit_by_reversal_or_currency() {
    let (fixture, owner, bookkeeper) = a_bookkeeper_limited_to_ten_thousand().await;
    let entries = "/v1/ledger/entries";
    let reverse = |token: &str, id: &serde_json::Value| {
        let id = id.as_str().expect("an id");
        posting(
            token,
            &format!("/v1/ledger/entries/{id}/reversal"),
            &serde_json::json!({ "occurred_on": "2026-01-16T00:00:00Z", "memo": id }),
        )
    };

    let (status, owners, _) = fixture
        .send(posting(&owner, entries, &riyal_entry(20_000)))
        .await;
    assert_eq!(status, StatusCode::OK, "{owners}");
    let (status, body, _) = fixture.send(reverse(&bookkeeper, &owners["id"])).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "20,000 backwards: {body}");
    let (status, theirs, _) = fixture
        .send(posting(&bookkeeper, entries, &riyal_entry(4_000)))
        .await;
    assert_eq!(status, StatusCode::OK, "{theirs}");
    let (status, body, _) = fixture.send(reverse(&bookkeeper, &theirs["id"])).await;
    assert_eq!(status, StatusCode::OK, "their own 4,000: {body}");

    for (code, kind) in [("1100", "asset"), ("4100", "revenue")] {
        let (status, body, _) = fixture
            .send(posting(
                &bookkeeper,
                "/v1/ledger/accounts",
                &serde_json::json!({ "code": code, "name": code, "kind": kind, "currency": "USD" }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body, _) = fixture
        .send(posting(
            &bookkeeper,
            entries,
            &an_entry("1100", "4100", 500_000_000, "USD"),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "in another currency: {body}");

    fixture.cleanup().await;
}

/// **No limit locks the owner out of its limits.** A rule that refuses
/// everything refuses the owner's reads — the check that it really bites, so
/// the rest cannot pass vacuously — and still leaves them able to read and
/// remove it, because `ManageTenant` is never narrowed.
#[tokio::test]
async fn no_limit_locks_the_owner_out_of_its_limits() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let calendar = || {
        Request::get("/v1/tenant/calendar")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, body) = set_limits(
        &fixture,
        &token,
        serde_json::json!([{ "name": "Everything", "when": { "when": "always" }, "then": "refuse" }]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body, _) = fixture.send(calendar()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the limit bites: {body}");

    let (status, etag, body) = limits(&fixture, &token).await;
    assert_eq!(status, StatusCode::OK, "the owner still reads it: {body}");
    let (status, body) = set_limits(&fixture, &token, serde_json::json!([]), Some(&etag)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "and removes it: {body}");

    let (status, body, _) = fixture.send(calendar()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    fixture.cleanup().await;
}

/// **Nor do limits this build can no longer read.** Every check they would
/// narrow is refused — a 503, not the unlimited answer (L6) — and the owner
/// can still replace them.
///
/// The row is written with the configuration store's own `set`, bypassing
/// `Limits::new`: it **simulates** limits a build with a larger registry saved,
/// which is what removing a fact from `limits::registry()` leaves behind. This
/// build cannot produce it, which is the point.
#[tokio::test]
async fn limits_this_build_cannot_read_lock_nobody_out_of_repairing_them() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let calendar = || {
        Request::get("/v1/tenant/calendar")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance access");
    let mut conn = db.acquire().await.expect("a connection");
    erp_eventlog::configuration::set(
        &mut conn,
        "tenant.permission_limits",
        &serde_json::json!([refuse_when(
            "Saved by an older build",
            &[fact_is("phase_of_the_moon", "waxing")]
        )]),
        Some("an-older-build"),
        None,
    )
    .await
    .expect("stored");
    drop(conn);

    let (status, body, _) = fixture.send(calendar()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "refused, not unlimited: {body}"
    );
    let (status, _, body) = limits(&fixture, &token).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the owner is told the stored rules are unusable: {body}"
    );

    let (status, body) = set_limits(&fixture, &token, serde_json::json!([]), None).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "and can replace them: {body}"
    );
    let (status, body, _) = fixture.send(calendar()).await;
    assert_eq!(status, StatusCode::OK, "{body}");

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
            to: Vec::new(),
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

// ---------------------------------------------------------------------------
// Read-model versions
// ---------------------------------------------------------------------------

/// Stamps one of `tenant`'s projection groups at a read-model version that is
/// not this build's.
///
/// **Raw SQL, and it has to be.** This build stamps only its own version, so
/// nothing in it can make a tenant's tables another version than itself. `0`
/// is what the tenant chain's `0016` leaves on every tenant built before it,
/// and what a restore of an older backup brings back; a version *above* the
/// build's is what the migrator of the next release leaves while this one is
/// still serving.
async fn stamped(fixture: &Fixture, tenant: TenantId, group: &str, version: i16) {
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    sqlx::query("UPDATE projection_checkpoint SET read_model_version = $2 WHERE group_name = $1")
        .bind(group)
        .bind(version)
        .execute(&mut *conn)
        .await
        .expect("stamps");
}

/// Marks one of `tenant`'s projection groups as built before read-model
/// versions were recorded.
async fn built_before_versions(fixture: &Fixture, tenant: TenantId, group: &str) {
    stamped(fixture, tenant, group, 0).await;
}

/// **A read model newer than the build is as unservable as an older one.**
///
/// During a rolling deploy the migrator swaps a group's tables to the next
/// release's shape while pods on this release are still answering. The
/// request path compared with `<` until 2026-09-14, so those pods served the
/// new tables by this build's rules — numbers nobody could vouch for, from the
/// one window in which nothing else was looking. The projection runner always
/// refused with `!=`; now both do.
#[tokio::test]
async fn a_module_whose_read_model_is_newer_than_the_build_answers_503_too() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    fixture.enable_module(tenant, files::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let files = "/v1/files?owner_kind=tenant&owner_id=SELF";
    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let next_release = <files::Files as erp_projection::ProjectionGroup>::VERSION + 1;
    stamped(&fixture, tenant, files::GROUP_NAME, next_release).await;
    fixture.control.clear_caches();

    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "tables the next release owns were served by this one: {body}"
    );
    assert_eq!(body["code"], "request.read_model_rebuilding", "{body}");

    // Another module's route, on the same tenant, is untouched.
    let (status, body) = fixture
        .as_caller(&token, "GET", "/v1/ledger/accounts", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    fixture.cleanup().await;
}

/// **A module whose read model is older than the build answers 503, and
/// nothing else does** — decision 7 of 2026-09-11. Numbers from tables worked
/// out by rules this build no longer uses are refused, not served; another
/// module's routes are untouched, and the public surface asks the same
/// question the tenant's own does. Once the group is rebuilt the very next
/// request is served: a stale answer is never cached, so the swap needs no
/// invalidation to reach this process.
#[tokio::test]
async fn a_module_whose_read_model_is_older_than_the_build_answers_503_until_it_is_rebuilt() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    fixture.enable_module(tenant, files::setup()).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    open_the_diary(&fixture, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let files = "/v1/files?owner_kind=tenant&owner_id=SELF";
    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    built_before_versions(&fixture, tenant, files::GROUP_NAME).await;
    built_before_versions(&fixture, tenant, booking::GROUP_NAME).await;
    // The first read above was cached as current; an out-of-band change is
    // what `clear_caches` is for.
    fixture.control.clear_caches();

    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "request.read_model_rebuilding", "{body}");
    assert_eq!(body["args"]["module"]["value"], "files", "{body}");

    // And again: a refusal is not remembered as anything a later request
    // could be served on.
    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    // Another module's route, on the same tenant, in the same moment.
    let (status, body) = fixture
        .as_caller(&token, "GET", "/v1/ledger/accounts", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A stranger on the booking site, with no account: the same refusal.
    let (status, body, _) = fixture
        .send(
            get("/v1/booking/public/services")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "request.read_model_rebuilding", "{body}");

    // The deploy step's rebuild, as `bin/migrator` runs it.
    let pool = fixture
        .control
        .maintenance_pool(tenant)
        .await
        .expect("a maintenance pool");
    let owned = files::projections();
    let refs: Vec<&dyn erp_projection::Projection<Group = files::Files>> =
        owned.iter().map(AsRef::as_ref).collect();
    erp_projection::rebuild_swap::<files::Files>(
        &pool,
        files::setup().install_sql,
        &refs,
        files::upcasters(),
        500,
    )
    .await
    .expect("rebuilds");
    pool.close().await;

    let (status, body) = fixture.as_caller(&token, "GET", files, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    fixture.cleanup().await;
}

/// Reads the document limit: status, `ETag`, body.
async fn document_limit(fixture: &Fixture, token: &str) -> (StatusCode, String, serde_json::Value) {
    let response = fixture
        .raw(
            Request::get("/v1/sales/document-limit")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let status = response.status();
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .unwrap_or(serde_json::Value::Null);
    (status, etag, body)
}

async fn set_document_limit(
    fixture: &Fixture,
    token: &str,
    body: serde_json::Value,
    if_match: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::put("/v1/sales/document-limit")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(version) = if_match {
        request = request.header(header::IF_MATCH, version);
    }
    let (status, body, _) = fixture
        .send(request.body(Body::from(body.to_string())).unwrap())
        .await;
    (status, body)
}

/// **The document limit is the owner's versioned setting**, typed: nothing
/// until it is set, refused when it is not more than nothing or names no
/// currency, `If-Match` on the write, and `null` to take it away again.
#[tokio::test]
async fn the_document_limit_is_the_owners_versioned_setting() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.enable_sales(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, etag, body) = document_limit(&fixture, &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        serde_json::json!({ "limit": null }),
        "no limit to start"
    );
    assert_eq!(etag, "\"0\"");

    let limit = |minor: i64, currency: &str| {
        serde_json::json!({ "limit": {
            "amount": { "minor": minor, "currency": currency }, "basis": "before_vat"
        } })
    };
    let (status, body) = set_document_limit(&fixture, &token, limit(0, "SAR"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "sales.document_limit_not_positive", "{body}");
    let (status, body) = set_document_limit(&fixture, &token, limit(100, "riyals"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "request.unknown_currency", "{body}");
    let (_, etag, _) = document_limit(&fixture, &token).await;
    assert_eq!(etag, "\"0\"", "nothing refused was stored");

    let (status, body) =
        set_document_limit(&fixture, &token, limit(1_000_000, "SAR"), Some("\"0\"")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) =
        set_document_limit(&fixture, &token, limit(2_000_000, "SAR"), Some("\"0\"")).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "a stale If-Match: {body}"
    );

    let (status, etag, body) = document_limit(&fixture, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, limit(1_000_000, "SAR"), "reads back as written");

    let (status, body) = set_document_limit(
        &fixture,
        &token,
        serde_json::json!({ "limit": null }),
        Some(&etag),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, _, body) = document_limit(&fixture, &token).await;
    assert_eq!(
        body,
        serde_json::json!({ "limit": null }),
        "taken away again"
    );

    fixture.cleanup().await;
}

/// **Over HTTP: a clerk over the limit is a 403 that names it, at the sales
/// route and at the booking desk, and the worker's pass is not limited.**
///
/// The owner writes the limit through the product. The clerk has no employee
/// record, so no claim can reach them.
#[expect(
    clippy::too_many_lines,
    reason = "one booking from the desk to the worker's pass, beside the sales route"
)]
#[tokio::test]
async fn a_clerk_over_the_document_limit_is_refused_and_the_worker_is_not() {
    let mut fixture = Fixture::new().await;
    let owner = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let clerk = fixture.user("clerk@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(owner, tenant).await;
    fixture.join_as(clerk, tenant, "clerk").await;
    fixture.enable_sales(tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let owner = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let clerk = fixture.token("clerk@acme.test", "hunter2hunter2").await;
    fixture.install_chart(&owner, "acme", "services").await;

    let (status, body) = set_document_limit(
        &fixture,
        &owner,
        serde_json::json!({ "limit": {
            "amount": { "minor": 10_000, "currency": "SAR" }, "basis": "after_vat"
        } }),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // 100 net is 115 after VAT: over the 100 limit.
    let invoice = serde_json::json!({
        "customer": { "name": "Rawabi" },
        "issued_on": "2026-03-01T00:00:00Z",
        "currency": "SAR",
        "lines": [{ "description": "Consulting", "net": 10_000, "vat": "standard" }]
    });
    let (status, body, _) = fixture
        .send(posting(&clerk, "/v1/sales/invoices", &invoice))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "sales.over_document_limit", "{body}");
    assert_eq!(body["args"]["amount"]["value"], "115.00 SAR", "{body}");
    let (status, body, _) = fixture
        .send(posting(&owner, "/v1/sales/invoices", &invoice))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the owner is never limited: {body}"
    );

    // **A key issued the owner's role is not the owner.** Until 2026-09-14
    // `Authority::of` read the role alone, so an integration key walked past
    // the limit the owner is exempt from. A machine is an ordinary member.
    let (status, key) = fixture
        .as_caller(
            &owner,
            "POST",
            "/v1/keys",
            Some(serde_json::json!({ "name": "Integration", "scopes": ["*:post_entries"], "role": "owner" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let secret = key["secret"].as_str().expect("a secret").to_owned();
    let (status, body, _) = fixture
        .send(posting(&secret, "/v1/sales/invoices", &invoice))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a key with the owner's role walked past the document limit: {body}"
    );
    assert_eq!(body["code"], "sales.over_document_limit", "{body}");

    // A booking priced at 200, completed.
    let (status, body, _) = fixture
        .send(posting(
            &owner,
            "/v1/booking/resources",
            &serde_json::json!({ "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1 }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, booked, _) = fixture
        .send(posting(
            &owner,
            "/v1/booking/reservations",
            &serde_json::json!({
                "customer_name": "سارة", "customer_phone": "+966500000000",
                "lines": [{
                    "what": "صبغة", "from": "2026-05-01T09:00:00Z", "until": "2026-05-01T10:00:00Z",
                    "takes": [{ "resource": "CHAIR-1" }],
                    "charge": { "rate": 20_000, "currency": "SAR", "quantity": 1 }
                }]
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{booked}");
    let reservation = booked["id"].as_str().expect("an id").to_owned();
    for stage in ["confirmed", "arrived", "in_service", "completed"] {
        let (status, body, _) = fixture
            .send(
                Request::post(format!("/v1/booking/reservations/{reservation}/stage"))
                    .header(header::AUTHORIZATION, format!("Bearer {owner}"))
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

    // **The desk**: the clerk asks for the invoice, and 230 is over the limit.
    let (status, body, _) = fixture
        .send(
            Request::post(format!("/v1/booking/reservations/{reservation}/invoice"))
                .header(header::AUTHORIZATION, format!("Bearer {clerk}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "sales.over_document_limit", "{body}");

    // **The worker**, once the business asked for billing on completion.
    let (status, body, _) = fixture
        .send(
            Request::put("/v1/booking/billing")
                .header(header::AUTHORIZATION, format!("Bearer {owner}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "on_completion": true }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let billed = erp_api::billing::bill_completions(
        &db,
        "2026-05-02T09:00:00Z".parse().expect("an instant"),
        &erp_eventlog::Metadata::default(),
    )
    .await
    .expect("the pass runs");
    assert_eq!(billed, 1, "nobody at the desk, so nobody limited");

    fixture.cleanup().await;
}

/// **A shelf, over HTTP: declared, received into lots, written off, counted and
/// explained.**
///
/// Phase 19's first box end to end — and the canary with it, because the number
/// a screen shows and the movements it shows underneath are two reads of one log
/// and a business cannot act on a quantity it cannot explain.
///
/// A lot-tracked product, because that is where every part of the slice shows at
/// once: a batch code, a date, earliest-expiry-first picking, and a count of the
/// lot that is left.
///
/// The refusals are asserted in both languages, because a refusal a clerk cannot
/// read is a refusal that becomes a phone call.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a product, two dated deliveries, a write-off, a count, three \
              refusals in two languages and both settings — what the slice is \
              made of"
)]
async fn stock_is_declared_received_written_off_and_counted() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    // **Because every movement out posts, and a posting is dated to a branch.**
    // `ledger::post_entry_in` checks the `X-Branch` this request carries
    // against the log, so a shelf at a branch nobody opened cannot be written
    // off — which is the right answer and is new in this slice.
    fixture.enable_module(tenant, branches::setup()).await;
    fixture.enable_module(tenant, inventory::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let milk = idem("PROD-MILK");
    let olaya = idem("BRANCH-OLAYA");
    let post = |path: String, key: &str, branch: Option<&str>, body: serde_json::Value| {
        let mut request = Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idem(key));
        if let Some(branch) = branch {
            request = request.header("x-branch", branch);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };

    // The books, because a write-off books its loss and a count books its
    // discrepancy — and because the posting-accounts setting is checked against
    // the tenant's own chart before it can be stored.
    let (status, installed, _) = fixture
        .send(
            Request::post("/v1/ledger/chart")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "template": "services", "currency": "SAR" }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{installed}");

    // The place the stock sits, opened before anything is received into it.
    let (status, branch, _) = fixture
        .send(post(
            "/v1/branches".to_owned(),
            "BRANCH-OLAYA",
            None,
            serde_json::json!({
                "name": "العليا",
                "address": { "street": "King Fahd Road", "city": "Riyadh",
                             "country": "SA" }
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{branch}");
    assert_eq!(branch["id"], olaya);

    // A product with no unit is refused, and says so in the caller's language.
    let (status, refused, _) = fixture
        .send(
            Request::post("/v1/inventory/products")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_LANGUAGE, "ar")
                .header("Idempotency-Key", idem("PROD-NOTHING"))
                .body(Body::from(
                    serde_json::json!({ "name": "حليب", "unit": "  " }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "inventory.needs_a_name_and_a_unit");
    assert!(
        refused["detail"]
            .as_str()
            .expect("a detail")
            .contains("الوحدة"),
        "the refusal did not answer in Arabic: {refused}"
    );

    // And so is a way of tracking one that this build does not know.
    let (status, refused, _) = fixture
        .send(post(
            "/v1/inventory/products".to_owned(),
            "PROD-BATCH",
            None,
            serde_json::json!({ "name": "حليب", "unit": "bottle", "tracking": "batch" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "inventory.not_a_tracking_mode");

    let (status, declared, _) = fixture
        .send(post(
            "/v1/inventory/products".to_owned(),
            "PROD-MILK",
            None,
            serde_json::json!({ "name": "حليب طازج", "unit": "bottle", "tracking": "lot" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{declared}");
    assert_eq!(declared["id"], milk);

    // Two crates, the second expiring first. Both at Olaya, because stock is
    // held per branch and this business has one.
    let mut lots = Vec::new();
    for (key, quantity, minor, code, expires) in [
        ("RCV-1", 24, 12_000, "B-2026-04-05", "2026-04-21"),
        ("RCV-2", 12, 7_200, "B-2026-04-02", "2026-04-11"),
    ] {
        let (status, body, _) = fixture
            .send(post(
                format!("/v1/inventory/stock/{milk}/receipts"),
                key,
                Some(olaya.as_str()),
                serde_json::json!({
                    "quantity": quantity,
                    "value": { "minor": minor, "currency": "SAR" },
                    "code": code,
                    "expires_on": expires,
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        // **The lot is named after the shelf and the receipt**, never minted —
        // the shelf because an idempotency key is unique only to the client
        // that sent it, and `lot.id` is a key across the whole tenant.
        assert_eq!(body["id"], format!("lot.{milk}.{olaya}.{}", idem(key)));
        lots.push(body["id"].as_str().expect("a lot id").to_owned());
    }

    // A crate of milk with no batch code on it is refused: this product is
    // tracked by lot, and a delivery that will not say which batch it is cannot
    // be recalled or thrown away by date.
    let (status, refused, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{milk}/receipts"),
            "RCV-3",
            Some(olaya.as_str()),
            serde_json::json!({
                "quantity": 6, "value": { "minor": 3_000, "currency": "SAR" }
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "inventory.needs_a_lot_code");

    // The older crate goes off and is thrown out. No lot is named, so the
    // picking rule takes the one expiring first.
    let (status, thrown, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{milk}/write-offs"),
            "WOF-1",
            Some(olaya.as_str()),
            serde_json::json!({ "reason": "expired", "quantity": 12 }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{thrown}");

    // More than is on the shelf is refused, in English this time — this is
    // somebody holding the goods, not a till.
    let (status, short, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{milk}/write-offs"),
            "WOF-2",
            Some(olaya.as_str()),
            serde_json::json!({ "reason": "damaged", "quantity": 99 }),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{short}");
    assert_eq!(short["code"], "inventory.not_enough_stock");
    assert!(
        short["detail"]
            .as_str()
            .expect("a detail")
            .contains("Count the shelf"),
        "{short}"
    );

    // A count of a batch that is not on the shelf is refused: the lot a count
    // names reaches the command, and the one thrown out has closed.
    let (status, refused, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{milk}/counts"),
            "CNT-0",
            Some(olaya.as_str()),
            serde_json::json!({ "lot": lots[1], "declared": 22 }),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["code"], "inventory.no_such_lot");

    // Somebody counts **the shelf**, naming no batch, and finds two missing. The
    // shortage comes off the lot that goes out next, which is the one left.
    let (status, counted, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{milk}/counts"),
            "CNT-1",
            Some(olaya.as_str()),
            serde_json::json!({ "declared": 22 }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{counted}");

    // Stock of a product nobody declared is refused on the state of the world,
    // not on the shape of the request.
    let (status, unknown, _) = fixture
        .send(post(
            format!("/v1/inventory/stock/{}/receipts", idem("PROD-GHOST")),
            "RCV-4",
            Some(olaya.as_str()),
            serde_json::json!({ "quantity": 1, "value": { "minor": 1, "currency": "SAR" } }),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{unknown}");
    assert_eq!(unknown["code"], "inventory.no_such_product");
    assert!(
        unknown["detail"]
            .as_str()
            .expect("a detail")
            .contains("There is no product"),
        "{unknown}"
    );

    fixture
        .project::<inventory::Inventory>(tenant, &inventory::projections(), inventory::upcasters())
        .await;
    // The books too, because the last thing this test asserts is what they now
    // hold — and an unprojected ledger would say nothing whatever happened.
    fixture
        .project::<ledger::Ledger>(tenant, &ledger::projections(), ledger::upcasters())
        .await;

    let get = |path: &str| {
        Request::get(path.to_owned())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, shelves, _) = fixture.send(get("/v1/inventory/stock")).await;
    assert_eq!(status, StatusCode::OK, "{shelves}");
    let shelf = &shelves["items"][0];
    assert_eq!(shelf["product"], milk);
    assert_eq!(shelf["branch"], olaya, "stock is held per branch");
    assert_eq!(
        shelf["on_hand"], 22,
        "twenty-four less the two nobody found"
    );
    // 120.00 for twenty-four, less two of them at their own lot's cost.
    assert_eq!(shelf["value"]["minor"], 11_000);

    // **What is left is one lot, and the listing says which and when.**
    let (status, open, _) = fixture.send(get("/v1/inventory/lots")).await;
    assert_eq!(status, StatusCode::OK, "{open}");
    let open = open["items"].as_array().expect("a list");
    assert_eq!(
        open.len(),
        1,
        "the crate that expired first emptied and closed"
    );
    assert_eq!(open[0]["id"], lots[0]);
    assert_eq!(open[0]["code"], "B-2026-04-05");
    assert_eq!(open[0]["expires_on"], "2026-04-21");
    assert_eq!(open[0]["quantity"], 24, "what arrived");
    assert_eq!(open[0]["remaining"], 22, "what is still there");

    let (status, movements, _) = fixture.send(get("/v1/inventory/movements")).await;
    assert_eq!(status, StatusCode::OK, "{movements}");
    let movements = movements["items"].as_array().expect("a list");
    assert_eq!(movements.len(), 4, "two in, one out, one counted");

    let thrown = movements
        .iter()
        .find(|row| row["kind"] == "written_off")
        .expect("the write-off is in the list");
    assert_eq!(thrown["reason"], "expired");
    assert_eq!(thrown["quantity"], -12);
    assert_eq!(
        thrown["lot"], lots[1],
        "the rule took the crate expiring first, not the one received first"
    );

    let count = movements
        .iter()
        .find(|row| row["kind"] == "counted")
        .expect("the count is in the list");
    assert_eq!(count["expected"], 24);
    assert_eq!(count["declared"], 22);
    assert_eq!(count["quantity"], -2);
    assert_eq!(count["lot"], lots[0]);

    // **The canary**: what is on hand is the sum of what moved, lot by lot.
    for lot in open {
        assert_eq!(
            movements
                .iter()
                .filter(|row| row["lot"] == lot["id"])
                .filter_map(|row| row["quantity"].as_i64())
                .sum::<i64>(),
            lot["remaining"].as_i64().expect("a quantity"),
            "a lot holds a quantity its movements do not explain"
        );
    }
    assert_eq!(
        movements
            .iter()
            .filter_map(|row| row["quantity"].as_i64())
            .sum::<i64>(),
        shelf["on_hand"].as_i64().expect("a quantity"),
        "the shelf holds a quantity its movements do not explain"
    );

    // **And what it cost, in the books.** The crate that went off is 72.00 —
    // what that lot was carried at, not an average across both — and the two
    // bottles nobody found are 2 of 24 of 120.00, which is 10.00. Both are
    // losses and both came off the asset. The deliveries put it there: a
    // receipt debits `1300` and credits `2010 Goods received, not invoiced`,
    // so the asset is 192.00 delivered less 82.00 lost, and the holding account
    // is the whole 192.00 this tenant has never been billed for.
    let (status, accounts, _) = fixture.send(get("/v1/ledger/accounts")).await;
    assert_eq!(status, StatusCode::OK, "{accounts}");
    let balance = |code: &str| {
        accounts
            .as_array()
            .expect("a list")
            .iter()
            .find(|account| account["code"] == code)
            .and_then(|account| account["balance"].as_i64())
            .expect("an account")
    };
    assert_eq!(
        balance("5900"),
        7_200 + 1_000,
        "the crate and the two bottles"
    );
    assert_eq!(
        balance("1300"),
        12_000 + 7_200 - (7_200 + 1_000),
        "what was delivered, less what was lost"
    );
    assert_eq!(
        balance("2010"),
        -(12_000 + 7_200),
        "nobody has billed this tenant for any of it"
    );

    // Where it posts, and the ETag that guards the choice.
    let response = fixture.raw(get("/v1/inventory/posting-accounts")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let chosen: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .expect("reads"),
    )
    .expect("json");
    assert_eq!(chosen["inventory"], "1300");
    assert_eq!(chosen["goods_received"], "2010");
    assert_eq!(chosen["cogs"], "5010");
    assert_eq!(chosen["variance"], "5900");
    assert_eq!(chosen["waste"], "5900");

    // An account this tenant's chart does not have is refused rather than
    // stored — the guard `sales` makes, asked of the log.
    // **`If-Match` only when there is one to send.** Without it the write is
    // unconditional, which is what a first setting has to be.
    let put = |path: &str, body: serde_json::Value, etag: Option<&str>| {
        let mut request = Request::put(path.to_owned())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(etag) = etag {
            request = request.header(header::IF_MATCH, etag);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };
    let (status, refused, _) = fixture
        .send(put(
            "/v1/inventory/posting-accounts",
            serde_json::json!({
                "inventory": "1300", "goods_received": "2010", "cogs": "5010",
                "variance": "5900", "waste": "9999"
            }),
            Some(etag.as_str()),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "ledger.no_such_account");

    let (status, stored, _) = fixture
        .send(put(
            "/v1/inventory/posting-accounts",
            // **Spoilage apart from shrinkage**, which is the whole reason the
            // two are separate fields: one PUT, no code change, and a manager
            // reads what was thrown away without a count variance in it.
            serde_json::json!({
                "inventory": "1300", "goods_received": "2010", "cogs": "5010",
                "variance": "5900", "waste": "5400"
            }),
            Some(etag.as_str()),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{stored}");

    let (status, chosen, _) = fixture.send(get("/v1/inventory/posting-accounts")).await;
    assert_eq!(status, StatusCode::OK, "{chosen}");
    assert_eq!(chosen["waste"], "5400");
    assert_eq!(
        chosen["variance"], "5900",
        "and the count's stays where it was"
    );

    // **And how long before a date the business wants warning.** Nothing reads
    // it yet and the route says so; it ships now so the answer is already there
    // when the check that reads it arrives.
    let (status, window, _) = fixture.send(get("/v1/inventory/expiry-window")).await;
    assert_eq!(status, StatusCode::OK, "{window}");
    assert_eq!(window["days"], 30);

    let (status, refused, _) = fixture
        .send(put(
            "/v1/inventory/expiry-window",
            serde_json::json!({ "days": -1 }),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "inventory.not_a_window");

    let (status, stored, _) = fixture
        .send(put(
            "/v1/inventory/expiry-window",
            serde_json::json!({ "days": 7 }),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{stored}");
    let (_, window, _) = fixture.send(get("/v1/inventory/expiry-window")).await;
    assert_eq!(window["days"], 7);

    fixture.cleanup().await;
}

/// **The shelves at a glance, over HTTP.** An empty tenant's summary is empty;
/// a batch is going off or gone by the tenant's own day and the tenant's own
/// window; a shelf below zero says what it owes; and `branch` narrows the
/// summary, the stock and the lots to one branch, each row naming its product.
///
/// **The tenant's clock is set to a zone whose day is not UTC's** when the test
/// runs — of UTC-12 and UTC+14 one always is — and the two branches hold batches
/// dated the tenant's today and the day before. Read by UTC's day, one of them
/// lands in the other column, whichever way the zone differs. The zone is one
/// whose day will not turn for an hour, so the day the test dates its batches
/// by is still the day when the route reads its clock.
///
/// **The window is two days, set through its route**, and Olaya also holds
/// batches dated its last day and the day after: read under the default thirty
/// days, the day after would be going off too.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a chart, two branches, two products, four dated batches, a short sale and three routes read twice"
)]
async fn stock_is_summarised_a_branch_at_a_time() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_ledger(tenant).await;
    fixture.enable_module(tenant, branches::setup()).await;
    fixture.enable_module(tenant, inventory::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let get = |path: &str| {
        Request::get(path.to_owned())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let send = |method: &str, path: &str, branch: Option<&str>, body: serde_json::Value| {
        let mut request = Request::builder()
            .method(method)
            .uri(path.to_owned())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idem(&format!("{path}{body}")));
        if let Some(branch) = branch {
            request = request.header("x-branch", branch);
        }
        request.body(Body::from(body.to_string())).unwrap()
    };

    // **An empty tenant**: nothing on a shelf is an empty summary.
    let (status, empty, _) = fixture.send(get("/v1/inventory/summary")).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["branches"], serde_json::json!([]), "{empty}");

    let now = chrono::Utc::now();
    let utc = erp_types::Calendar::UTC.day(now);
    // UTC-12 is on another day before noon UTC and UTC+14 from ten, and each
    // turns at one of those hours; an hour's margin always leaves one of them.
    let zone = ["Etc/GMT+12", "Pacific/Kiritimati"]
        .into_iter()
        .find(|zone| {
            let calendar = erp_types::Calendar::named(zone).expect("a zone");
            calendar.day(now) != utc
                && calendar.day(now + chrono::Duration::hours(1)) == calendar.day(now)
        })
        .expect("one of the two is on another day than UTC, and stays on it for an hour");
    let (status, set, _) = fixture
        .send(send(
            "PUT",
            "/v1/tenant/calendar",
            None,
            serde_json::json!({ "zone": zone }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{set}");
    let today = erp_types::Calendar::named(zone).expect("a zone").day(now);
    let (status, set, _) = fixture
        .send(send(
            "PUT",
            "/v1/inventory/expiry-window",
            None,
            serde_json::json!({ "days": 2 }),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{set}");
    let last_day = today + chrono::Days::new(2);

    let (status, installed, _) = fixture
        .send(send(
            "POST",
            "/v1/ledger/chart",
            None,
            serde_json::json!({ "template": "services", "currency": "SAR" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    let mut opened = Vec::new();
    for name in ["العليا", "الملز"] {
        let (status, branch, _) = fixture
            .send(send(
                "POST",
                "/v1/branches",
                None,
                serde_json::json!({
                    "name": name,
                    "address": { "street": "King Fahd Road", "city": "Riyadh", "country": "SA" }
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{branch}");
        opened.push(branch["id"].as_str().expect("an id").to_owned());
    }
    let (olaya, malaz) = (&opened[0], &opened[1]);
    let (status, milk, _) = fixture
        .send(send(
            "POST",
            "/v1/inventory/products",
            None,
            serde_json::json!({ "name": "حليب طازج", "unit": "bottle", "tracking": "lot" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{milk}");
    let milk = milk["id"].as_str().expect("an id").to_owned();

    // Olaya's batches good until the tenant's today and the window's last day
    // are going off, and the one good a day longer is not yet; Malaz's was good
    // until the day before, so it has gone.
    for (branch, code, expires_on) in [
        (olaya, "B-TODAY", today),
        (olaya, "B-LAST-DAY", last_day),
        (olaya, "B-OUTSIDE", last_day + chrono::Days::new(1)),
        (malaz, "B-YESTERDAY", today.pred_opt().expect("a day")),
    ] {
        let (status, received, _) = fixture
            .send(send(
                "POST",
                &format!("/v1/inventory/stock/{milk}/receipts"),
                Some(branch),
                serde_json::json!({
                    "quantity": 6,
                    "value": { "minor": 3_000, "currency": "SAR" },
                    "code": code,
                    "expires_on": expires_on.to_string()
                }),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{received}");
    }

    // **Beans sold short at Olaya**: two bags at 5.00 received, five sold the
    // way `sales` sells them, so three bags are owed at 5.00.
    let (status, beans, _) = fixture
        .send(send(
            "POST",
            "/v1/inventory/products",
            None,
            serde_json::json!({ "name": "حبوب إسبريسو", "unit": "bag" }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{beans}");
    let beans = beans["id"].as_str().expect("an id").to_owned();
    let (status, received, _) = fixture
        .send(send(
            "POST",
            &format!("/v1/inventory/stock/{beans}/receipts"),
            Some(olaya),
            serde_json::json!({ "quantity": 2, "value": { "minor": 1_000, "currency": "SAR" } }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{received}");
    let db = fixture
        .control
        .enter_for_maintenance(tenant)
        .await
        .expect("maintenance entry");
    let mut tx = db.begin().await.expect("a transaction");
    inventory::consume_in(
        &mut tx,
        &erp_types::AggregateId::new(&beans).expect("an id"),
        &inventory::Consumption {
            quantity: Some(5),
            lot: None,
            serials: Vec::new(),
            reference: "inv-1.1".to_owned(),
            at: now,
        },
        &erp_eventlog::Metadata::default().at_branch(olaya),
    )
    .await
    .expect("a plain product sells short");
    tx.commit().await.expect("commits");

    fixture
        .project::<inventory::Inventory>(tenant, &inventory::projections(), inventory::upcasters())
        .await;

    let (status, summary, _) = fixture.send(get("/v1/inventory/summary")).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(
        summary["today"],
        today.to_string(),
        "the tenant's day in {zone}, not UTC's {utc}"
    );
    assert_eq!(
        summary["expiring_through"],
        last_day.to_string(),
        "the tenant's two days, not the default thirty"
    );
    let rows = summary["branches"].as_array().expect("a list").clone();
    assert_eq!(rows.len(), 2, "{summary}");
    let row = |branch: &str| {
        rows.iter()
            .find(|row| row["branch"] == branch)
            .cloned()
            .unwrap_or_else(|| panic!("no row for {branch}: {summary}"))
    };
    assert_eq!(
        (&row(olaya)["expiring"], &row(olaya)["expired"]),
        (&serde_json::json!(2), &serde_json::json!(0)),
        "good until the tenant's today and the window's last day, and the day after not yet: {summary}"
    );
    assert_eq!(
        (&row(malaz)["expiring"], &row(malaz)["expired"]),
        (&serde_json::json!(0), &serde_json::json!(1)),
        "good until the tenant's yesterday has gone: {summary}"
    );
    assert_eq!(row(olaya)["products"], 2);
    assert_eq!(
        row(olaya)["value"],
        serde_json::json!([{ "minor": 3 * 3_000 - 1_500, "currency": "SAR" }])
    );
    assert_eq!(
        row(olaya)["below_zero"],
        serde_json::json!([{
            "product": beans,
            "name": "حبوب إسبريسو",
            "on_hand": -3,
            "owes": { "minor": 1_500, "currency": "SAR" }
        }]),
        "three bags owed at 5.00: {summary}"
    );
    assert_eq!(row(malaz)["below_zero"], serde_json::json!([]));

    // **One branch, when asked for**, on all three routes.
    let (status, one, _) = fixture
        .send(get(&format!("/v1/inventory/summary?branch={malaz}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["branches"], serde_json::json!([row(malaz)]));

    let (status, every, _) = fixture.send(get("/v1/inventory/stock")).await;
    assert_eq!(status, StatusCode::OK, "{every}");
    assert_eq!(every["items"].as_array().map(Vec::len), Some(3));
    let (status, shelves, _) = fixture
        .send(get(&format!("/v1/inventory/stock?branch={malaz}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{shelves}");
    let shelves = shelves["items"].as_array().expect("a list").clone();
    assert_eq!(shelves.len(), 1, "{shelves:?}");
    assert_eq!(shelves[0]["branch"], malaz.as_str());
    assert_eq!(shelves[0]["name"], "حليب طازج");

    let (status, lots, _) = fixture
        .send(get(&format!("/v1/inventory/lots?branch={olaya}")))
        .await;
    assert_eq!(status, StatusCode::OK, "{lots}");
    let lots = lots["items"].as_array().expect("a list").clone();
    assert_eq!(lots.len(), 3, "{lots:?}");
    for lot in &lots {
        assert_eq!(lot["branch"], olaya.as_str());
        assert_eq!(lot["name"], "حليب طازج");
    }

    fixture.cleanup().await;
}
