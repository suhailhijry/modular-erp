//! The Saudi tax module's HTTP surface.
//!
//! Translation only, like every module's — see [`ledger::http`] for why these
//! live in the module rather than in the composition root.
//!
//! What is different here is where the *domain* went: the return used to be
//! netted in the composition root, and it is computed in [`crate::vat_return`]
//! now. This file asks which modules the tenant has, calls the module, and
//! renders.

use crate::{Sides, TaxError};
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::{CurrencyCode, Timestamp};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Language, ManageAccounts, ManageTenant, Public, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, bad_request, metadata, require_module};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(vat_return))
        .routes(routes!(filed_returns, file_return))
        .routes(routes!(registration, register))
        .routes(routes!(zatca_standing))
        .routes(routes!(zatca_documents))
        .routes(routes!(zatca_document))
        .routes(routes!(print_document))
        .routes(routes!(document_xml))
        .routes(routes!(document_link))
        .routes(routes!(public_print))
        .routes(routes!(onboarding_status, begin_onboarding))
        .routes(routes!(accept_certificate))
        .routes(routes!(activate))
}

/// How many filed returns a list gives back. A business files four a year.
const PAGE: i64 = 200;

/// **What this module's routes can answer with.**
///
/// Its own failures, the failures of the modules it is built on, and everything
/// any route can produce — the request-level messages, the control plane's and
/// the event log's, which [`erp_web::CATALOG`] already unions.
///
/// That list is exhaustive by construction: a route can only surface a message
/// from a crate this one depends on. Leaving one out is not a compile error and
/// not a test failure — it is a client receiving `ledger.does_not_balance` as
/// the bare code with no sentence in it, which is how this was found.
///
/// A module cannot name its siblings and has no reason to. The complete catalog
/// is `erp_api::CATALOG`, and `docs/ERRORS.md` comes from that.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &sales::CATALOG,
    &purchases::CATALOG,
    &ledger::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct BandView {
    vat: &'static str,
    vat_rate: i32,
    net: i64,
    tax: i64,
    /// Documents with a tax point in this period. Invoices and credit notes on
    /// the output side; bills on the input side.
    documents: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct SideView {
    bands: Vec<BandView>,
    net: i64,
    tax: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReturnView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    currency: String,
    /// What was charged on sales, net of credit notes with a tax point in this
    /// period. Empty when the tenant has no sales module.
    output: SideView,
    /// What was paid on purchases, and the reclaimable part of it. Empty when
    /// the tenant has no purchases module.
    input: SideView,
    /// **The number that gets paid, or reclaimed.** Output tax less input tax;
    /// negative means ZATCA owes the business.
    payable: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Period {
    /// Inclusive.
    /// The first day of the period, on the tenant's calendar.
    #[schema(value_type = String, example = "2026-01-01")]
    from: chrono::NaiveDate,
    /// **Exclusive**, so consecutive returns neither overlap nor leave a day out.
    /// The first day **after** the period: consecutive returns neither
    /// overlap nor leave a day out.
    #[schema(value_type = String, example = "2026-04-01")]
    until: chrono::NaiveDate,
    currency: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "from": "2026-01-01",
    "until": "2026-04-01",
    "currency": "SAR",
    "filed_on": "2026-04-28T00:00:00Z"
}))]
struct NewFiling {
    /// The first day of the period, on the tenant's calendar.
    #[schema(value_type = String, example = "2026-01-01")]
    from: chrono::NaiveDate,
    /// **Exclusive.**
    /// The first day **after** the period: consecutive returns neither
    /// overlap nor leave a day out.
    #[schema(value_type = String, example = "2026-04-01")]
    until: chrono::NaiveDate,
    currency: String,
    /// The date the business treats the filing as made. Not a clock reading, for
    /// the same reason a tax point is not one.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    filed_on: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct FiledView {
    /// The period, as it identifies itself: `SAR.2026-01-01.2026-04-01`.
    period: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    output_tax: i64,
    input_tax: i64,
    payable: i64,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    filed_on: Timestamp,
    /// ZATCA's acknowledgement, once clearance exists to produce one.
    reference: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The VAT return for a period: what was charged, what was paid, the difference.
///
/// Each document is reported in the period of **its own tax point** — an invoice
/// on its issue date, a credit note on its credit date, a bill on the date the
/// supplier stated. Re-running a filed period gives the number that was filed.
///
/// A tenant with only one of sales and purchases gets zeroes for the other side
/// rather than a 404: a business that has not enabled purchases genuinely
/// reclaimed nothing, and that is a return they can file.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/vat-return",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = String, Query, description = "First day of the period, `YYYY-MM-DD`, on the tenant's calendar."),
        ("until" = String, Query, description = "First day **after** the period, `YYYY-MM-DD`."),
        ("currency" = String, Query, description = "ISO 4217."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = ReturnView),
        (status = BAD_REQUEST, description = "An unknown currency, or a period that ends before it starts", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the tax_sa module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn vat_return(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(period): Query<Period>,
) -> Result<Json<ReturnView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (currency, from, until) = period_of(&period.currency, period.from, period.until, locale)?;

    let sides = sides_of(&tenant);
    // Waited on per side, so a tenant with one module is not made to wait for a
    // projection that will never run.
    if sides.sells {
        consistency
            .wait_for(&tenant.db, sales::GROUP_NAME, locale)
            .await?;
    }
    if sides.buys {
        consistency
            .wait_for(&tenant.db, purchases::GROUP_NAME, locale)
            .await?;
    }

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let declared = crate::vat_return(&mut conn, sides, currency, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(view(&declared)))
}

/// Record that a period was filed, with the numbers that went.
///
/// Filing the same period twice is a **conflict**, not a no-op: a second filing
/// is an amendment, which is a different document with its own rules.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/returns",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = NewFiling,
    responses(
        (status = CREATED, description = "Filed. The numbers are recorded as they stood.", body = FiledView),
        (status = BAD_REQUEST, description = "An unknown currency, or a period that ends before it starts", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "The period was already filed — correcting it is an amendment", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn file_return(
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewFiling>,
) -> Result<(StatusCode, Json<FiledView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (currency, from, until) = period_of(&body.currency, body.from, body.until, locale)?;

    let filed = crate::file_return(
        &tenant.db,
        sides_of(&tenant),
        currency,
        from,
        until,
        body.filed_on,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| tax_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    let period = crate::period_id(currency, from, until)
        .map(|id| id.as_str().to_owned())
        .unwrap_or_default();

    Ok((
        StatusCode::CREATED,
        Json(FiledView {
            period,
            from: filed.from,
            until: filed.until,
            output_tax: 0,
            input_tax: 0,
            payable: filed.payable.minor(),
            filed_on: filed.filed_on,
            reference: None,
        }),
    ))
}

/// Every return this business has filed.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/returns",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<FiledView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn filed_returns(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<FiledView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let returns = crate::filed(&mut conn, PAGE)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(
        returns
            .into_iter()
            .map(|r| FiledView {
                period: r.period,
                from: r.from,
                until: r.until,
                output_tax: r.output_tax.minor(),
                input_tax: r.input_tax.minor(),
                payable: r.payable.minor(),
                filed_on: r.filed_on,
                reference: r.reference,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------

/// Which sides of the return this tenant contributes to.
fn sides_of<C: erp_web::Capability>(tenant: &Allowed<C>) -> Sides {
    Sides {
        sells: tenant.db.has_module(&sales::module_id()),
        buys: tenant.db.has_module(&purchases::module_id()),
    }
}

fn period_of(
    currency: &str,
    from: chrono::NaiveDate,
    until: chrono::NaiveDate,
    locale: Locale,
) -> Result<(CurrencyCode, chrono::NaiveDate, chrono::NaiveDate), Problem> {
    let parsed = CurrencyCode::new(currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            currency,
            locale,
        )
    })?;
    if until <= from {
        return Err(bad_request(
            erp_web::messages::EMPTY_PERIOD,
            "period",
            &from.to_string(),
            locale,
        ));
    }
    Ok((parsed, from, until))
}

fn view(declared: &crate::Return) -> ReturnView {
    let side = |s: &crate::Side| SideView {
        bands: s
            .bands
            .iter()
            .map(|b| BandView {
                vat: b.category.as_str(),
                vat_rate: b.basis_points,
                net: b.net.minor(),
                tax: b.tax.minor(),
                documents: b.documents,
            })
            .collect(),
        net: s.net.minor(),
        tax: s.tax.minor(),
    };

    ReturnView {
        from: declared.from,
        until: declared.until,
        currency: declared.currency.to_string(),
        output: side(&declared.output),
        input: side(&declared.input),
        payable: declared.payable.minor(),
    }
}

/// Maps a command failure onto a status.
fn tax_problem(error: &CommandError<TaxError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Already filed. Look at what is there and decide whether an
                // amendment is what you meant.
                TaxError::AlreadyFiled { .. } => StatusCode::CONFLICT,
                TaxError::Read(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        // **The one that must never be silent.** A different request reused an
        // identifier that is taken; a retry of the request that created it
        // never reaches here, because the kernel reports those as success.
        CommandError::Execute(ExecuteError::AlreadyExists { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::ALREADY_EXISTS),
        ),

        other => {
            tracing::error!(error = %other, "tax command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

// ---------------------------------------------------------------------------
// ZATCA
// ---------------------------------------------------------------------------

/// How many documents a list gives back. A busy shop issues a few hundred a day.
const DOCUMENTS: i64 = 200;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[schema(example = json!({
    "vat_number": "310122393500003",
    "name": "روابي للاستشارات",
    "name_latin": "Rawabi Consulting",
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
}))]
struct RegistrationBody {
    /// Fifteen digits, beginning and ending with `3`.
    vat_number: String,
    /// **The legal name, in Arabic** — it is what the invoice says, because the
    /// invoice is an Arabic document.
    name: String,
    /// The same name in Latin script, for screens. Never sent to ZATCA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name_latin: Option<String>,
    /// Which register `identifier` is from: `crn`, `mom`, `mls`, `sag`,
    /// `number700` or `other`.
    scheme: String,
    /// The number in that register — the commercial registration, usually.
    identifier: String,
    address: AddressBody,
    /// The business's industry — `Consulting`, `Retail`, `Beauty`. It goes in
    /// the ZATCA certificate request as the business category, so it is
    /// required: no industry, no certificate.
    industry: String,
    /// When the business treats the registration as effective. Not a clock
    /// reading, for the same reason a tax point is not one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    effective_from: Option<Timestamp>,
}

/// A Saudi national address, which is a shape rather than free text.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct AddressBody {
    street: String,
    /// Four digits.
    building: String,
    /// The four-digit secondary number, where the address has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additional: Option<String>,
    district: String,
    city: String,
    /// Five digits.
    postal_code: String,
    /// ISO 3166-1 alpha-2. `SA` for a business registered here.
    country: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct StandingView {
    /// Whether there is a registration at all. Nothing can be cleared or
    /// reported without one, so everything else here is moot until it is true.
    registered: bool,
    /// How many documents are in each state: `unregistered`, `pending`,
    /// `cleared`, `reported`, `refused`.
    counts: std::collections::BTreeMap<String, i64>,
    /// **Simplified invoices past their 24 hours and still not reported.** The
    /// number an inspection asks about.
    overdue: i64,
    /// **Standard invoices not yet cleared.** Not late — a standard invoice is
    /// cleared before issue — but documents the buyer must not have yet.
    awaiting_clearance: i64,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    oldest_pending: Option<Timestamp>,
    /// How many documents are in the chain.
    chain_length: i64,
    /// **Documents with no signature yet.** They can be neither submitted nor
    /// printed with a phase-two QR, so this is the number that says a business
    /// is not really live whatever else is true.
    unsigned: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct RemarkView {
    code: String,
    category: String,
    message: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct DocumentView {
    /// The statutory number, which is the document's identity.
    number: String,
    /// The invoice it was built from.
    source: String,
    /// `standard` — cleared before the buyer gets it — or `simplified`,
    /// reported within 24 hours.
    kind: &'static str,
    /// 388 invoice, 381 credit note, 383 debit note.
    type_code: i32,
    /// `unregistered`, `pending`, `cleared`, `reported` or `refused`.
    status: &'static str,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    issued_at: Timestamp,
    currency: String,
    net: i64,
    tax: i64,
    gross: i64,
    /// Where it sits in the hash chain. Absent on a document issued before the
    /// business registered, which has no place in it.
    icv: Option<i64>,
    previous_hash: Option<String>,
    invoice_hash: Option<String>,
    /// The base64 TLV block that goes on the printed document. Five fields
    /// before the document is signed, nine after — the last four are the stamp.
    qr: Option<String>,
    /// `ds:SignatureValue`, once it has been signed.
    signature: Option<String>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    signed_at: Option<Timestamp>,
    /// Warnings on an accepted document, errors on a refused one.
    remarks: Vec<RemarkView>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    settled_at: Option<Timestamp>,
    /// Whether it may be handed to the customer now: signed, and on a
    /// standard invoice cleared. `GET …/{number}/print` renders it.
    deliverable: bool,
}

/// One document, with the bytes.
#[derive(Debug, Serialize, ToSchema)]
struct FullDocumentView {
    #[serde(flatten)]
    document: DocumentView,
    /// The canonical UBL that was hashed, and what the signature covers.
    xml: Option<String>,
    /// **The document as submitted**: those bytes plus the signature, the QR
    /// and the `cac:Signature` that points at it.
    signed_xml: Option<String>,
    /// **The document ZATCA stamped, base64** — the one a buyer must be given.
    /// Cleared standard invoices only.
    stamped_xml: Option<String>,
}

fn document_view(stored: crate::Stored) -> DocumentView {
    let deliverable = crate::deliverable(&stored).is_ok();
    DocumentView {
        number: stored.number,
        source: stored.source,
        kind: stored.kind.as_str(),
        type_code: stored.type_code,
        status: stored.status.as_str(),
        issued_at: stored.issued_at,
        currency: stored.currency.to_string(),
        net: stored.net.minor(),
        tax: stored.tax.minor(),
        gross: stored.gross.minor(),
        icv: stored.icv,
        previous_hash: stored.previous_hash,
        invoice_hash: stored.invoice_hash,
        qr: stored.qr,
        signature: stored.signature,
        signed_at: stored.signed_at,
        remarks: stored
            .remarks
            .into_iter()
            .map(|r| RemarkView {
                code: r.code,
                category: r.category,
                message: r.message,
            })
            .collect(),
        deliverable,
        settled_at: stored.settled_at,
    }
}

/// Register the business with ZATCA, or correct what is registered.
///
/// Every document issued **after** this carries it. Nothing already issued
/// changes, which is the point: an invoice cleared in March was cleared under
/// the address that was current in March, and rewriting it would break the hash
/// ZATCA holds.
///
/// What ZATCA would refuse is refused here, because by the time ZATCA says so
/// the invoice exists — and a standard invoice cannot be given to the buyer
/// until it is cleared.
#[utoipa::path(
    put,
    path = "/v1/tax_sa/registration",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = RegistrationBody,
    responses(
        (status = OK, description = "Registered, from here on.", body = RegistrationBody),
        (status = BAD_REQUEST, description = "A VAT number that is not one, a name with no Arabic in it, or an address that is not a national address", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn register(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<RegistrationBody>,
) -> Result<Json<RegistrationBody>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let scheme = body
        .scheme
        .parse::<crate::taxpayer::IdScheme>()
        .map_err(|_| {
            bad_request(
                erp_web::messages::UNKNOWN_ID_SCHEME,
                "scheme",
                &body.scheme,
                locale,
            )
        })?;

    let registration = crate::Registration {
        vat_number: body.vat_number.trim().to_owned(),
        name: body.name.trim().to_owned(),
        name_latin: body.name_latin.clone().filter(|n| !n.trim().is_empty()),
        scheme,
        identifier: body.identifier.trim().to_owned(),
        address: crate::taxpayer::Address {
            street: body.address.street.trim().to_owned(),
            building: body.address.building.trim().to_owned(),
            additional: body
                .address
                .additional
                .clone()
                .filter(|n| !n.trim().is_empty()),
            district: body.address.district.trim().to_owned(),
            city: body.address.city.trim().to_owned(),
            postal_code: body.address.postal_code.trim().to_owned(),
            country: body.address.country.trim().to_uppercase(),
        },
        industry: Some(body.industry.trim().to_owned()),
    };

    // Not a clock reading by default either: with no date given, the
    // registration is effective from the moment it is recorded, which is the
    // only honest answer when nobody said otherwise.
    let effective_from = body.effective_from.unwrap_or_else(Utc::now);

    crate::register_taxpayer(&tenant.db, registration, effective_from, &metadata(&tenant))
        .await
        .map_err(|e| tax_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(RegistrationBody {
        effective_from: Some(effective_from),
        ..body
    }))
}

/// What is registered with ZATCA.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/registration",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = RegistrationBody),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or nothing is registered yet", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn registration(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<RegistrationBody>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::registered(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let registration = found.ok_or_else(|| {
        ApiError::NotFound(erp_i18n::Message::new(crate::messages::NOT_REGISTERED))
            .into_problem(locale, &CATALOG)
    })?;

    Ok(Json(RegistrationBody {
        vat_number: registration.vat_number,
        name: registration.name,
        name_latin: registration.name_latin,
        scheme: registration.scheme.as_str().to_owned(),
        identifier: registration.identifier,
        industry: registration.industry.unwrap_or_default(),
        address: AddressBody {
            street: registration.address.street,
            building: registration.address.building,
            additional: registration.address.additional,
            district: registration.address.district,
            city: registration.address.city,
            postal_code: registration.address.postal_code,
            country: registration.address.country,
        },
        effective_from: None,
    }))
}

/// Where the business stands with ZATCA, in one answer.
///
/// The two numbers that matter are different questions. `overdue` is
/// **simplified invoices past their 24 hours** — the ones an inspection asks
/// about. `awaiting_clearance` is **standard invoices not yet cleared**, which
/// are not late but must not have reached the buyer yet.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("as_of" = Option<String>, Query, description = "Judge the deadlines as of this instant rather than now."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = StandingView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn zatca_standing(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<AsOf>,
) -> Result<Json<StandingView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let standing = crate::standing(&mut conn, query.as_of.unwrap_or_else(Utc::now))
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(StandingView {
        registered: standing.registered,
        counts: standing
            .counts
            .into_iter()
            .map(|(status, count)| (status.as_str().to_owned(), count))
            .collect(),
        overdue: standing.overdue,
        awaiting_clearance: standing.awaiting_clearance,
        oldest_pending: standing.oldest_pending,
        chain_length: standing.chain_length,
        unsigned: standing.unsigned,
    }))
}

#[derive(Debug, Deserialize)]
struct AsOf {
    as_of: Option<Timestamp>,
}

/// Every document ZATCA has been or will be shown, most recent first.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/documents",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = erp_web::Paged<DocumentView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn zatca_documents(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(page): Query<erp_web::After>,
) -> Result<Json<erp_web::Paged<DocumentView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let after = page.cursor(locale)?;
    let limit = page.limit(DOCUMENTS, DOCUMENTS);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::documents(&mut conn, limit, after.as_ref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(erp_web::Paged::of(found, document_view)))
}

/// One document, with the UBL that was hashed and the stamp that came back.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/documents/{number}",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("number" = String, Path, description = "The statutory document number — `INV-00001`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = FullDocumentView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No document with that number", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn zatca_document(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    axum::extract::Path(number): axum::extract::Path<String>,
) -> Result<Json<FullDocumentView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::document(&mut conn, &number)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let stored = found.ok_or_else(|| {
        ApiError::NotFound(
            erp_i18n::Message::new(crate::messages::NO_SUCH_DOCUMENT)
                .with("document", erp_i18n::MessageArg::text(number.clone())),
        )
        .into_problem(locale, &CATALOG)
    })?;

    let xml = stored.xml.clone();
    let signed_xml = stored.signed_xml.clone();
    let stamped_xml = stored.stamped_xml.clone();
    Ok(Json(FullDocumentView {
        document: document_view(stored),
        xml,
        signed_xml,
        stamped_xml,
    }))
}

// ---------------------------------------------------------------------------
// Onboarding
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "environment": "simulation",
    "branch": "الفرع الرئيسي"
}))]
struct OnboardingRequest {
    /// `sandbox`, `simulation` or `production`. **Not a default** — the only
    /// visible difference is a string in the request, and a mistake onboards
    /// into the wrong authority rather than failing.
    environment: String,
    /// Only when this business wants its invoices distinct per branch: the
    /// branch this unit belongs to (for a VAT group member, their own
    /// 10-digit TIN). Absent, the unit is the whole business and carries its
    /// registered name.
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct CsrView {
    /// **The certificate request, base64** — what goes in the `csr` field of
    /// ZATCA's `POST /compliance`, with the OTP in an `OTP` header.
    csr: String,
    /// Where to send it.
    submit_to: String,
    /// How many sample documents the compliance checks will want afterwards.
    compliance_documents: usize,
    /// What to do with what comes back.
    next: &'static str,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "stage": "compliance",
    "environment": "simulation",
    "token": "TUlJQ...",
    "secret": "abc123...",
    "request_id": "1234567890123"
}))]
struct CertificateBody {
    /// `compliance` for what an OTP buys, `production` for what a passed
    /// compliance check buys.
    stage: String,
    environment: String,
    /// ZATCA's `binarySecurityToken`, verbatim.
    token: String,
    /// ZATCA's `secret`, verbatim.
    secret: String,
    /// ZATCA's `requestID`. The production request quotes the compliance one.
    #[serde(default)]
    request_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct CertificateView {
    stage: &'static str,
    environment: &'static str,
    request_id: String,
    /// The certificate's subject, as one line.
    subject: String,
    /// Its serial number, in hex. What ZATCA's support desk asks for.
    serial: String,
    not_before: String,
    not_after: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct OnboardingView {
    /// Which stages this tenant has credentials for: `compliance`, `production`.
    reached: Vec<&'static str>,
    /// Whether it can clear and report real invoices.
    live: bool,
    // `String` rather than `&'static str` because it now comes from the read
    // model rather than from an enum in memory. Same JSON either way.
    environment: Option<String>,
    /// The certificate currently on record.
    serial: Option<String>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    issued_at: Option<Timestamp>,
    /// `none`, `checking` (the worker is submitting samples or asking for the
    /// production certificate), `refused` (ZATCA said no — see `refusal`), or
    /// `live`.
    state: &'static str,
    /// The samples, once they all passed for the current certificate.
    checks: Option<ChecksView>,
    /// What ZATCA refused about the current certificate, if anything.
    refusal: Option<RefusalView>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChecksView {
    submitted: i32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    passed_at: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct RefusalView {
    /// `compliance_checks` or `production_certificate`.
    step: String,
    /// ZATCA's words, or the first refused document and its first error.
    detail: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    at: Timestamp,
}

/// Generate the key pair and the certificate request for this tenant's unit.
///
/// The private key is sealed here and **never leaves this system** — that is
/// what a certificate request is for. What comes back is the request, which goes
/// to ZATCA with the six-digit OTP the taxpayer generates in the Fatoora portal.
///
/// **Calling this again generates a new key**, which invalidates any certificate
/// already issued for the old one. Read the status first.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/zatca/onboarding",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = OnboardingRequest,
    responses(
        (status = CREATED, description = "A key pair and a request. The key is sealed here.", body = CsrView),
        (status = BAD_REQUEST, description = "An unknown environment, or a unit detail that cannot go in a certificate", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or nothing is registered with ZATCA yet", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key, so a private key cannot be stored", body = Problem),
    ),
)]
async fn begin_onboarding(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<OnboardingRequest>,
) -> Result<(StatusCode, Json<CsrView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let sealing = sealing(&state, locale)?;
    let environment = environment_of(&body.environment, locale)?;
    let registration = registered_unit(&tenant, locale).await?;
    let unit = unit_for(&registration, body.branch.as_deref(), locale)?;

    let csr = crate::zatca::onboarding::begin(&tenant.db, sealing, &unit, environment)
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(CsrView {
            csr,
            submit_to: format!("{}/compliance", environment.base_url()),
            compliance_documents: unit.issues.compliance_documents(),
            next: "POST it as {\"csr\": …} with an `OTP` header, then PUT what comes \
                   back to /v1/tax_sa/zatca/onboarding/certificate",
        }),
    ))
}

