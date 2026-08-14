//! A tenant with every module enabled, filled with plausible data.
//!
//! # Why this is a client, not a script
//!
//! Every step goes through the public HTTP API — sign up, install a chart, issue
//! an invoice, take a payment. Nothing here reaches into a database or calls a
//! command directly, which is the point: a demo built out of internal calls can
//! be perfect while the API a customer would use is broken.
//!
//! So this doubles as the widest integration test the system has. `tests/demo.rs`
//! runs it and then asserts the three things that are hard to check any other
//! way:
//!
//! - every module is enabled and answering,
//! - every projection group rebuilds from the log to exactly what is live,
//! - every invariant is clean.
//!
//! # Why it is deterministic
//!
//! Fixed dates, fixed amounts, fixed identifiers. A demo whose numbers change
//! between runs cannot be screenshotted, cannot be talked through twice, and
//! turns a CI failure into "did the data change or did the code?".

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use spa_api::{AppState, router};
use spa_control::{ControlPlane, TenantDb};
use spa_types::TenantId;
use tower::ServiceExt;

/// Every module this build offers — all of them, by construction.
///
/// Read from [`spa_api::modules`] rather than listed here, so "the demo has
/// every module enabled" is true because it cannot be false. A module added to
/// the API is a module this demo signs up for on the next run.
///
/// What that does *not* buy: a new module still needs teaching to the seeder
/// below, or the demo enables it and shows nothing. `tests/demo.rs` asks each
/// module for something only it can answer, which is where that shows up.
#[must_use]
pub fn modules() -> Vec<&'static str> {
    spa_api::modules()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// What the demo produced, and how to get into it.
