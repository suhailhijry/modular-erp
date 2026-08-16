//! The Saudi tax module's HTTP surface.
//!
//! Translation only, like every other module's. What is different is where the
//! *domain* went: the return used to be netted here, in the composition root,
//! and it is computed in `tax_sa::vat_return` now. This file asks which modules
//! the tenant has, calls the module, and renders.
//!
//! ponytail: still in `spa-api`, like `ledger_routes` and the rest. Under the
//! core/module split a module ships its own routes and core mounts them — which
//! is the restructure this file is waiting for rather than an argument against
//! it.

use axum::extract::State;
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::ExecuteError;
use spa_i18n::{Locale, Localize};
use spa_types::{CurrencyCode, Timestamp};
use tax_sa::{Sides, TaxError};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::consistency::{Consistency, nudge};
use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageAccounts, ManageTenant, Read};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::{Json, Query, bad_request, metadata, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(vat_return))
        .routes(routes!(filed_returns, file_return))
        .routes(routes!(registration, register))
        .routes(routes!(zatca_standing))
        .routes(routes!(zatca_documents))
        .routes(routes!(zatca_document))
}

/// How many filed returns a list gives back. A business files four a year.
const PAGE: i64 = 200;

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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
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
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let declared = tax_sa::vat_return(&mut conn, sides, currency, from, until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    let (currency, from, until) = period_of(&body.currency, body.from, body.until, locale)?;

    let filed = tax_sa::file_return(
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

    let period = tax_sa::period_id(currency, from, until)
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let returns = tax_sa::filed(&mut conn, PAGE)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
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
fn sides_of<C: crate::extract::Capability>(tenant: &Allowed<C>) -> Sides {
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
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            currency,
            locale,
        )
    })?;
    if until <= from {
        return Err(bad_request(
            crate::messages::EMPTY_PERIOD,
            "period",
            &from.to_rfc3339(),
            locale,
        ));
    }
    Ok((parsed, from, until))
}

fn view(declared: &tax_sa::Return) -> ReturnView {
    let side = |s: &tax_sa::Side| SideView {
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

        CommandError::Pool(e @ spa_control::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            spa_i18n::Message::new(spa_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        other => {
            tracing::error!(error = %other, "tax command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                spa_i18n::Message::new(spa_control::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
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
    /// The base64 TLV block that goes on the printed document.
    qr: Option<String>,
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
    /// The canonical UBL that was hashed and submitted.
    xml: Option<String>,
    /// **The document ZATCA stamped, base64** — the one a buyer must be given.
    /// Cleared standard invoices only.
    stamped_xml: Option<String>,
}

fn document_view(stored: tax_sa::Stored) -> DocumentView {
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;

    let scheme = body
        .scheme
        .parse::<tax_sa::taxpayer::IdScheme>()
        .map_err(|_| {
            bad_request(
                crate::messages::UNKNOWN_ID_SCHEME,
                "scheme",
                &body.scheme,
                locale,
            )
        })?;

    let registration = tax_sa::Registration {
        vat_number: body.vat_number.trim().to_owned(),
        name: body.name.trim().to_owned(),
        name_latin: body.name_latin.clone().filter(|n| !n.trim().is_empty()),
        scheme,
        identifier: body.identifier.trim().to_owned(),
        address: tax_sa::taxpayer::Address {
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

    tax_sa::register_taxpayer(&tenant.db, registration, effective_from, &metadata(&tenant))
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let found = tax_sa::registered(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    let registration = found.ok_or_else(|| {
        ApiError::NotFound(spa_i18n::Message::new(tax_sa::messages::NOT_REGISTERED))
            .into_problem(locale)
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let standing = tax_sa::standing(&mut conn, query.as_of.unwrap_or_else(Utc::now))
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<DocumentView>),
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
) -> Result<Json<Vec<DocumentView>>, Problem> {
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let found = tax_sa::documents(&mut conn, DOCUMENTS)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    Ok(Json(found.into_iter().map(document_view).collect()))
}

/// One document, with the UBL that was hashed and the stamp that came back.
#[utoipa::path(
    get,
    path = "/v1/tax_sa/zatca/documents/{number}",
    tag = "tax_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &tax_sa::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, tax_sa::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let found = tax_sa::document(&mut conn, &number)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    let stored = found.ok_or_else(|| {
        ApiError::NotFound(
            spa_i18n::Message::new(tax_sa::messages::NO_SUCH_DOCUMENT)
                .with("document", spa_i18n::MessageArg::text(number.clone())),
        )
        .into_problem(locale)
    })?;

    let xml = stored.xml.clone();
    let stamped_xml = stored.stamped_xml.clone();
    Ok(Json(FullDocumentView {
        document: document_view(stored),
        xml,
        stamped_xml,
    }))
}