/// Record a certificate ZATCA issued for this tenant's unit.
///
/// Checked against the private key held here **before** anything is stored: a
/// certificate over somebody else's key would be accepted by this endpoint and
/// then rejected on every invoice, at clearance, with an error that says nothing
/// about why.
#[utoipa::path(
    put,
    path = "/v1/tax_sa/zatca/onboarding/certificate",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = CertificateBody,
    responses(
        (status = OK, description = "Stored. The tenant can now do what this stage allows.", body = CertificateView),
        (status = BAD_REQUEST, description = "An unknown stage or environment, a token that is not a certificate, or a certificate for another key", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or no key has been generated yet", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key", body = Problem),
    ),
)]
async fn accept_certificate(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<CertificateBody>,
) -> Result<Json<CertificateView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let sealing = sealing(&state, locale)?;
    let environment = environment_of(&body.environment, locale)?;

    let issued_for = body
        .stage
        .parse::<crate::zatca::onboarding::Stage>()
        .map_err(|_| {
            bad_request(
                erp_web::messages::UNKNOWN_ONBOARDING_STAGE,
                "stage",
                &body.stage,
                locale,
            )
        })?;

    let csid = crate::zatca::onboarding::Csid {
        token: body.token.trim().to_owned(),
        secret: body.secret.trim().to_owned(),
        request_id: body.request_id.trim().to_owned(),
    };

    let issued = crate::zatca::onboarding::accept_certificate(
        &tenant.db,
        sealing,
        issued_for,
        environment,
        &csid,
        Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| onboarding_problem(&e, locale))?;

    Ok(Json(CertificateView {
        stage: issued.stage.as_str(),
        environment: issued.environment.as_str(),
        request_id: issued.request_id,
        subject: issued.subject,
        serial: issued.serial,
        not_before: issued.not_before,
        not_after: issued.not_after,
    }))
}

