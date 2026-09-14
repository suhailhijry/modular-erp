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
use erp_web::{IfMatch, Versioned, config_problem};
use erp_web::{Json, Query, bad_request, creating, metadata, parse_id, require_module};

use crate::{Basket, Method, Opening, PayOut, PosError, Return, Tender};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_shifts, open_shift))
        .routes(routes!(get_shift))
        .routes(routes!(shift_takings))
        .routes(routes!(ring_sale))
        .routes(routes!(take_back))
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
    ///
    /// **Per unit when `quantity` is given**, and the whole line otherwise —
    /// the till rings a price and says how many.
    net: i64,
    /// The product this rings off the shelf, when the till sells stock.
    /// Optional: the shelf comes down and the cost is booked in the same write
    /// as the sale, and a line without one behaves exactly as before.
    #[serde(default)]
    product: Option<String>,
    /// How many. Optional; one when it is left out.
    #[serde(default)]
    quantity: Option<i64>,
    /// Which units, on a serial-tracked product — one name per unit. A line
    /// that names any must also name the `product` and ring exactly that many.
    #[serde(default)]
    serials: Vec<String>,
    /// Which lot the units come off, when the till overrides the picking rule
    /// — a scanned batch. The lot's `id`; a line that names one must also name
    /// the `product`, and a lot that is not open here or is short is refused.
    #[serde(default)]
    lot: Option<String>,
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
struct ReturnedLine {
    /// Which line of the sale, by position from zero.
    against: u16,
    /// How much of that line is coming back, excluding tax, as a positive
    /// amount. Its tax follows from the line's own rate.
    net: Amount,
    /// **How many units are physically coming back onto the shelf**, when that
    /// line sold stock. Optional, and never worked out from `net`: the money
    /// and the goods are two statements. Leave it out and only the money goes
    /// back.
    #[serde(default)]
    quantity: Option<i64>,
    /// Which units are coming back, on a line that sold serial-tracked stock —
    /// one name per unit, as many as `quantity`, each one this sale took and
    /// that has not come back already. Leave it out and a line of named units
    /// can only come back whole.
    #[serde(default)]
    serials: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewReturn {
    /// Your key. Sending it twice is a no-op.
    reference: String,
    /// What the customer is handed back, and how. Must come to what is being
    /// credited: the whole sale without `lines`, those lines with.
    tenders: Vec<NewTender>,
    /// Which lines are coming back. **Empty is the whole sale.** With lines,
    /// the tenders must come to exactly what those lines credit, tax included.
    #[serde(default)]
    lines: Vec<ReturnedLine>,
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
        (status = BAD_REQUEST, description = "Nothing on the sale, a value that did not parse, or a line of the wrong shape: a quantity that is not one (`sales.not_a_quantity`, `inventory.not_a_quantity`), serials that do not match their line (`sales.named_units`, `inventory.needs_serials`), a lot named with no product (`sales.lot_without_a_product`), or a product id that cannot be one (`inventory.not_a_product_id`)", body = Problem),
        (status = NOT_FOUND, description = "No such shift", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The till is shut, the tenders do not come to the sale, the ledger refused it, or the shelf refuses a line: a product nobody declared (`inventory.no_such_product`), a serial that is not on the shelf (`inventory.no_such_serial`), a named lot that is not open here (`inventory.no_such_lot`) or is short (`inventory.lot_is_short`), or more of a lot- or serial-tracked product than the shelf holds (`inventory.not_enough_stock`)", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, or over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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

    let rung = crate::sell(
        &tenant.db,
        &shift,
        &sale,
        &basket,
        &creating(&tenant, &key),
        sales::Authority::of(&tenant.db),
    )
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

/// Hand a sale back: the money, the credit note and the drawer, in one write.
///
/// The sale is named in the path because the document being credited is the one
/// that was rung, and `reference` is the caller's own key for *this* return —
/// two different things, which is why they are two fields.
#[utoipa::path(
    post,
    path = "/v1/pos/shifts/{shift}/sales/{sale}/returns",
    tag = "pos",
    params(
        ("shift" = String, Path, description = "The key it was opened under."),
        ("sale" = String, Path, description = "The sale being handed back."),
    ),
    request_body = NewReturn,
    responses(
        (status = OK, body = PosAccepted),
        (status = BAD_REQUEST, description = "Nothing handed back, a value that did not parse, or a returned line of the wrong shape: a quantity that is not one (`sales.not_a_quantity`), or names that do not agree with the quantity (`sales.named_units`, `inventory.needs_serials`)", body = Problem),
        (status = NOT_FOUND, description = "No such shift", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The till is shut, the sale is not one that can be credited, the tenders do not come to what the lines credit, the ledger refused it, or the shelf refuses the units coming back (`inventory.not_consumed`, `inventory.more_than_was_taken`, `inventory.named_units_come_back_whole`, `inventory.not_out`)", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, without the `sales:approve_credit_note` claim once the tenant uses claims (`sales.not_approved`), or over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn take_back(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((shift, sale)): Path<(String, String)>,
    Json(body): Json<NewReturn>,
) -> Result<Json<PosAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = parse_id(&shift, locale)?;
    let sale = parse_id(&sale, locale)?;

    let returning = Return {
        reference: body.reference,
        tenders: tenders(&body.tenders, locale)?,
        lines: returned(&body.lines, locale)?,
        why: body.why,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::take_back(
        &tenant.db,
        &id,
        &sale,
        &returning,
        &metadata(&tenant),
        sales::Authority::of(&tenant.db),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(PosAccepted {
        id: shift,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
        (status = OK, body = TillAccounts, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn till_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<TillAccounts>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, crate::PostingAccounts::KEY)
        .await
        .map_err(|e| config(&e, locale))?;
    let accounts = crate::PostingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| config(&e, locale))?;

    Ok(Versioned(
        version,
        TillAccounts {
            cash: accounts.cash.to_string(),
            bank: accounts.bank.to_string(),
            over_short: accounts.over_short.to_string(),
        },
    ))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/pos/till-accounts",
    tag = "pos",
    params(("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally.")),
    request_body = TillAccounts,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = BAD_REQUEST, description = "Not an account code", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable pos", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
    ),
)]
async fn set_till_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<TillAccounts>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
        expected,
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
                // A till rings a price; anything off it is a different price,
                // not an allowance printed beside one.
                allowances: Vec::new(),
                description: line.description.clone(),
                net: Money::from_minor(line.net, currency),
                category: category(&line.vat, locale)?,
                // **Straight through to `sales`.** A till sale is an invoice
                // issued through `sales::issue_in` like any other, so what
                // depletes the shelf is the same code — nothing here decides
                // anything about stock.
                product: line
                    .product
                    .as_deref()
                    .map(|id| parse_id(id, locale))
                    .transpose()?,
                quantity: line.quantity,
                serials: line.serials.clone(),
                lot: line.lot.clone(),
            })
        })
        .collect()
}

