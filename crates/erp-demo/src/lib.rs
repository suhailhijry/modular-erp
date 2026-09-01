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
use erp_api::{AppState, router};
use erp_control::{ControlPlane, TenantDb};
use erp_types::TenantId;
use tower::ServiceExt;

/// Every module this build offers — all of them, by construction.
///
/// Read from [`erp_api::modules`] rather than listed here, so "the demo has
/// every module enabled" is true because it cannot be false. A module added to
/// the API is a module this demo signs up for on the next run.
///
/// What that does *not* buy: a new module still needs teaching to the seeder
/// below, or the demo enables it and shows nothing. `tests/demo.rs` asks each
/// module for something only it can answer, which is where that shows up.
#[must_use]
pub fn modules() -> Vec<&'static str> {
    erp_api::modules()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// What the demo produced, and how to get into it.
#[derive(Debug, Clone)]
pub struct Seeded {
    pub tenant: TenantId,
    /// When the reaper will destroy it, if it was given a life span.
    pub expires_after: Option<std::time::Duration>,
    pub slug: String,
    /// The tenant's own database. Derived from the id rather than the slug, so
    /// it is asked for rather than guessed.
    pub database: String,
    pub email: String,
    /// A live session for the owner. Signing up logs you in.
    pub token: String,
    /// The colleague, and their password. A demo of a permissions model needs
    /// two people in it or there is nothing to demonstrate.
    pub colleague: String,
    pub invoices: usize,
    /// Invoices cancelled by a credit note. Part of `invoices`, not extra.
    pub credited: usize,
    pub payments: usize,
    /// Supplier bills — the input-tax side of the return.
    pub bills: usize,
    /// VAT returns filed. A tax module nobody has filed with shows a
    /// calculation rather than a business.
    pub filed: usize,
    pub journal_entries: usize,
    /// Customers on the record, which is what the invoices below are *for*.
    pub customers: usize,
    /// ZATCA documents built and waiting to be cleared or reported. Every
    /// invoice and every credit note is one.
    pub zatca_documents: usize,
    /// Stylists, chairs and rooms on the rota.
    pub bookables: usize,
    /// Appointments in the diary, across every stage a screen has to draw.
    pub reservations: usize,
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
    Access(#[from] erp_control::AccessError),
    #[error(transparent)]
    Pool(#[from] erp_control::PoolError),
    #[error(transparent)]
    Projection(#[from] erp_projection::RunError),
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
            erp_control::Actor::system(),
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
/// `ttl` is how long the tenant lives before the reaper destroys it. `None`
/// makes it an ordinary tenant that nothing will ever clean up — right for a
/// test that drops its own database, wrong for anything reachable from outside.
pub async fn seed(
    state: &AppState,
    slug: &str,
    password: &str,
    ttl: Option<std::time::Duration>,
) -> Result<Seeded, DemoError> {
    let app = router(state.clone());

    let signed_up = sign_up(state, &app, slug, password).await?;
    let tenant = signed_up.tenant;
    let token = signed_up.token.clone();

    // **Before any invoice.** A document issued before the business is
    // registered with ZATCA has no place in the hash chain and cannot be
    // cleared retrospectively — so a demo that registers afterwards is a demo
    // of a business that lost its first quarter.
    register_with_zatca(&app, slug, &token).await?;

    // **Before the invoices**, because an invoice is issued to somebody. The
    // document still freezes what it printed (L5); this is the record beside it.
    let customers = seed_customers(&app, slug, &token).await?;

    install_chart(&app, slug, &token).await?;
    let journal_entries = seed_opening_balances(&app, slug, &token).await?;
    let invoices = seed_invoices(&app, slug, &token).await?;
    let payments = seed_payments(&app, slug, &token).await?;

    // A mistake and its correction, because a demo of an accounting system in
    // which nothing was ever *wrong* is not a demo of an accounting system —
    // and because these are the paths a prospective customer asks about first.
    let credited = seed_corrections(&app, slug, &token).await?;
    let invoices = invoices + credited;

    // The other side of a VAT return. Without bills a demo shows output tax and
    // calls it a return, which is half a number and the wrong half to show
    // somebody deciding whether this can file for them.
    let bills = seed_bills(&app, slug, &token).await?;

    // Drive the projections before filing: a return is computed from the read
    // models, and filing one that has not caught up would record zeroes.
    project(&state.control, tenant).await?;
    let filed = seed_filing(&app, slug, &token).await?;

    // The diary. After the customers, because a booking is made *by* somebody
    // and the reference is what stops two spellings being two people — and
    // **before the last projection run**, or the rota and the day are in the
    // log with nothing to read them.
    let (bookables, reservations) = seed_diary(&app, slug, &token).await?;

    let colleague = seed_colleague(&app, slug, &token, password).await?;

    // Drive the projections, so the demo has something to show the moment it
    // finishes rather than whenever a worker next visits. A deployment with a
    // worker running would get there on its own; this makes the demo usable
    // without one.
    project(&state.control, tenant).await?;

    // Last, so a demo that failed half-way through building is not one the
    // reaper quietly tidies away before anyone sees why it failed.
    if let Some(ttl) = ttl {
        state
            .control
            .set_demo_expiry(tenant, ttl, erp_control::Actor::system())
            .await?;
    }

    let database = state
        .control
        .tenant(tenant)
        .await?
        .ok_or_else(|| DemoError::Unexpected {
            path: "/v1/signups".to_owned(),
            body: format!("tenant {tenant} vanished between signing up and reading it back"),
        })?
        .database_name;

    // After the last projection run, so it counts what is actually there.
    let zatca_documents = count_zatca_documents(&app, slug, &token).await?;

    Ok(Seeded {
        tenant,
        slug: slug.to_owned(),
        database,
        email: signed_up.email,
        token,
        expires_after: ttl,
        colleague,
        invoices,
        credited,
        payments,
        bills,
        filed,
        journal_entries,
        customers,
        zatca_documents,
        bookables,
        reservations,
    })
}

/// Runs every projection group to the head of the log.
pub async fn project(control: &Arc<ControlPlane>, tenant: TenantId) -> Result<(), DemoError> {
    let db = control.enter_for_maintenance(tenant).await?;
    advance::<booking::Booking>(&db, &booking::projections(), booking::upcasters()).await?;
    advance::<crm::Crm>(&db, &crm::projections(), crm::upcasters()).await?;
    advance::<ledger::Ledger>(&db, &ledger::projections(), ledger::upcasters()).await?;
    advance::<sales::Sales>(&db, &sales::projections(), sales::upcasters()).await?;
    advance::<purchases::Purchases>(&db, &purchases::projections(), purchases::upcasters()).await?;
    advance::<tax_sa::TaxSa>(&db, &tax_sa::projections(), tax_sa::upcasters()).await?;
    Ok(())
}

async fn advance<G: erp_projection::ProjectionGroup>(
    db: &TenantDb,
    projections: &[Arc<dyn erp_projection::Projection<Group = G>>],
    upcasters: &erp_eventlog::Upcasters,
) -> Result<(), DemoError> {
    let refs: Vec<&dyn erp_projection::Projection<Group = G>> =
        projections.iter().map(AsRef::as_ref).collect();

    loop {
        let mut tx = db.begin().await?;
        match erp_projection::run_once_in::<G>(&mut tx, &refs, upcasters, 500).await {
            Ok(erp_projection::Progress::Advanced { .. }) => {
                tx.commit().await.map_err(erp_projection::RunError::from)?;
            }
            Ok(_) => {
                tx.rollback()
                    .await
                    .map_err(erp_projection::RunError::from)?;
                return Ok(());
            }
            Err(e) => {
                tx.rollback()
                    .await
                    .map_err(erp_projection::RunError::from)?;
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

/// Signs up, opens the confirmation, and comes back logged in.
///
/// # The one thing here that is not a customer doing customer things
///
/// Signup takes two calls with an email in between, and the token is only ever
/// in that email — deliberately, because a token the API handed back would let
/// anybody confirm their own signup and the whole endpoint would be back where
/// it started (see `erp-control/src/signup.rs`).
///
/// So something has to open the mailbox. [`confirmation_link`] is that
/// something: one `SELECT` against the control plane's outbox, reading the
/// message a person would have read. It is the only place in this crate that
/// touches a database, and it is here rather than as a back door in the product
/// because a back door built for a seeder is a back door.
///
/// Both HTTP calls are still made, in order, exactly as a customer makes them.
async fn sign_up(
    state: &AppState,
    app: &axum::Router,
    slug: &str,
    password: &str,
) -> Result<SignedUp, DemoError> {
    let email = format!("owner@{slug}.example");
    post(
        app,
        slug,
        "/v1/signups",
        None,
        &serde_json::json!({
            "slug": slug,
            "company": "Rawabi Consulting · روابي للاستشارات",
            "email": email,
            "password": password,
            "modules": modules(),
        }),
        StatusCode::ACCEPTED,
    )
    .await?;

    let token = confirmation_link(state, &email).await?;
    let body = post(
        app,
        slug,
        &format!("/v1/signups/{token}"),
        None,
        &serde_json::json!({}),
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

/// Two customers: one a company that can take a standard invoice, one a walk-in.
///
/// The pair matters. `zatca::Kind::of` reads the buyer's VAT number to decide
/// whether a document is cleared before it is handed over or reported within the
/// day, so a demo with only one kind of customer demonstrates only one of the
/// two obligations.
async fn seed_customers(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    let customers = [
        // The two that appear on more than one invoice. Referencing them is
        // what makes receivables show one row each instead of one per spelling.
        serde_json::json!({
            "id": "CUST-0001",
            "name": "مجموعة الفيصلية",
            "name_latin": "Al Faisaliah Group",
            "kind": "company",
            "phone": "+966500000001",
            "email": "ap@faisaliah.example",
            "vat_number": {
                "vat_number": "310122393500003",
                "scheme": "CRN",
                "identifier": "1010101010"
            },
            "address": {
                "street": "طريق الملك فهد",
                "building": "2322",
                "district": "العليا",
                "city": "الرياض",
                "postal_code": "12211",
                "country": "SA"
            },
            "registered_on": "2026-01-05T00:00:00Z"
        }),
        serde_json::json!({
            "id": "CUST-0002",
            "name": "نجد للخدمات اللوجستية",
            "name_latin": "Najd Logistics",
            "kind": "company",
            "phone": "+966500000002",
            "vat_number": {
                "vat_number": "311234567800003",
                "scheme": "CRN",
                "identifier": "2020202020"
            },
            "registered_on": "2026-01-12T00:00:00Z"
        }),
        // A company with no Saudi VAT registration, which is why its invoice is
        // zero-rated and simplified. `kind` is still `company`; not every
        // company is registered here.
        serde_json::json!({
            "id": "CUST-0003",
            "name": "Gulf Freight DMCC",
            "kind": "company",
            "email": "accounts@gulffreight.example",
            "registered_on": "2026-02-01T00:00:00Z"
        }),
    ];

    for customer in &customers {
        post(
            app,
            slug,
            "/v1/crm/customers",
            Some(token),
            customer,
            StatusCode::CREATED,
        )
        .await?;
    }
    Ok(customers.len())
}

/// A rota and a day's diary.
///
/// # Why the demo books a salon and a hotel at once
///
/// Because the claim this module makes is that they are the same code. A
/// stylist at capacity one and a room type at capacity three, in one tenant,
/// with one of the rooms assigned and the rest still a pool, is that claim
/// shown rather than asserted. If it ever needs a branch in `booking` to hold
/// both, this is where it stops working.
///
/// Every stage a screen has to draw is in here for the reason the invoices
/// are: a diary in which everything is `reserved` demonstrates one cell of a
/// calendar and nothing about the day.
async fn seed_diary(
    app: &axum::Router,
    slug: &str,
    token: &str,
) -> Result<(usize, usize), DemoError> {
    let bookables = [
        serde_json::json!({
            "id": "noura", "name": "نورة", "name_latin": "Noura",
            "kind": "person", "capacity": 1
        }),
        serde_json::json!({
            "id": "hind", "name": "هند", "name_latin": "Hind",
            "kind": "person", "capacity": 1
        }),
        serde_json::json!({
            "id": "chair-1", "name": "كرسي ١", "kind": "place", "capacity": 1
        }),
        serde_json::json!({
            "id": "chair-2", "name": "كرسي ٢", "kind": "place", "capacity": 1
        }),
        // A pool: three of them, booked by the type. `room-201` below is one of
        // the units, and it is what "assign the unit later" gives out.
        serde_json::json!({
            "id": "suite", "name": "جناح", "name_latin": "Suite",
            "kind": "place", "capacity": 3
        }),
        serde_json::json!({
            "id": "room-201", "name": "٢٠١", "kind": "place", "capacity": 1
        }),
    ];
    for bookable in &bookables {
        post(
            app,
            slug,
            "/v1/booking/resources",
            Some(token),
            bookable,
            StatusCode::CREATED,
        )
        .await?;
    }

    // Sunday to Thursday, nine to nine. The Saudi working week, and the reason
    // the rule names weekdays at all: Friday and Saturday are the weekend here,
    // so a rota that ran Monday to Friday would be somebody else's calendar.
    for person in ["noura", "hind"] {
        put(
            app,
            slug,
            &format!("/v1/booking/resources/{person}/availability"),
            token,
            &serde_json::json!({
                "hours": [{ "weekdays": [7, 1, 2, 3, 4], "opens_at": 540, "closes_at": 1260 }]
            }),
            StatusCode::OK,
        )
        .await?;
    }

    let reservations = seed_appointments(app, slug, token).await?;
    Ok((bookables.len(), reservations))
}

/// A Wednesday, as it reads at two in the afternoon.
///
/// Split from the rota above only because it was one function of a hundred and
/// sixty lines. The two halves are a setup and a day.
async fn seed_appointments(
    app: &axum::Router,
    slug: &str,
    token: &str,
) -> Result<usize, DemoError> {
    let appointments = [
        (
            "BK-0001",
            "CUST-0001",
            "قص وتصفيف",
            "07:00",
            "08:00",
            "noura",
            "chair-1",
        ),
        (
            "BK-0002",
            "CUST-0002",
            "صبغة",
            "08:00",
            "10:00",
            "hind",
            "chair-2",
        ),
        (
            "BK-0003",
            "CUST-0001",
            "قص",
            "10:00",
            "10:45",
            "noura",
            "chair-1",
        ),
        (
            "BK-0004",
            "CUST-0003",
            "حلاقة",
            "11:00",
            "11:30",
            "hind",
            "chair-2",
        ),
        (
            "BK-0005",
            "CUST-0002",
            "علاج بالكيراتين",
            "12:00",
            "14:00",
            "noura",
            "chair-1",
        ),
    ];
    for (id, customer, what, from, until, who, chair) in appointments {
        post(
            app,
            slug,
            "/v1/booking/reservations",
            Some(token),
            &serde_json::json!({
                "id": id,
                "customer": customer,
                "customer_name": customer_name(customer),
                "lines": [{
                    "what": what,
                    "from": format!("2026-04-15T{from}:00Z"),
                    "until": format!("2026-04-15T{until}:00Z"),
                    "takes": [{ "resource": who }, { "resource": chair }]
                }]
            }),
            StatusCode::CREATED,
        )
        .await?;
    }

    // The day as it actually reads at two in the afternoon: one finished, one
    // in the chair, one waiting, one that never came, one still to come.
    for (id, stage, why) in [
        ("BK-0001", "completed", ""),
        ("BK-0002", "in_service", ""),
        ("BK-0003", "arrived", ""),
        ("BK-0004", "no_show", "لم تحضر"),
    ] {
        post(
            app,
            slug,
            &format!("/v1/booking/reservations/{id}/stage"),
            Some(token),
            &serde_json::json!({ "stage": stage, "why": why }),
            StatusCode::OK,
        )
        .await?;
    }

    seed_stay(app, slug, token).await?;
    Ok(appointments.len() + 1)
}

/// The hotel half: a suite booked by the type, and one unit given out.
///
/// In the same tenant as the salon above, on purpose. The claim this module
/// makes is that a stylist at capacity one and a room type at capacity three
/// are the same code, and one tenant holding both is that claim shown rather
/// than asserted.
async fn seed_stay(app: &axum::Router, slug: &str, token: &str) -> Result<(), DemoError> {
    post(
        app,
        slug,
        "/v1/booking/reservations",
        Some(token),
        &serde_json::json!({
            "id": "BK-0006",
            "customer": "CUST-0003",
            "customer_name": customer_name("CUST-0003"),
            "lines": [{
                "what": "إقامة ليلتين",
                "from": "2026-04-15T12:00:00Z",
                "until": "2026-04-17T08:00:00Z",
                "takes": [{ "resource": "suite" }]
            }]
        }),
        StatusCode::CREATED,
    )
    .await?;
    put(
        app,
        slug,
        "/v1/booking/reservations/BK-0006/lines/0/unit",
        token,
        &serde_json::json!({ "unit": "room-201" }),
        StatusCode::OK,
    )
    .await?;
    Ok(())
}

/// What the diary prints beside a booking.
///
/// The frozen copy, and it has to be: `proj_booking` may not read `proj_crm`
/// (L3). Written out here rather than read back over HTTP because the demo is
/// what a client would send, and a client has the name in the form already.
fn customer_name(customer: &str) -> &'static str {
    match customer {
        "CUST-0001" => "مجموعة الفيصلية",
        "CUST-0002" => "نجد للخدمات اللوجستية",
        _ => "Gulf Freight DMCC",
    }
}

async fn install_chart(app: &axum::Router, slug: &str, token: &str) -> Result<(), DemoError> {
    post(
        app,
        slug,
        "/v1/ledger/chart",
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
            slug,
            "/v1/ledger/entries",
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

/// The first quarter, filed.
///
/// A demo of a tax module in which nothing has been declared shows an
/// arithmetic exercise. This is the thing a prospective customer is actually
/// buying.
/// **The Saudi registration**, which every document this tenant issues carries.
///
/// Real-looking and not real: the VAT number satisfies ZATCA's shape — fifteen
/// digits from three to three — because the API refuses anything else, and the
/// whole point of refusing it here is that ZATCA would.
async fn register_with_zatca(app: &axum::Router, slug: &str, token: &str) -> Result<(), DemoError> {
    put(
        app,
        slug,
        "/v1/tax_sa/registration",
        token,
        &serde_json::json!({
            "vat_number": "310122393500003",
            "name": "روابي للاستشارات",
            "name_latin": "Rawabi Consulting",
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
        }),
        StatusCode::OK,
    )
    .await?;
    Ok(())
}

/// How many ZATCA documents were built, read back through the API.
async fn count_zatca_documents(
    app: &axum::Router,
    slug: &str,
    token: &str,
) -> Result<usize, DemoError> {
    let body = get(app, slug, "/v1/tax_sa/zatca", token).await?;
    Ok(usize::try_from(body["chain_length"].as_i64().unwrap_or_default()).unwrap_or_default())
}

async fn seed_filing(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    post(
        app,
        slug,
        "/v1/tax_sa/returns",
        Some(token),
        &serde_json::json!({
            "from": "2026-01-01T00:00:00Z",
            "until": "2026-04-01T00:00:00Z",
            "currency": "SAR",
            "filed_on": "2026-04-28T00:00:00Z"
        }),
        StatusCode::CREATED,
    )
    .await?;
    Ok(1)
}

/// Four supplier bills across two suppliers, so the return has an input side.
///
/// One of them is exempt — residential rent — because the difference between
/// zero-rated and exempt is invisible until a return has both, and it is the
/// distinction that costs money to get wrong.
async fn seed_bills(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    let bills = [
        (
            "ap-2201",
            "Najd Logistics Services",
            "311234567800003",
            "NL-77120",
            "2026-01-12T00:00:00Z",
            serde_json::json!([
                { "description": "Freight, January", "account": "5000",
                  "net": 820_000, "vat": "standard", "vat_rate": 1500, "tax": 123_000 },
            ]),
        ),
        (
            "ap-2214",
            "Al Khobar Properties",
            "310999888700003",
            "AKP-2026-02",
            "2026-02-01T00:00:00Z",
            // Residential rent is exempt. No tax, and none to reclaim.
            serde_json::json!([
                { "description": "Office rent, February", "account": "5100",
                  "net": 1_500_000, "vat": "exempt", "vat_rate": 0, "tax": 0 },
            ]),
        ),
        (
            "ap-2230",
            "Najd Logistics Services",
            "311234567800003",
            "NL-77455",
            "2026-02-19T00:00:00Z",
            serde_json::json!([
                { "description": "Freight, February", "account": "5000",
                  "net": 640_000, "vat": "standard", "vat_rate": 1500, "tax": 96_000 },
                { "description": "Customs handling", "account": "5200",
                  "net": 180_000, "vat": "standard", "vat_rate": 1500, "tax": 27_000 },
            ]),
        ),
        (
            "ap-2248",
            "Al Khobar Properties",
            "310999888700003",
            "AKP-2026-03",
            "2026-03-01T00:00:00Z",
            serde_json::json!([
                { "description": "Office rent, March", "account": "5100",
                  "net": 1_500_000, "vat": "exempt", "vat_rate": 0, "tax": 0 },
            ]),
        ),
    ];

    for (id, supplier, vat_number, reference, billed_on, lines) in &bills {
        post(
            app,
            slug,
            "/v1/purchases/bills",
            Some(token),
            &serde_json::json!({
                "id": id,
                "supplier": { "name": supplier, "vat_number": vat_number },
                "reference": reference,
                "billed_on": billed_on,
                "currency": "SAR",
                "lines": lines,
            }),
            StatusCode::CREATED,
        )
        .await?;
    }

    // One of them settled, so a payables list has something in every state.
    post(
        app,
        slug,
        "/v1/purchases/bills/ap-2201/payments",
        Some(token),
        &serde_json::json!({
            "reference": "TRF-90218",
            "amount": { "minor": 943_000, "currency": "SAR" },
            "paid_on": "2026-02-11T00:00:00Z",
            "account": "1010"
        }),
        StatusCode::OK,
    )
    .await?;

    Ok(bills.len())
}

/// The `crm` record each invoice is issued to.
///
/// The demo's whole point here: two invoices to Al Faisaliah name the same
/// record, so receivables shows one row for them rather than one per spelling.
fn reference(customer: &str) -> &'static str {
    match customer {
        "Najd Logistics" => "CUST-0002",
        "Gulf Freight DMCC" => "CUST-0003",
        _ => "CUST-0001",
    }
}

/// Five invoices across three customers, spanning every VAT treatment.
///
/// The mix is the point: a demo with one 15% invoice shows nothing about the
/// tax breakdown a Saudi invoice has to print.
///
/// The ids read like a CRM's references on purpose. `id` is the *client's* key
/// — what makes a retry safe — and the invoice number is allocated here from a
/// gapless statutory series. Seeding them as `INV-2026-001` would put two things
/// that look like invoice numbers side by side and teach that they are the same
/// kind of thing.
async fn seed_invoices(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    let invoices = [
        (
            "crm-4471",
            "2026-01-08T00:00:00Z",
            "Al Faisaliah Group",
            Some("310122393500003"),
            serde_json::json!([
                { "description": "Systems consulting — January", "net": 4_500_000, "vat": "standard" },
            ]),
        ),
        (
            "crm-4489",
            "2026-01-22T00:00:00Z",
            "Najd Logistics",
            Some("311234567800003"),
            serde_json::json!([
                { "description": "Integration work", "net": 2_800_000, "vat": "standard" },
                { "description": "Support retainer", "net": 600_000, "vat": "standard" },
            ]),
        ),
        (
            "crm-4502",
            "2026-02-05T00:00:00Z",
            "Gulf Freight DMCC",
            None,
            serde_json::json!([
                { "description": "Consulting for an overseas client", "net": 3_300_000, "vat": "zero" },
            ]),
        ),
        (
            "crm-4517",
            "2026-02-19T00:00:00Z",
            "Al Faisaliah Group",
            Some("310122393500003"),
            serde_json::json!([
                { "description": "Systems consulting — February", "net": 4_500_000, "vat": "standard" },
                { "description": "Staff accommodation recharge", "net": 900_000, "vat": "exempt" },
            ]),
        ),
        (
            "crm-4530",
            "2026-03-04T00:00:00Z",
            "Najd Logistics",
            Some("311234567800003"),
            serde_json::json!([
                { "description": "Change requests", "net": 1_150_000, "vat": "standard" },
            ]),
        ),
    ];

    // **One invoice carries a document-level discount**, which is what a
    // settlement discount actually is: a figure the customer can see, with the
    // tax coming off it. It used to be a negative line here, which showed a
    // smaller total and never said why.
    let discounts = |id: &str| match id {
        "crm-4517" => serde_json::json!([
            { "reason": "Early settlement discount", "amount": 150_000, "vat": "standard" },
        ]),
        _ => serde_json::json!([]),
    };

    for (id, issued_on, customer, vat_number, lines) in &invoices {
        post(
            app,
            slug,
            "/v1/sales/invoices",
            Some(token),
            &serde_json::json!({
                "id": id,
                "customer": {
                    // **The reference and the copy, both.** The record is what
                    // groups a customer's debt; the name and address below are
                    // what the document prints and what ZATCA clears.
                    "id": reference(customer),
                    "name": customer,
                    "vat_number": vat_number,
                    // ZATCA wants a buyer address on a standard invoice, and
                    // warns without one.
                    "address": vat_number.map(|_| serde_json::json!({
                        "street": "طريق الملك فهد",
                        "city": "الرياض",
                        "country": "SA",
                        "district": "العليا",
                        "postal_code": "12211"
                    })),
                },
                "issued_on": issued_on,
                "due_on": "2026-04-01T00:00:00Z",
                "currency": "SAR",
                "lines": lines,
                "discounts": discounts(id),
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
        ("crm-4471", "SNB-88401", 5_175_000, "2026-02-01T00:00:00Z"),
        // (28,000.00 + 6,000.00) + 15% = 39,100.00
        ("crm-4489", "SNB-88907", 3_910_000, "2026-02-15T00:00:00Z"),
        // Zero-rated: 33,000.00, and only half of it has arrived.
        ("crm-4502", "RJHI-1204", 1_650_000, "2026-03-01T00:00:00Z"),
    ];

    for (invoice, reference, minor, received_on) in &payments {
        post(
            app,
            slug,
            &format!("/v1/sales/invoices/{invoice}/payments"),
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

/// One credited invoice and one reversed journal entry.
///
/// Both corrections, and both leaving their originals on the books — which is
/// the thing about this system that is hardest to believe from a description
/// and obvious from a screen.
async fn seed_corrections(app: &axum::Router, slug: &str, token: &str) -> Result<usize, DemoError> {
    // An invoice raised against the wrong customer, and put right.
    post(
        app,
        slug,
        "/v1/sales/invoices",
        Some(token),
        &serde_json::json!({
            "id": "crm-4544",
            "customer": {
                "id": "CUST-0002",
                "name": "Najd Logistics",
                "vat_number": "311234567800003"
            },
            "issued_on": "2026-03-11T00:00:00Z",
            "due_on": "2026-04-10T00:00:00Z",
            "currency": "SAR",
            "lines": [
                { "description": "Raised against the wrong customer", "net": 750_000,
                  "vat": "standard" }
            ],
        }),
        StatusCode::CREATED,
    )
    .await?;

    post(
        app,
        slug,
        "/v1/sales/invoices/crm-4544/credit-note",
        Some(token),
        &serde_json::json!({
            "id": "crm-4544-void",
            "reason": "Raised against the wrong customer",
            "on": "2026-03-12T00:00:00Z",
        }),
        StatusCode::OK,
    )
    .await?;

    // And a journal entry posted for the wrong amount, reversed.
    post(
        app,
        slug,
        "/v1/ledger/entries",
        Some(token),
        &serde_json::json!({
            "id": "UTILITIES-2026-02",
            "occurred_on": "2026-02-28T00:00:00Z",
            "memo": "February utilities — wrong amount",
            "lines": [
                { "account": "5200", "amount": { "minor": 480_000, "currency": "SAR" } },
                { "account": "1010", "amount": { "minor": -480_000, "currency": "SAR" } },
            ],
        }),
        StatusCode::OK,
    )
    .await?;

    post(
        app,
        slug,
        "/v1/ledger/entries/UTILITIES-2026-02/reversal",
        Some(token),
        &serde_json::json!({
            "id": "UTILITIES-2026-02-R",
            "occurred_on": "2026-03-02T00:00:00Z",
            "memo": "Reversing the February utilities entry",
        }),
        StatusCode::OK,
    )
    .await?;

    Ok(1)
}

/// A second person, who does the invoicing and not the books.
///
/// Returns their login. Per-module roles are invisible with one user in the
/// tenant, and "Sara can raise invoices but cannot touch the chart of accounts"
/// is the whole point of them.
async fn seed_colleague(
    app: &axum::Router,
    slug: &str,
    token: &str,
    password: &str,
) -> Result<String, DemoError> {
    let handle = format!("sara@{slug}.example");

    let added = post(
        app,
        slug,
        "/v1/members",
        Some(token),
        &serde_json::json!({ "email": handle, "password": password, "role": "viewer" }),
        StatusCode::CREATED,
    )
    .await?;

    let identity = added["identity"]
        .as_str()
        .ok_or_else(|| DemoError::Unexpected {
            path: "/v1/members".to_owned(),
            body: added.to_string(),
        })?;

    put(
        app,
        slug,
        &format!("/v1/members/{identity}/modules/sales"),
        token,
        &serde_json::json!({ "role": "accountant" }),
        StatusCode::NO_CONTENT,
    )
    .await?;

    Ok(handle)
}

// ---------------------------------------------------------------------------

/// A `PUT`, for the handful of things that are settings rather than events.
async fn put(
    app: &axum::Router,
    slug: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
    expected: StatusCode,
) -> Result<serde_json::Value, DemoError> {
    let request = Request::put(path)
        .header(header::HOST, format!("{slug}.localhost"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .map_err(|e| DemoError::Unexpected {
            path: path.to_owned(),
            body: e.to_string(),
        })?;

    send(app, "PUT", path, request, expected).await
}

/// The confirmation token, read out of the mailbox.
///
/// The email is an outbox effect in the control plane, so this is what a mail
/// client would have been handed. It looks for the link this build writes and
/// takes what follows it, which is the token and nothing else.
///
/// Fails loudly when there is no message or no link in it. A seeder that
/// shrugged here would carry on and fail somewhere unrelated, which is the
/// failure mode `send` exists to refuse (L6).
async fn confirmation_link(state: &AppState, email: &str) -> Result<String, DemoError> {
    const MARKER: &str = "/v1/signups/";

    let body: Option<String> = sqlx::query_scalar(
        "SELECT payload ->> 'body' FROM outbox
          WHERE kind = 'email.send' AND payload ->> 'to' = $1
          ORDER BY id DESC LIMIT 1",
    )
    .bind(email)
    .fetch_optional(state.control.pool())
    .await
    .map_err(|e| DemoError::Unexpected {
        path: "outbox".to_owned(),
        body: e.to_string(),
    })?;

    let body = body.ok_or_else(|| DemoError::Unexpected {
        path: "outbox".to_owned(),
        body: format!("no confirmation email was promised to {email}"),
    })?;

    body.split_once(MARKER)
        .map(|(_, rest)| {
            rest.split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .filter(|token| !token.is_empty())
        .ok_or_else(|| DemoError::Unexpected {
            path: "outbox".to_owned(),
            body: format!("the confirmation email to {email} carries no {MARKER} link"),
        })
}

async fn post(
    app: &axum::Router,
    // The tenant this is addressed to. **The subdomain is the tenant now**, so
    // the demo has to address one the way a browser does — which is also what
    // makes it a test of that, rather than of a path it constructed.
    slug: &str,
    path: &str,
    token: Option<&str>,
    body: &serde_json::Value,
    expected: StatusCode,
) -> Result<serde_json::Value, DemoError> {
    let mut request = Request::post(path)
        .header(header::HOST, format!("{slug}.localhost"))
        .header(header::CONTENT_TYPE, "application/json");
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
    slug: &str,
    path: &str,
    token: &str,
) -> Result<serde_json::Value, DemoError> {
    let request = Request::get(path)
        .header(header::HOST, format!("{slug}.localhost"))
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