/// How far this tenant has got with ZATCA onboarding.
///
/// `state` is the short answer — `none`, `checking`, `refused` or `live`;
/// `checks` and `refusal` say what the worker has done since the certificate.
/// Answered without unsealing anything: whether a secret exists is a different
/// question from what it is, and this endpoint may only ask the first.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/onboarding",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = OnboardingView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn onboarding_status(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<OnboardingView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let reached = crate::zatca::onboarding::reached(&tenant.db)
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;
    let onboarded = onboarding_row(&tenant, locale).await?;

    let state = match &onboarded {
        Some(o) if o.stage == crate::zatca::onboarding::Stage::Production.as_str() => "live",
        Some(o) if o.refused_step.is_some() => "refused",
        Some(_) => "checking",
        None => "none",
    };
    let checks = onboarded.as_ref().and_then(|o| {
        o.checks_submitted
            .zip(o.checks_passed_at)
            .map(|(submitted, passed_at)| ChecksView {
                submitted,
                passed_at,
            })
    });
    let refusal = onboarded.as_ref().and_then(|o| {
        Some(RefusalView {
            step: o.refused_step.clone()?,
            detail: o.refused_detail.clone().unwrap_or_default(),
            at: o.refused_at?,
        })
    });

    Ok(Json(OnboardingView {
        live: reached.contains(&crate::zatca::onboarding::Stage::Production),
        reached: reached
            .into_iter()
            .map(crate::zatca::onboarding::Stage::as_str)
            .collect(),
        environment: onboarded.as_ref().map(|o| o.environment.clone()),
        serial: onboarded.as_ref().map(|o| o.serial.clone()),
        issued_at: onboarded.as_ref().map(|o| o.issued_at),
        state,
        checks,
        refusal,
    }))
}