/// What a till return says came back, as the credit lines `sales` takes: the
/// money parsed, and how many units and which carried straight through — the
/// till decides nothing about which units are out.
fn returned(sent: &[ReturnedLine], locale: Locale) -> Result<Vec<sales::CreditLine>, Problem> {
    sent.iter()
        .map(|line| {
            Ok(sales::CreditLine {
                against: line.against,
                net: amount(&line.net, locale)?,
                quantity: line.quantity,
                serials: line.serials.clone(),
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
                // The till's operator, over the document limit.
                PosError::Sale(refused) if refused.refuses_the_caller() => StatusCode::FORBIDDEN,
                // **A line of the wrong shape**, as `/v1/sales` answers it: a
                // quantity that is not one, names that do not match, a lot with
                // no product, or a line the shelf cannot read.
                PosError::Sale(refused) if refused.is_malformed() => StatusCode::BAD_REQUEST,

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
    config_problem(error, locale, &CATALOG)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// **A clerk without `sales:approve_credit_note` is refused a return with
    /// the sales screen's own code and status**, since §70 moved that claim
    /// into the credit-note roots the till goes through.
    #[test]
    fn the_credit_note_claim_refuses_the_caller_at_the_till_too() {
        let refused = CommandError::Execute(ExecuteError::Rejected(PosError::Sale(
            sales::SalesError::NotApproved(sales::APPROVE_CREDIT_NOTE.to_owned()),
        )));
        let problem = problem_for(&refused, Locale::English);
        assert_eq!(problem.status, 403);
        assert_eq!(problem.code, "sales.not_approved");
    }

    /// **A till line carries its product, its quantity and its serials into
    /// the draft**, because the till decides nothing about stock: what
    /// depletes a shelf is `sales::issue_in`, and a field dropped in this
    /// translation is a shop that sells the beans and never takes them off.
    /// The `net` stays **one unit's** — `sales` multiplies.
    #[test]
    fn a_till_line_carries_its_product_and_its_units_into_the_draft() {
        let sent: Vec<NewLine> = serde_json::from_value(serde_json::json!([{
            "description": "بن",
            "net": 2_500,
            "vat": "standard",
            "product": "f81d4fae-7dec-11d0-a765-00a0c91e6bf6",
            "quantity": 3,
            "serials": ["A-1", "A-2", "A-3"],
            "lot": "lot.x"
        }]))
        .expect("a line");
        let sar = CurrencyCode::new("SAR").expect("a currency");
        let drafted = lines(&sent, sar, Locale::English).expect("a draft");
        let line = &drafted[0];
        assert_eq!(
            line.product.as_ref().map(erp_types::AggregateId::as_str),
            Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")
        );
        assert_eq!(line.quantity, Some(3));
        assert_eq!(line.serials, ["A-1", "A-2", "A-3"]);
        assert_eq!(line.lot.as_deref(), Some("lot.x"));
        assert_eq!(line.net, Money::from_minor(2_500, sar), "one unit's");
    }

    /// A line without them is what every till line was before stock: a price
    /// rung once, off no shelf.
    #[test]
    fn a_till_line_without_them_is_a_bare_price() {
        let sent: Vec<NewLine> = serde_json::from_value(serde_json::json!([
            { "description": "قهوة", "net": 1_500, "vat": "standard" }
        ]))
        .expect("a line");
        let sar = CurrencyCode::new("SAR").expect("a currency");
        let drafted = lines(&sent, sar, Locale::English).expect("a draft");
        assert_eq!(drafted[0].product, None);
        assert_eq!(drafted[0].quantity, None);
        assert!(drafted[0].serials.is_empty());
        assert_eq!(drafted[0].lot, None);
    }

    /// **A till return carries which units came back**, not just how many:
    /// `sales` and `inventory` decide whether those names were sold, and a
    /// field dropped here is a phone back in the drawer and not on the shelf.
    #[test]
    fn a_till_return_carries_its_units_to_the_credit_line() {
        let sent: Vec<ReturnedLine> = serde_json::from_value(serde_json::json!([{
            "against": 0,
            "net": { "minor": 50_000, "currency": "SAR" },
            "quantity": 1,
            "serials": ["SN-2"]
        }]))
        .expect("a line");
        let credited = returned(&sent, Locale::English).expect("parses");
        assert_eq!(credited[0].quantity, Some(1));
        assert_eq!(credited[0].serials, ["SN-2"]);
    }

    /// **A till line whose units do not match it is a 400**, the status
    /// `/v1/sales/invoices` answers the same refusal with — not the 422 a
    /// refusal on the state of the shelf gets.
    #[test]
    fn a_malformed_stock_line_is_a_bad_request_at_the_till_too() {
        let malformed = CommandError::Execute(ExecuteError::Rejected(PosError::Sale(
            sales::SalesError::Stock(inventory::InventoryError::NeedsSerials {
                units: 3,
                named: 1,
            }),
        )));
        let problem = problem_for(&malformed, Locale::English);
        assert_eq!(problem.status, 400);
        assert_eq!(problem.code, "inventory.needs_serials");

        let short = CommandError::Execute(ExecuteError::Rejected(PosError::Sale(
            sales::SalesError::Stock(inventory::InventoryError::NotEnoughStock {
                held: 0,
                wanted: 3,
            }),
        )));
        assert_eq!(problem_for(&short, Locale::English).status, 422);
    }

    /// **A sales line of the wrong shape is a 400 at the till too** — §76's
    /// review found `sales.lot_without_a_product`, `sales.named_units` and
    /// `sales.not_a_quantity` answered 400 by `/v1/sales` and 422 here, while
    /// this route's own description said 400.
    #[test]
    fn a_malformed_sales_line_is_a_bad_request_at_the_till_too() {
        for (refused, code) in [
            (
                sales::SalesError::LotWithoutAProduct {
                    lot: "lot.x".to_owned(),
                },
                "sales.lot_without_a_product",
            ),
            (
                sales::SalesError::NamedUnits { named: 1 },
                "sales.named_units",
            ),
            (sales::SalesError::NotAQuantity, "sales.not_a_quantity"),
        ] {
            let malformed = CommandError::Execute(ExecuteError::Rejected(PosError::Sale(refused)));
            let problem = problem_for(&malformed, Locale::English);
            assert_eq!(problem.code, code);
            assert_eq!(problem.status, 400, "{code}");
        }
    }

    /// **The till's operator over the document limit is a 403**, the status
    /// the sales routes and the booking desk answer the same refusal with —
    /// not the 422 a sales refusal on the state of the world gets at the till.
    #[test]
    fn the_document_limit_refuses_the_caller_at_the_till_too() {
        let sar = CurrencyCode::new("SAR").expect("a currency");
        let refused = CommandError::Execute(ExecuteError::Rejected(PosError::Sale(
            sales::SalesError::OverDocumentLimit {
                limit: Money::from_minor(100, sar),
                amount: Money::from_minor(115, sar),
            },
        )));
        let problem = problem_for(&refused, Locale::English);
        assert_eq!(problem.status, 403);
        assert_eq!(problem.code, "sales.over_document_limit");
    }
}