#[derive(Debug, Clone)]
pub struct Seeded {
    pub tenant: TenantId,
    pub slug: String,
    /// The tenant's own database. Derived from the id rather than the slug, so
    /// it is asked for rather than guessed.
    pub database: String,
    pub email: String,
    /// A live session for the owner. Signing up logs you in.
    pub token: String,
    pub invoices: usize,
    pub payments: usize,
    pub journal_entries: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    /// A request the demo made was refused. Carries the body, because a demo
    /// that fails in CI is read by someone who was not watching it run.
    #[error("{method} {path} returned {status}: {body}")]
    Refused {
        method: &'static str,
        path: String,
        status: StatusCode,
        body: String,
    },
    #[error("{path} returned a body this demo did not understand: {body}")]
    Unexpected { path: String, body: String },
    #[error(transparent)]
    Access(#[from] spa_control::AccessError),
    #[error(transparent)]
    Pool(#[from] spa_control::PoolError),
    #[error(transparent)]
    Projection(#[from] spa_projection::RunError),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Prepares a database that has never run anything.
///
/// Migrates the control schema and registers the cluster tenants are placed on.
/// Both idempotent, so running it against a live deployment is a no-op.
///
/// # Why this lives here and not in `bin/api`
///
/// Migrating on start is a deployment decision, and several API instances
/// racing to do it is a bad one. A one-shot seeder is the exception: `just demo`
/// is usually the first thing pointed at a fresh database, and a demo that
/// fails with `relation "cluster" does not exist` is a demo nobody sees.
pub async fn bootstrap(
    control: &ControlPlane,
    cluster: &str,
    url_variable: &str,
) -> Result<(), DemoError> {
    control.migrate().await?;
    control
        .register_cluster(
            cluster,
            url_variable,
            None,
            DEFAULT_CAPACITY,
            DEFAULT_CAPACITY,
            spa_control::Actor::system(),
        )
        .await?;
    Ok(())
}

/// Room for a demo and whatever else is on the box. Not a production number —
/// a real cluster's capacity is sized from measurement (architecture D13).
const DEFAULT_CAPACITY: i32 = 10_000;

/// Builds the demo tenant and everything in it.
///
/// Idempotent only in the sense that every write it makes is — re-running it
/// against an existing slug fails at signup, which is the honest outcome. Drop
/// the tenant and run it again.
pub async fn seed(state: &AppState, slug: &str, password: &str) -> Result<Seeded, DemoError> {
    let app = router(state.clone());

    let signed_up = sign_up(&app, slug, password).await?;
    let tenant = signed_up.tenant;
    let token = signed_up.token.clone();

    install_chart(&app, slug, &token).await?;
    let journal_entries = seed_opening_balances(&app, slug, &token).await?;
    let invoices = seed_invoices(&app, slug, &token).await?;
    let payments = seed_payments(&app, slug, &token).await?;

    // Drive the projections, so the demo has something to show the moment it
    // finishes rather than whenever a worker next visits. A deployment with a
    // worker running would get there on its own; this makes the demo usable
    // without one.
    project(&state.control, tenant).await?;

    let database = state
        .control
        .tenant(tenant)
        .await?
        .ok_or_else(|| DemoError::Unexpected {
            path: "/v1/signups".to_owned(),
            body: format!("tenant {tenant} vanished between signing up and reading it back"),
        })?
        .database_name;

    Ok(Seeded {
        tenant,
        slug: slug.to_owned(),
        database,
        email: signed_up.email,
        token,
        invoices,
        payments,
        journal_entries,
    })
}

/// Runs every projection group to the head of the log.
pub async fn project(control: &Arc<ControlPlane>, tenant: TenantId) -> Result<(), DemoError> {
    let db = control.enter_for_maintenance(tenant).await?;
    advance::<ledger::Ledger>(&db, &ledger::projections(), ledger::upcasters()).await?;
    advance::<sales::Sales>(&db, &sales::projections(), sales::upcasters()).await?;
    Ok(())
}

async fn advance<G: spa_projection::ProjectionGroup>(
    db: &TenantDb,
    projections: &[Arc<dyn spa_projection::Projection<Group = G>>],
    upcasters: &spa_eventlog::Upcasters,
) -> Result<(), DemoError> {
    let refs: Vec<&dyn spa_projection::Projection<Group = G>> =
        projections.iter().map(AsRef::as_ref).collect();

    loop {
        let mut tx = db.begin().await?;
        match spa_projection::run_once_in::<G>(&mut tx, &refs, upcasters, 500).await {
            Ok(spa_projection::Progress::Advanced { .. }) => {
                tx.commit().await.map_err(spa_projection::RunError::from)?;
            }
            Ok(_) => {
                tx.rollback()
                    .await
                    .map_err(spa_projection::RunError::from)?;
                return Ok(());
            }
            Err(e) => {
                tx.rollback()
                    .await
                    .map_err(spa_projection::RunError::from)?;
                return Err(e.into());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The steps
// ---------------------------------------------------------------------------

struct SignedUp {
    tenant: TenantId,
    email: String,
    token: String,
}

async fn sign_up(app: &axum::Router, slug: &str, password: &str) -> Result<SignedUp, DemoError> {
    let email = format!("owner@{slug}.example");
    let body = post(
        app,
        "/v1/signups",
        None,
        &serde_json::json!({
            "slug": slug,
            "company": "Rawabi Consulting · روابي للاستشارات",
            "email": email,
            "password": password,
            "modules": modules(),
        }),
        StatusCode::CREATED,
    )
    .await?;

    let tenant = body["tenant"]
        .as_str()
        .and_then(|s| s.parse::<TenantId>().ok())
        .ok_or_else(|| DemoError::Unexpected {
            path: "/v1/signups".to_owned(),
            body: body.to_string(),
        })?;

    let token = body["token"]
        .as_str()
        .ok_or_else(|| DemoError::Unexpected {
            path: "/v1/signups".to_owned(),
            body: body.to_string(),
        })?
        .to_owned();

    Ok(SignedUp {
        tenant,
        email,
        token,
    })
}

async fn install_chart(app: &axum::Router, slug: &str, token: &str) -> Result<(), DemoError> {
    post(
        app,
        &format!("/v1/tenants/{slug}/ledger/chart"),
        Some(token),
        &serde_json::json!({ "template": "services", "currency": "SAR" }),
        StatusCode::OK,
    )
    .await?;
    Ok(())
}

/// The entries a real set of books starts with, and which no sale would create:
/// the owner putting money in, and the rent.
///
/// Without these the ledger is entirely sales-driven, and a demo of an
/// accounting system in which nothing was ever *spent* is not a demo of an
/// accounting system.
async fn seed_opening_balances(
    app: &axum::Router,
    slug: &str,
    token: &str,
) -> Result<usize, DemoError> {
    let entries = [
        (
            "OPENING-1",
            "2026-01-01T00:00:00Z",
            "Owner's opening capital",
            serde_json::json!([
                { "account": "1010", "amount": { "minor": 20_000_000, "currency": "SAR" } },
                { "account": "3000", "amount": { "minor": -20_000_000, "currency": "SAR" } },
            ]),
        ),
        (
            "RENT-2026-01",
            "2026-01-31T00:00:00Z",
            "January rent",
            serde_json::json!([
                { "account": "5100", "amount": { "minor": 1_200_000, "currency": "SAR" } },
                { "account": "1010", "amount": { "minor": -1_200_000, "currency": "SAR" } },
            ]),
        ),
        (
            "SALARIES-2026-01",
            "2026-01-31T00:00:00Z",
            "January salaries",
            serde_json::json!([
                { "account": "5000", "amount": { "minor": 4_500_000, "currency": "SAR" } },
                { "account": "1010", "amount": { "minor": -4_500_000, "currency": "SAR" } },
            ]),
        ),
    ];

    for (id, occurred_on, memo, lines) in &entries {
        post(
            app,
            &format!("/v1/tenants/{slug}/ledger/entries"),
            Some(token),
            &serde_json::json!({
                "id": id,
                "occurred_on": occurred_on,
                "memo": memo,
                "lines": lines,
            }),
            StatusCode::OK,
        )
        .await?;
    }

    Ok(entries.len())
}

/// Five invoices across three customers, spanning every VAT treatment.
///
/// The mix is the point: a demo with one 15% invoice shows nothing about the
/// tax breakdown a Saudi invoice has to print.
async fn seed_invoices(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    let invoices = [
        (
            "INV-2026-001",
            "2026-01-08T00:00:00Z",
            "Al Faisaliah Group",
            Some("310122393500003"),
            serde_json::json!([
                { "description": "Systems consulting — January", "net": 4_500_000, "vat": "standard" },
            ]),
        ),
        (
            "INV-2026-002",
            "2026-01-22T00:00:00Z",
            "Najd Logistics",
            Some("311234567800003"),
            serde_json::json!([
                { "description": "Integration work", "net": 2_800_000, "vat": "standard" },
                { "description": "Support retainer", "net": 600_000, "vat": "standard" },
            ]),
        ),
        (
            "INV-2026-003",
            "2026-02-05T00:00:00Z",
            "Gulf Freight DMCC",
            None,
            serde_json::json!([
                { "description": "Consulting for an overseas client", "net": 3_300_000, "vat": "zero" },
            ]),
        ),
        (
            "INV-2026-004",
            "2026-02-19T00:00:00Z",
            "Al Faisaliah Group",
            Some("310122393500003"),
            serde_json::json!([
                { "description": "Systems consulting — February", "net": 4_500_000, "vat": "standard" },
                { "description": "Staff accommodation recharge", "net": 900_000, "vat": "exempt" },
                { "description": "Early settlement discount", "net": -150_000, "vat": "standard" },
            ]),
        ),
        (
            "INV-2026-005",
            "2026-03-04T00:00:00Z",
            "Najd Logistics",
            Some("311234567800003"),
            serde_json::json!([
                { "description": "Change requests", "net": 1_150_000, "vat": "standard" },
            ]),
        ),
    ];

    for (id, issued_on, customer, vat_number, lines) in &invoices {
        post(
            app,
            &format!("/v1/tenants/{slug}/sales/invoices"),
            Some(token),
            &serde_json::json!({
                "id": id,
                "customer": { "name": customer, "vat_number": vat_number },
                "issued_on": issued_on,
                "due_on": "2026-04-01T00:00:00Z",
                "currency": "SAR",
                "lines": lines,
            }),
            StatusCode::CREATED,
        )
        .await?;
    }

    Ok(invoices.len())
}

/// Two settled invoices, one part-paid, two untouched — so a receivables list
/// has something in every state.
async fn seed_payments(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    let payments = [
        // 45,000.00 + 15% = 51,750.00
        (
            "INV-2026-001",
            "SNB-88401",
            5_175_000,
            "2026-02-01T00:00:00Z",
        ),
        // (28,000.00 + 6,000.00) + 15% = 39,100.00
        (
            "INV-2026-002",
            "SNB-88907",
            3_910_000,
            "2026-02-15T00:00:00Z",
        ),
        // Zero-rated: 33,000.00, and only half of it has arrived.
        (
            "INV-2026-003",
            "RJHI-1204",
            1_650_000,
            "2026-03-01T00:00:00Z",
        ),
    ];

    for (invoice, reference, minor, received_on) in &payments {
        post(
            app,
            &format!("/v1/tenants/{slug}/sales/invoices/{invoice}/payments"),
            Some(token),
            &serde_json::json!({
                "reference": reference,
                "amount": { "minor": minor, "currency": "SAR" },
                "received_on": received_on,
                "account": "1010",
            }),
            StatusCode::OK,
        )
        .await?;
    }

    Ok(payments.len())
}

// ---------------------------------------------------------------------------

async fn post(
    app: &axum::Router,
    path: &str,
    token: Option<&str>,
    body: &serde_json::Value,
    expected: StatusCode,
) -> Result<serde_json::Value, DemoError> {
    let mut request = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request =
        request
            .body(Body::from(body.to_string()))
            .map_err(|e| DemoError::Unexpected {
                path: path.to_owned(),
                body: e.to_string(),
            })?;

    send(app, "POST", path, request, expected).await
}

/// Issues a request and insists on the status the demo expected.
///
/// Anything else stops the demo. A seeder that logs a failure and carries on
/// produces a half-built demo that looks like a product bug (L6).
pub async fn get(
    app: &axum::Router,
    path: &str,
    token: &str,
) -> Result<serde_json::Value, DemoError> {
    let request = Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .map_err(|e| DemoError::Unexpected {
            path: path.to_owned(),
            body: e.to_string(),
        })?;

    send(app, "GET", path, request, StatusCode::OK).await
}

async fn send(
    app: &axum::Router,
    method: &'static str,
    path: &str,
    request: Request<Body>,
    expected: StatusCode,
) -> Result<serde_json::Value, DemoError> {
    let response = app
        .clone()
        .oneshot(request)
        .await
        .map_err(|e: std::convert::Infallible| DemoError::Unexpected {
            path: path.to_owned(),
            body: e.to_string(),
        })?;

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .map_err(|e| DemoError::Unexpected {
            path: path.to_owned(),
            body: e.to_string(),
        })?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);

    if status != expected {
        return Err(DemoError::Refused {
            method,
            path: path.to_owned(),
            status,
            body: json.to_string(),
        });
    }

    Ok(json)
}