/// The unit, from the registration and at most a branch.
///
/// The VAT number, the legal name, the address and the industry are the
/// registration's — a second endpoint restating them is how the certificate
/// ends up naming a different business from the invoices. Both document types
/// are always declared. The serial and the common name are minted here: they
/// identify this unit to ZATCA and nobody has a better name for it.
fn unit_for(
    registration: &crate::Registration,
    branch: Option<&str>,
    locale: Locale,
) -> Result<crate::zatca::csr::Unit, Problem> {
    let industry = registration
        .industry
        .as_deref()
        .map(str::trim)
        .filter(|industry| !industry.is_empty())
        .ok_or_else(|| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Message::new(crate::messages::NO_INDUSTRY),
                locale,
                &CATALOG,
            )
        })?;
    let branch = branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .unwrap_or(registration.name.as_str());
    // A stable EGS serial, **derived** from the unit's identity rather than
    // minted, so a retried activation produces the same request instead of a
    // second certificate (L8) — and nothing a person has to think up. Twelve
    // hex characters is unique enough for the one unit a tenant has.
    let hex = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("{}:{branch}", registration.vat_number).as_bytes(),
    )
    .simple()
    .to_string();
    let serial = hex[hex.len() - 12..].to_owned();

    Ok(crate::zatca::csr::Unit {
        vat_number: registration.vat_number.clone(),
        organization: registration.name.clone(),
        branch: branch.to_owned(),
        common_name: format!("EGS-{serial}"),
        // This software, not the tenant's. A solution name a tenant could set
        // is one that stops matching what is registered with ZATCA.
        solution: SOLUTION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial,
        address: format!(
            "{} {} {}",
            registration.address.street,
            registration.address.city,
            registration.address.postal_code
        ),
        industry: industry.to_owned(),
        issues: crate::zatca::csr::Issues::both(),
    })
}

