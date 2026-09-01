//! The pos module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # A sale answers with the receipt, not with an id
//!
//! `POST /v1/pos/shifts/{shift}/sales` returns the statutory invoice **number**
//! and the total, because that is what a receipt prints and what a customer is
//! handed. The document itself is `sales`', and `GET /v1/sales/invoices/{id}`
//! is where it is read — this module never copies it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::{CurrencyCode, Money, Timestamp};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{
    After, Allowed, Amount, IdempotencyKey, Language, ManageAccounts, Paged, PostEntries,
};
use erp_web::{Consistency, Read, nudge};
use erp_web::{Json, Query, bad_request, creating, metadata, parse_id, require_module};

use crate::{Basket, Method, Opening, PayOut, PosError, Tender};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_shifts, open_shift))
        .routes(routes!(get_shift))
        .routes(routes!(shift_takings))
        .routes(routes!(ring_sale))
        .routes(routes!(pay_out))
        .routes(routes!(close_shift))
        .routes(routes!(till_accounts, set_till_accounts))
}

/// This module's failures plus everything any route can produce. `sales` is in
/// here because a till sale is one of its invoices, and its refusals say what is
/// wrong with a document better than this module could reword them.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &sales::CATALOG,
    &ledger::CATALOG,
    &crm::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "till": "١",
    "float": {"minor": 50_000, "currency": "SAR"}
}))]
struct NewShift {
    /// Your own name for this counter.
    till: String,
    /// What is in the drawer before anything is sold.
    float: Amount,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewTender {
    /// `cash`, `card` or `transfer`. **Only cash is in the drawer**, so only
    /// cash changes what the count should come to.
    method: String,
    amount: Amount,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewLine {
    description: String,
    /// Before tax, in the sale's currency. **The rate is not yours to send**:
    /// it is the tenant's configured one, resolved inside the write, so an
    /// invoice cannot be stamped with a rate that was never current.
    net: i64,
    /// `standard`, `zero` or `exempt`.
    vat: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct SaleDiscount {
    amount: i64,
    reason: String,
    vat: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "customer": {"name": "زبون"},
    "currency": "SAR",
    "lines": [{"description": "قهوة", "net": 1_500, "vat": "standard"}],
    "tenders": [{"method": "cash", "amount": {"minor": 1_725, "currency": "SAR"}}]
}))]
struct NewSale {
    /// Who it is for. A walk-in is a name and nothing else; **a VAT number makes
    /// this a standard invoice**, which ZATCA clears before the customer may be
    /// given it.
    customer: SaleCustomer,
    /// ISO 4217, upper case.
    currency: String,
    lines: Vec<NewLine>,
    #[serde(default)]
    discounts: Vec<SaleDiscount>,
    /// How it was paid. Must come to **exactly** the sale: less would leave a
    /// balance owing, which is an invoice on credit and not a till sale, and
    /// change handed back is a counter concern rather than a record.
    tenders: Vec<NewTender>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct SaleCustomer {
    /// The `crm` record, when there is one.
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    vat_number: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewPayOut {
    /// Your key. Sending it twice is a no-op.
    reference: String,
    amount: Amount,
    /// The account code the money went to.
    to: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewCount {
    /// What was actually counted in the drawer.
    declared: Amount,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

/// An amount on the way out. `erp_web::Amount` is the way in and is
/// deserialize-only, which is the right asymmetry.
#[derive(Debug, Serialize, ToSchema)]
struct TillCash {
    minor: i64,
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ShiftRecord {
    id: String,
    till: String,
    operator: String,
    float: TillCash,
    /// What the drawer should hold. A running total while it is open.
    expected: TillCash,
    /// What was counted. Absent while it is still open.
    declared: Option<TillCash>,
    /// **The number that gets read.** Negative is short.
    variance: Option<TillCash>,
    sales_count: u32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    opened_at: Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    closed_at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct TakingRecord {
    method: String,
    taken: TillCash,
    refunded: TillCash,
}

#[derive(Debug, Serialize, ToSchema)]
struct Takings {
    items: Vec<TakingRecord>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PosAccepted {
    id: String,
    position: Option<i64>,
}

/// What a till hands the customer.
#[derive(Debug, Serialize, ToSchema)]
struct SaleRung {
    /// The `sales` invoice this became. Read it there for the lines and the QR.
    sale: String,
    /// **The statutory number**, from a gapless series. What the receipt prints.
    number: String,
    total: TillCash,
    position: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct TillAccounts {
    /// The drawer. `1000` in every shipped chart.
    cash: String,
    /// Where card and transfer land. `1010`.
    bank: String,
    /// Where a shortage or an overage lands. `5910`.
    over_short: String,
}

#[derive(Debug, Deserialize)]
struct ShiftQuery {
    #[serde(flatten)]
    page: After,
    /// Only this counter's.
    till: Option<String>,
    /// Only the ones still taking money.
    #[serde(default)]
    open: bool,
}

// ---------------------------------------------------------------------------
// Shifts
// ---------------------------------------------------------------------------

/// Shifts, newest first.
#[utoipa::path(
    get,
    path = "/v1/pos/shifts",
    tag = "pos",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("till" = Option<String>, Query, description = "Only this counter's."),
        ("open" = Option<bool>, Query, description = "Only the ones still taking money."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<ShiftRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn list_shifts(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<ShiftQuery>,
) -> Result<Json<Paged<ShiftRecord>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::shifts(
        &mut conn,
        query.till.as_deref(),
        query.open,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, record)))
}

/// Open a till.
#[utoipa::path(
    post,
    path = "/v1/pos/shifts",
    tag = "pos",
    request_body = NewShift,
    responses(
        (status = CREATED, body = PosAccepted),
        (status = BAD_REQUEST, description = "A negative float", body = Problem),
        (status = CONFLICT, description = "That key already opened a different shift", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn open_shift(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewShift>,
) -> Result<(StatusCode, Json<PosAccepted>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = key.id().clone();

    let opening = Opening {
        till: body.till,
        // **Whoever is signed in**, and not a field on the request: a till that
        // lets the operator be typed in lets it be typed in wrong.
        operator: tenant.session.identity.to_string(),
        float: amount(&body.float, locale)?,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::open_shift(&tenant.db, &id, &opening, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(PosAccepted {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/pos/shifts/{shift}",
    tag = "pos",
    params(("shift" = String, Path, description = "The key it was opened under.")),
    responses(
        (status = OK, body = ShiftRecord),
        (status = NOT_FOUND, description = "No such shift, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_shift(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<ShiftRecord>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::shift(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|found| Json(record(found)))
        .ok_or_else(|| missing(crate::messages::NO_SUCH_SHIFT, &id, locale))
}

/// What it took, by how the money arrived.
#[utoipa::path(
    get,
    path = "/v1/pos/shifts/{shift}/takings",
    tag = "pos",
    params(("shift" = String, Path, description = "The key it was opened under.")),
    responses(
        (status = OK, body = Takings),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn shift_takings(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<Takings>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let rows = crate::takings(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(Takings {
        items: rows
            .into_iter()
            .map(|t| TakingRecord {
                method: t.method,
                taken: money(t.taken),
                refunded: money(t.refunded),
            })
            .collect(),
    }))
}

/// Ring a sale: the invoice, its payment and the drawer, in one write.
#[utoipa::path(
    post,
    path = "/v1/pos/shifts/{shift}/sales",
    tag = "pos",
    params(("shift" = String, Path, description = "The key it was opened under.")),
    request_body = NewSale,
    responses(
        (status = CREATED, body = SaleRung),
        (status = BAD_REQUEST, description = "Nothing on the sale, or a value that did not parse", body = Problem),
        (status = NOT_FOUND, description = "No such shift", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The till is shut, the tenders do not come to the sale, or the ledger refused it", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn ring_sale(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(shift): Path<String>,
    key: IdempotencyKey,
    Json(body): Json<NewSale>,
) -> Result<(StatusCode, Json<SaleRung>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let shift = parse_id(&shift, locale)?;
    let sale = key.id().clone();

    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let basket = Basket {
        customer: customer(&body.customer, locale)?,
        lines: lines(&body.lines, currency, locale)?,
        discounts: discounts(&body.discounts, currency, locale)?,
        currency,
        tenders: tenders(&body.tenders, locale)?,
        note: body.note,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let rung = crate::sell(&tenant.db, &shift, &sale, &basket, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(SaleRung {
            sale: sale.to_string(),
            number: rung.number,
            total: money(rung.total),
            position: rung.committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// Take cash out of the drawer for something that is not a refund.
#[utoipa::path(
    post,
    path = "/v1/pos/shifts/{shift}/pay-outs",
    tag = "pos",
    params(("shift" = String, Path, description = "The key it was opened under.")),
    request_body = NewPayOut,
    responses(
        (status = OK, body = PosAccepted),
        (status = NOT_FOUND, description = "No such shift", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The till is shut, or the ledger refused it", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn pay_out(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(shift): Path<String>,
    Json(body): Json<NewPayOut>,
) -> Result<Json<PosAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = parse_id(&shift, locale)?;

    let payment = PayOut {
        reference: body.reference,
        amount: amount(&body.amount, locale)?,
        to: parse_id(&body.to, locale)?,
        why: body.why,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::pay_out(&tenant.db, &id, &payment, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(PosAccepted {
        id: shift,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Count the drawer and shut the till.
#[utoipa::path(
    post,
    path = "/v1/pos/shifts/{shift}/count",
    tag = "pos",
    params(("shift" = String, Path, description = "The key it was opened under.")),
    request_body = NewCount,
    responses(
        (status = OK, body = PosAccepted),
        (status = NOT_FOUND, description = "No such shift", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The ledger refused the variance", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn close_shift(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(shift): Path<String>,
    Json(body): Json<NewCount>,
) -> Result<Json<PosAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = parse_id(&shift, locale)?;
    let declared = amount(&body.declared, locale)?;
    let at = body.at.unwrap_or_else(chrono::Utc::now);

    let committed = crate::close_shift(&tenant.db, &id, declared, at, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(PosAccepted {
        id: shift,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Where the drawer posts.
#[utoipa::path(
    get,
    path = "/v1/pos/till-accounts",
    tag = "pos",
    responses(
        (status = OK, body = TillAccounts),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn till_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<TillAccounts>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let accounts = crate::PostingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| config(&e, locale))?;

    Ok(Json(TillAccounts {
        cash: accounts.cash.to_string(),
        bank: accounts.bank.to_string(),
        over_short: accounts.over_short.to_string(),
    }))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/pos/till-accounts",
    tag = "pos",
    request_body = TillAccounts,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "Not an account code", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn set_till_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<TillAccounts>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let accounts = crate::PostingAccounts {
        cash: parse_id(&body.cash, locale)?,
        bank: parse_id(&body.bank, locale)?,
        over_short: parse_id(&body.over_short, locale)?,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn amount(sent: &Amount, locale: Locale) -> Result<Money, Problem> {
    let currency = CurrencyCode::new(&sent.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &sent.currency,
            locale,
        )
    })?;
    Ok(Money::from_minor(sent.minor, currency))
}

fn money(value: Money) -> TillCash {
    TillCash {
        minor: value.minor(),
        currency: value.currency().as_str().to_owned(),
    }
}

fn tenders(sent: &[NewTender], locale: Locale) -> Result<Vec<Tender>, Problem> {
    sent.iter()
        .map(|t| {
            let method: Method = t.method.parse().map_err(|e: crate::UnknownMethod| {
                bad_request(crate::messages::UNKNOWN_METHOD, "method", &e.0, locale)
            })?;
            Ok(Tender::new(method, amount(&t.amount, locale)?))
        })
        .collect()
}

fn lines(
    sent: &[NewLine],
    currency: CurrencyCode,
    locale: Locale,
) -> Result<Vec<sales::DraftLine>, Problem> {
    sent.iter()
        .map(|line| {
            Ok(sales::DraftLine {
                description: line.description.clone(),
                net: Money::from_minor(line.net, currency),
                category: category(&line.vat, locale)?,
            })
        })
        .collect()
}

fn discounts(
    sent: &[SaleDiscount],
    currency: CurrencyCode,
    locale: Locale,
) -> Result<Vec<sales::DraftDiscount>, Problem> {
    sent.iter()
        .map(|d| {
            Ok(sales::DraftDiscount {
                amount: Money::from_minor(d.amount, currency),
                reason: d.reason.clone(),
                category: category(&d.vat, locale)?,
            })
        })
        .collect()
}

fn category(raw: &str, locale: Locale) -> Result<ledger::VatCategory, Problem> {
    raw.parse()
        .map_err(|_| bad_request(erp_web::messages::UNKNOWN_VAT_CATEGORY, "vat", raw, locale))
}

fn customer(sent: &SaleCustomer, locale: Locale) -> Result<sales::Customer, Problem> {
    let mut customer = sales::Customer::new(sent.name.clone());
    if let Some(id) = &sent.id {
        customer.id = Some(parse_id(id, locale)?);
    }
    if let Some(vat) = &sent.vat_number {
        customer = customer.with_vat_number(vat.clone());
    }
    Ok(customer)
}

fn record(s: crate::ShiftSummary) -> ShiftRecord {
    ShiftRecord {
        id: s.id,
        till: s.till,
        operator: s.operator,
        float: money(s.float),
        expected: money(s.expected),
        declared: s.declared.map(money),
        variance: s.variance.map(money),
        sales_count: s.sales_count,
        opened_at: s.opened_at,
        closed_at: s.closed_at,
    }
}

fn missing(code: erp_i18n::MessageCode, id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(code).with("id", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<PosError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                PosError::NoSuchShift(_) => StatusCode::NOT_FOUND,

                // Well-formed, and refused on the state of the world.
                PosError::Closed(_)
                | PosError::TendersDoNotMatch { .. }
                | PosError::Ledger(_)
                | PosError::Sale(_)
                | PosError::Unbalanced(_) => StatusCode::UNPROCESSABLE_ENTITY,

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

        CommandError::Execute(ExecuteError::AlreadyExists { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::ALREADY_EXISTS),
        ),

        other => {
            tracing::error!(error = %other, "pos command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    let status = match error {
        erp_tenant::PoolError::Overloaded { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn config(error: &erp_eventlog::ConfigError, locale: Locale) -> Problem {
    tracing::error!(error = %error, "pos configuration failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &error.message(),
        locale,
        &CATALOG,
    )
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(error = %error, "pos read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
