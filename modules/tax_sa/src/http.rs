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
use erp_web::{Allowed, Language, ManageAccounts, ManageTenant, Read};
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
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// **Exclusive**, so consecutive returns neither overlap nor leave a day out.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    currency: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "from": "2026-01-01T00:00:00Z",
    "until": "2026-04-01T00:00:00Z",
    "currency": "SAR",
    "filed_on": "2026-04-28T00:00:00Z"
}))]
struct NewFiling {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// **Exclusive.**
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
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
        ("from" = String, Query, description = "Start of the period, inclusive. RFC 3339."),
        ("until" = String, Query, description = "End of the period, **exclusive**. RFC 3339."),
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
            from,
            until,
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    from: Timestamp,
    until: Timestamp,
    locale: Locale,
) -> Result<(CurrencyCode, Timestamp, Timestamp), Problem> {
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
            &from.to_rfc3339(),
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
    require_module(&tenant, &crate::module_id(), locale)?;

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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    "branch": "الفرع الرئيسي",
    "common_name": "EGS1-886431145",
    "serial": "886431145",
    "industry": "Consulting",
    "issues_standard": true,
    "issues_simplified": true
}))]
struct OnboardingRequest {
    /// `sandbox`, `simulation` or `production`. **Not a default** — the only
    /// visible difference is a string in the request, and a mistake onboards
    /// into the wrong authority rather than failing.
    environment: String,
    /// The branch this unit belongs to. For a VAT group member, their own
    /// 10-digit TIN.
    branch: String,
    /// A name for this unit, unique among the taxpayer's units.
    common_name: String,
    /// This unit's serial number, unique per taxpayer.
    serial: String,
    /// The taxpayer's industry.
    industry: String,
    /// Whether this unit issues standard invoices — the ones cleared before the
    /// buyer gets them.
    #[serde(default = "yes")]
    issues_standard: bool,
    /// Whether it issues simplified ones — reported within 24 hours.
    #[serde(default = "yes")]
    issues_simplified: bool,
}

const fn yes() -> bool {
    true
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
    require_module(&tenant, &crate::module_id(), locale)?;
    let sealing = sealing(&state, locale)?;
    let environment = environment_of(&body.environment, locale)?;
    let unit = unit_for(&tenant, &body, locale).await?;

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
    require_module(&tenant, &crate::module_id(), locale)?;
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
    require_module(&tenant, &crate::module_id(), locale)?;

    let reached = crate::zatca::onboarding::reached(&tenant.db)
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let onboarded = crate::projections::onboarding(&mut conn)
        .await
        .map_err(|e| {
            // A read model this module owns failing is ours to get right, so it
            // is not something a caller can act on.
            tracing::error!(error = %e, "reading the onboarding read model failed");
            Problem::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
                locale,
                &CATALOG,
            )
        })?;
    drop(conn);

    Ok(Json(OnboardingView {
        live: reached.contains(&crate::zatca::onboarding::Stage::Production),
        reached: reached
            .into_iter()
            .map(crate::zatca::onboarding::Stage::as_str)
            .collect(),
        environment: onboarded.as_ref().map(|o| o.environment.clone()),
        serial: onboarded.as_ref().map(|o| o.serial.clone()),
        issued_at: onboarded.as_ref().map(|o| o.issued_at),
    }))
}