/// What this software calls itself to ZATCA. Registered once, per solution.
const SOLUTION: &str = "Erp";

fn environment_of(value: &str, locale: Locale) -> Result<crate::zatca::csr::Environment, Problem> {
    value.parse().map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_ZATCA_ENVIRONMENT,
            "environment",
            value,
            locale,
        )
    })
}

/// The deployment's sealing key, or a refusal.
///
/// **Not a degraded mode.** Without it there is nowhere safe to put a signing
/// key, and storing one in the clear because an environment variable is missing
/// is exactly the "log a warning and continue" this system does not do (L6).
fn sealing(state: &AppState, locale: Locale) -> Result<&erp_eventlog::SealingKey, Problem> {
    state.sealing.as_ref().ok_or_else(|| {
        erp_web::Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(erp_web::messages::NO_SEALING_KEY),
            locale,
            &CATALOG,
        )
    })
}

fn onboarding_problem(error: &crate::zatca::onboarding::OnboardError, locale: Locale) -> Problem {
    use crate::zatca::onboarding::OnboardError;

    let (status, message) = match error {
        // The caller's, and each one names what to fix.
        OnboardError::Csr(reason) => (
            StatusCode::BAD_REQUEST,
            erp_i18n::Message::new(erp_web::messages::UNUSABLE_UNIT)
                .with("reason", erp_i18n::MessageArg::text(reason.to_string())),
        ),
        OnboardError::Certificate(reason) => (
            StatusCode::BAD_REQUEST,
            erp_i18n::Message::new(erp_web::messages::UNREADABLE_CERTIFICATE)
                .with("reason", erp_i18n::MessageArg::text(reason.clone())),
        ),
        OnboardError::KeyMismatch => (
            StatusCode::BAD_REQUEST,
            erp_i18n::Message::new(erp_web::messages::CERTIFICATE_KEY_MISMATCH),
        ),
        OnboardError::NotYet(what) => (
            StatusCode::NOT_FOUND,
            erp_i18n::Message::new(erp_web::messages::ONBOARDING_NOT_YET)
                .with("stage", erp_i18n::MessageArg::text((*what).to_owned())),
        ),
        // ZATCA's.
        OnboardError::NotIssued {
            disposition,
            detail,
        } => (
            StatusCode::BAD_GATEWAY,
            erp_i18n::Message::new(erp_web::messages::CSID_NOT_ISSUED)
                .with(
                    "disposition",
                    erp_i18n::MessageArg::text(disposition.clone()),
                )
                .with("detail", erp_i18n::MessageArg::text(detail.clone())),
        ),
        // **Which of the four calls**, because they all fail the same way and
        // an error that does not say leaves somebody bisecting a flow that
        // talked to a tax authority.
        OnboardError::Unanswered { step, source } => (
            StatusCode::BAD_GATEWAY,
            erp_i18n::Message::new(erp_web::messages::ZATCA_UNREACHABLE)
                .with("step", erp_i18n::MessageArg::text((*step).to_owned()))
                .with("reason", erp_i18n::MessageArg::text(source.to_string())),
        ),
        // Ours.
        other => {
            tracing::error!(error = %other, "ZATCA onboarding failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "environment": "simulation",
    "otp": "123456"
}))]
struct ActivationRequest {
    /// `sandbox`, `simulation` or `production` — whichever portal the OTP was
    /// generated in.
    environment: String,
    /// **The six digits the taxpayer generates in the Fatoora portal.** Valid
    /// for about an hour, used once, and never stored here.
    otp: String,
    /// Only when this business wants its invoices distinct per branch. Absent,
    /// the unit is the whole business.
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ActivationView {
    /// The certificate the OTP bought. It can sign the compliance samples and
    /// nothing else.
    compliance: CertificateView,
    /// `checking`: the worker now submits the samples and asks for the
    /// production certificate. Watch `GET /v1/tax_sa/zatca/onboarding`.
    state: &'static str,
    /// How many sample documents the worker will submit.
    checks_expected: usize,
}

/// Start taking this business live with ZATCA, from a Fatoora OTP.
///
/// This request spends the OTP: a key pair and a certificate request are
/// generated here, the OTP buys the compliance certificate, and both are
/// sealed. **The worker does the rest** — one signed sample of every document
/// type, then the production certificate — and `GET
/// /v1/tax_sa/zatca/onboarding` says where it stands. Nothing after this
/// request needs the taxpayer or a second OTP.
///
/// The unit is the registration's: VAT number, legal name, address and
/// industry. Both document types are declared. A `branch` is only for a
/// business that wants its invoices distinct per branch.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/zatca/onboarding/activate",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = ActivationRequest,
    responses(
        (status = ACCEPTED, description = "The compliance certificate is sealed; the worker is finishing. Watch the status.", body = ActivationView),
        (status = BAD_REQUEST, description = "An OTP that is not six digits, an unknown environment, or a registration with no industry", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or nothing is registered with ZATCA yet", body = Problem),
        (status = CONFLICT, description = "Already live in this environment. Renew or replace the certificate through the manual route.", body = Problem),
        (status = BAD_GATEWAY, description = "ZATCA refused the OTP, or could not be reached. Nothing was stored.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key", body = Problem),
    ),
)]
async fn activate(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<ActivationRequest>,
) -> Result<(StatusCode, Json<ActivationView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let sealing = sealing(&state, locale)?;
    let environment = environment_of(&body.environment, locale)?;

    let otp = body
        .otp
        .parse::<crate::zatca::onboarding::Otp>()
        .map_err(|_| {
            // The value itself never reaches the message: an OTP in a log is a
            // certificate somebody else can obtain for an hour.
            bad_request(erp_web::messages::NOT_AN_OTP, "otp", "", locale)
        })?;
    let registration = registered_unit(&tenant, locale).await?;
    let unit = unit_for(&registration, body.branch.as_deref(), locale)?;
    refuse_if_live(&tenant, environment, locale).await?;

    let fatoora = crate::zatca::http::Fatoora::new(environment).map_err(|source| {
        onboarding_problem(
            &crate::zatca::onboarding::OnboardError::Unanswered {
                step: "building a client",
                source,
            },
            locale,
        )
    })?;
    let compliance = crate::zatca::onboarding::Onboarder::new(&tenant.db, sealing, &fatoora)
        .onboard(&unit, environment, &otp, Utc::now(), &metadata(&tenant))
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    // So the worker finishes within a visit rather than on its schedule.
    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::ACCEPTED,
        Json(ActivationView {
            compliance: certificate_view(compliance),
            state: "checking",
            checks_expected: unit.issues.compliance_documents(),
        }),
    ))
}

fn certificate_view(issued: crate::zatca::onboarding::Issued) -> CertificateView {
    CertificateView {
        stage: issued.stage.as_str(),
        environment: issued.environment.as_str(),
        request_id: issued.request_id,
        subject: issued.subject,
        serial: issued.serial,
        not_before: issued.not_before,
        not_after: issued.not_after,
    }
}

/// The tenant's ZATCA registration, or a 404 that says to make one first.
async fn registered_unit<C: erp_web::Capability>(
    tenant: &Allowed<C>,
    locale: Locale,
) -> Result<crate::Registration, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::registered(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    found.ok_or_else(|| {
        ApiError::NotFound(erp_i18n::Message::new(crate::messages::NOT_REGISTERED))
            .into_problem(locale, &CATALOG)
    })
}

/// The onboarding row, or a 500 that is ours: a read model this module owns
/// failing is not something a caller can act on.
async fn onboarding_row<C: erp_web::Capability>(
    tenant: &Allowed<C>,
    locale: Locale,
) -> Result<Option<crate::Onboarded>, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let onboarded = crate::projections::onboarding(&mut conn)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "reading the onboarding read model failed");
            Problem::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
                locale,
                &CATALOG,
            )
        })?;
    drop(conn);
    Ok(onboarded)
}

/// **409 when this business is already live in that environment.** Asking again
/// would seal a new key and orphan the production certificate that clears its
/// invoices; a renewal or a key replacement is an operator's act through the
/// manual path. Read from the projection because the destructive step comes
/// before any command handler runs; two activations racing past it both get
/// valid certificates and the later one wins, which is harmless.
async fn refuse_if_live(
    tenant: &Allowed<ManageTenant>,
    environment: crate::zatca::csr::Environment,
    locale: Locale,
) -> Result<(), Problem> {
    let live_here = onboarding_row(tenant, locale).await?.is_some_and(|o| {
        o.stage == crate::zatca::onboarding::Stage::Production.as_str()
            && o.environment == environment.as_str()
    });
    if live_here {
        return Err(Problem::new(
            StatusCode::CONFLICT,
            &erp_i18n::Message::new(crate::messages::ALREADY_LIVE).with(
                "environment",
                erp_i18n::MessageArg::text(environment.as_str().to_owned()),
            ),
            locale,
            &CATALOG,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What the customer holds
// ---------------------------------------------------------------------------

/// How long a print waits for the worker: long enough for the visit the sale
/// asked for — projection, signature, submission — and short enough to answer
/// before a proxy gives up on the request.
const PRINT_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Deserialize)]
struct PrintQuery {
    /// Seconds to wait for the signature or the clearance before answering.
    /// The default is the most, 20; `0` answers at once.
    wait: Option<u64>,
}

impl PrintQuery {
    fn wait(&self) -> std::time::Duration {
        let seconds = self
            .wait
            .unwrap_or(PRINT_WAIT.as_secs())
            .min(PRINT_WAIT.as_secs());
        std::time::Duration::from_secs(seconds)
    }
}

/// A link a customer opens without signing in.
#[derive(Debug, Serialize, ToSchema)]
struct LinkView {
    /// Relative to the tenant's host: `/v1/tax_sa/zatca/public/INV-00001.<mac>`.
    link: String,
}

/// **The document, once it may be handed over** — waiting for the worker up
/// to `wait`. Nothing is served before the signature, and a standard invoice
/// nothing before the clearance; see [`crate::deliverable`].
async fn handed_over(
    db: &erp_tenant::TenantDb,
    number: &str,
    wait: std::time::Duration,
    locale: Locale,
) -> Result<(crate::zatca::Document, crate::Deliverable), Problem> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let mut conn = db
            .read()
            .await
            .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
        let found = crate::document(&mut conn, number)
            .await
            .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
        drop(conn);
        let Some(stored) = found else {
            return Err(no_such_document(number, locale));
        };
        match crate::deliverable(&stored) {
            Ok(deliverable) => {
                let document = stored.document.ok_or_else(|| {
                    undeliverable(crate::NotDeliverable::Unregistered, number, locale)
                })?;
                return Ok((document, deliverable));
            }
            Err(crate::NotDeliverable::NotYetSigned | crate::NotDeliverable::AwaitingClearance)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(why) => return Err(undeliverable(why, number, locale)),
        }
    }
}