/// The unit, built from the request and from what the tenant already registered.
///
/// The VAT number and the legal name are **not** in the request body: they are
/// the ZATCA registration this tenant already made, and letting a second
/// endpoint restate them is how the certificate ends up naming a different
/// business from the invoices.
async fn unit_for(
    tenant: &Allowed<ManageTenant>,
    body: &OnboardingRequest,
    locale: Locale,
) -> Result<crate::zatca::csr::Unit, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let registration = crate::registered(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let registration = registration.ok_or_else(|| {
        ApiError::NotFound(erp_i18n::Message::new(crate::messages::NOT_REGISTERED))
            .into_problem(locale, &CATALOG)
    })?;

    Ok(crate::zatca::csr::Unit {
        vat_number: registration.vat_number,
        organization: registration.name,
        branch: body.branch.trim().to_owned(),
        common_name: body.common_name.trim().to_owned(),
        // This software, not the tenant's. A solution name a tenant could set
        // is one that stops matching what is registered with ZATCA.
        solution: SOLUTION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: body.serial.trim().to_owned(),
        address: format!(
            "{} {} {}",
            registration.address.street,
            registration.address.city,
            registration.address.postal_code
        ),
        industry: body.industry.trim().to_owned(),
        issues: crate::zatca::csr::Issues {
            standard: body.issues_standard,
            simplified: body.issues_simplified,
        },
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
    "otp": "123456",
    "branch": "الفرع الرئيسي",
    "common_name": "EGS1-886431145",
    "serial": "886431145",
    "industry": "Consulting"
}))]
struct ActivationRequest {
    environment: String,
    /// **The six digits the taxpayer generates in the Fatoora portal.** Valid
    /// for about an hour, used once, and never stored here.
    otp: String,
    branch: String,
    common_name: String,
    serial: String,
    industry: String,
    #[serde(default = "yes")]
    issues_standard: bool,
    #[serde(default = "yes")]
    issues_simplified: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct ActivationView {
    /// The certificate an OTP bought.
    compliance: CertificateView,
    /// How many sample documents ZATCA was shown, and how many it accepted.
    checks_submitted: usize,
    checks_passed: usize,
    /// **The one that clears real invoices.**
    production: CertificateView,
}

/// Take this business all the way live with ZATCA, from a Fatoora OTP.
///
/// Four calls to ZATCA, in the order it requires:
///
/// 1. a key pair and a certificate request, generated here,
/// 2. `POST /compliance` with the OTP — the compliance certificate,
/// 3. `POST /compliance/invoices` — one signed sample of every document type
///    this unit declared, which ZATCA must accept before it will go further,
/// 4. `POST /production/csids` — the certificate that clears real invoices.
///
/// **Nothing is stored unless the step that produced it succeeded**, and the
/// private key is sealed before the first call, so a certificate is never issued
/// against a key this system no longer has.
///
/// If the compliance samples are refused, that is **this software's problem, not
/// the caller's** — the samples are generated here — and it answers 502 with
/// what ZATCA said.
#[utoipa::path(
    post,
    path = "/v1/tax_sa/zatca/onboarding/activate",
    tag = "tax_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = ActivationRequest,
    responses(
        (status = OK, description = "Live. This business can now clear and report invoices.", body = ActivationView),
        (status = BAD_REQUEST, description = "An OTP that is not six digits, an unknown environment, or a unit detail that cannot go in a certificate", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, the module is not enabled, or nothing is registered with ZATCA yet", body = Problem),
        (status = BAD_GATEWAY, description = "ZATCA refused, or could not be reached. Nothing beyond the last successful step was stored.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key", body = Problem),
    ),
)]
async fn activate(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<ActivationRequest>,
) -> Result<Json<ActivationView>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
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
    let unit = crate::zatca::csr::Unit {
        branch: body.branch.trim().to_owned(),
        common_name: body.common_name.trim().to_owned(),
        serial: body.serial.trim().to_owned(),
        industry: body.industry.trim().to_owned(),
        issues: crate::zatca::csr::Issues {
            standard: body.issues_standard,
            simplified: body.issues_simplified,
        },
        ..unit_from(&registration)
    };

    let fatoora = crate::zatca::http::Fatoora::new(environment).map_err(|source| {
        onboarding_problem(
            &crate::zatca::onboarding::OnboardError::Unanswered {
                step: "building a client",
                source,
            },
            locale,
        )
    })?;
    let onboarder = crate::zatca::onboarding::Onboarder::new(&tenant.db, sealing, &fatoora);
    let now = Utc::now();

    let compliance = onboarder
        .onboard(&unit, environment, &otp, now, &metadata(&tenant))
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    let checks = onboarder
        .pass_compliance_checks(&registration, &unit, environment, now)
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    if !checks.all_passed() {
        // **Ours, not the caller's.** The samples are generated here, so ZATCA
        // refusing one is a bug in this software — logged in full, and reported
        // with enough for somebody to act on.
        tracing::error!(
            submitted = checks.submitted,
            passed = checks.passed,
            failures = ?checks.failures,
            "ZATCA refused a compliance document"
        );
        let reason = checks
            .failures
            .first()
            .map(|(document, errors)| {
                let first = errors
                    .first()
                    .map_or_else(String::new, |e| format!("{}: {}", e.code, e.message));
                format!("{document} — {first}")
            })
            .unwrap_or_default();

        return Err(Problem::new(
            StatusCode::BAD_GATEWAY,
            &erp_i18n::Message::new(erp_web::messages::COMPLIANCE_REFUSED)
                .with(
                    "failed",
                    erp_i18n::MessageArg::Count(
                        i64::try_from(checks.submitted - checks.passed).unwrap_or_default(),
                    ),
                )
                .with(
                    "submitted",
                    erp_i18n::MessageArg::Count(
                        i64::try_from(checks.submitted).unwrap_or_default(),
                    ),
                )
                .with("reason", erp_i18n::MessageArg::text(reason)),
            locale,
            &CATALOG,
        ));
    }

    let production = onboarder
        .go_live(environment, now, &metadata(&tenant))
        .await
        .map_err(|e| onboarding_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(ActivationView {
        compliance: certificate_view(compliance),
        checks_submitted: checks.submitted,
        checks_passed: checks.passed,
        production: certificate_view(production),
    }))
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

/// The parts of a unit that come from the registration rather than the request.
///
/// The VAT number and the legal name are **not** in any request body: they are
/// what this business already registered, and a second endpoint restating them
/// is how a certificate ends up naming a different business from the invoices.
fn unit_from(registration: &crate::Registration) -> crate::zatca::csr::Unit {
    crate::zatca::csr::Unit {
        vat_number: registration.vat_number.clone(),
        organization: registration.name.clone(),
        branch: String::new(),
        common_name: String::new(),
        solution: SOLUTION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: String::new(),
        address: format!(
            "{} {} {}",
            registration.address.street,
            registration.address.city,
            registration.address.postal_code
        ),
        industry: String::new(),
        issues: crate::zatca::csr::Issues::both(),
    }
}