fn no_such_document(number: &str, locale: Locale) -> Problem {
    ApiError::NotFound(
        erp_i18n::Message::new(crate::messages::NO_SUCH_DOCUMENT)
            .with("document", erp_i18n::MessageArg::text(number.to_owned())),
    )
    .into_problem(locale, &CATALOG)
}

/// Why a document is not handed over, as the caller sees it.
fn undeliverable(why: crate::NotDeliverable, number: &str, locale: Locale) -> Problem {
    use crate::NotDeliverable;
    let (status, code) = match why {
        // Retryable: the worker is on its way.
        NotDeliverable::NotYetSigned => (
            StatusCode::SERVICE_UNAVAILABLE,
            crate::messages::NOT_YET_SIGNED,
        ),
        NotDeliverable::AwaitingClearance => {
            (StatusCode::CONFLICT, crate::messages::AWAITING_CLEARANCE)
        }
        NotDeliverable::Refused => (StatusCode::CONFLICT, crate::messages::DOCUMENT_REFUSED),
        NotDeliverable::Unregistered => (StatusCode::CONFLICT, crate::messages::NOT_DELIVERABLE),
    };
    Problem::new(
        status,
        &erp_i18n::Message::new(code)
            .with("document", erp_i18n::MessageArg::text(number.to_owned())),
        locale,
        &CATALOG,
    )
}

/// The document, print-ready.
///
/// HTML with the QR inline as SVG and nothing fetched from anywhere: a till
/// prints it from the browser, a phone renders it from a link. An 80 mm
/// receipt for a simplified invoice, an A4 page for a standard one and for a
/// credit note; Arabic first and English beside it throughout.
///
/// **Waits for the worker.** A simplified invoice's QR carries the stamp, so
/// nothing is served before the document is signed; a standard invoice is not
/// a valid invoice until ZATCA has cleared it, so nothing is served before
/// that — and what is served then is the document ZATCA stamped. Both happen
/// on the visit the sale asked for, usually within seconds, and this waits up
/// to `wait` for them.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/documents/{number}/print",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("number" = String, Path, description = "The statutory document number — `INV-00001`."),
        ("wait" = Option<u64>, Query, description = "Seconds to wait for the signature or the clearance. Default and most 20; 0 answers at once."),
    ),
    responses(
        (status = OK, description = "The document, as a page.", content_type = "text/html"),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No document with that number", body = Problem),
        (status = CONFLICT, description = "Not to be handed over: a standard invoice ZATCA has not cleared — `tax_sa.awaiting_clearance`; one it refused — `tax_sa.document_refused`; one issued before registration — `tax_sa.not_deliverable`", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Not signed yet — `tax_sa.not_yet_signed`. Retry.", body = Problem),
    ),
)]
async fn print_document(
    tenant: Allowed<Read>,
    Language(locale): Language,
    axum::extract::Path(number): axum::extract::Path<String>,
    Query(query): Query<PrintQuery>,
) -> Result<axum::response::Html<String>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (document, deliverable) = handed_over(&tenant.db, &number, query.wait(), locale).await?;
    Ok(axum::response::Html(crate::print::html(
        &document,
        &deliverable.qr,
    )))
}

/// The document for a buyer's system: the one ZATCA stamped on a cleared
/// standard invoice, the signed one on a simplified invoice. Waits and refuses
/// exactly as the print does.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/documents/{number}/xml",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("number" = String, Path, description = "The statutory document number — `INV-00001`."),
        ("wait" = Option<u64>, Query, description = "Seconds to wait for the signature or the clearance. Default and most 20; 0 answers at once."),
    ),
    responses(
        (status = OK, description = "The UBL document, as a file.", content_type = "application/xml"),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No document with that number", body = Problem),
        (status = CONFLICT, description = "Not to be handed over — see the print", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Not signed yet — `tax_sa.not_yet_signed`. Retry.", body = Problem),
    ),
)]
async fn document_xml(
    tenant: Allowed<Read>,
    Language(locale): Language,
    axum::extract::Path(number): axum::extract::Path<String>,
    Query(query): Query<PrintQuery>,
) -> Result<axum::response::Response, Problem> {
    use axum::response::IntoResponse as _;
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let (_, deliverable) = handed_over(&tenant.db, &number, query.wait(), locale).await?;
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/xml; charset=utf-8".to_owned(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{number}.xml\""),
            ),
        ],
        deliverable.xml,
    )
        .into_response())
}

/// A link the customer opens without signing in.
///
/// The document's number and an HMAC of it under a secret this business keeps
/// (made on the first link asked for), so it cannot be guessed from the number
/// and needs nothing stored per document. It opens the same print, under the
/// same waiting and the same refusals. Relative to the business's own host.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/zatca/documents/{number}/link",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("number" = String, Path, description = "The statutory document number — `INV-00001`."),
    ),
    responses(
        (status = OK, body = LinkView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No document with that number", body = Problem),
    ),
)]
async fn document_link(
    tenant: Allowed<Read>,
    Language(locale): Language,
    axum::extract::Path(number): axum::extract::Path<String>,
) -> Result<Json<LinkView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    if crate::document(&mut conn, &number)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?
        .is_none()
    {
        return Err(no_such_document(&number, locale));
    }
    let secret = crate::LinkSecret::resolve_or_create(&mut conn)
        .await
        .map_err(|e| erp_web::config_problem(&e, locale, &CATALOG))?;
    Ok(Json(LinkView {
        link: format!("/v1/tax_sa/zatca/public/{}", secret.token(&number)),
    }))
}

/// The print, for whoever holds the link.
///
/// No sign-in: the link is the credential. Bounded per caller and per business
/// like every public route, and it waits and refuses exactly as the staff
/// print does.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/public/{token}",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The business's subdomain — `bassat.erp.com`."),
        ("token" = String, Path, description = "From `POST /v1/tax_sa/zatca/documents/{number}/link`."),
    ),
    security(),
    responses(
        (status = OK, description = "The document, as a page.", content_type = "text/html"),
        (status = NOT_FOUND, description = "A link that opens nothing here — `tax_sa.no_such_link`", body = Problem),
        (status = CONFLICT, description = "Not to be handed over — see the print", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Not signed yet — `tax_sa.not_yet_signed`. Retry.", body = Problem),
        (status = TOO_MANY_REQUESTS, body = Problem),
    ),
)]
async fn public_print(
    caller: Public,
    Language(locale): Language,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> Result<axum::response::Html<String>, Problem> {
    require_module(&caller.db, &crate::module_id(), locale)?;
    let no_such_link = || {
        ApiError::NotFound(erp_i18n::Message::new(crate::messages::NO_SUCH_LINK))
            .into_problem(locale, &CATALOG)
    };
    let mut conn = caller
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let secret =
        erp_eventlog::configuration::get::<crate::LinkSecret>(&mut conn, crate::LinkSecret::KEY)
            .await
            .map_err(|e| erp_web::config_problem(&e, locale, &CATALOG))?
            .map(|configured| configured.value)
            .ok_or_else(no_such_link)?;
    drop(conn);
    let number = secret.opens(&token).ok_or_else(no_such_link)?;
    let (document, deliverable) = handed_over(&caller.db, &number, PRINT_WAIT, locale).await?;
    Ok(axum::response::Html(crate::print::html(
        &document,
        &deliverable.qr,
    )))
}
