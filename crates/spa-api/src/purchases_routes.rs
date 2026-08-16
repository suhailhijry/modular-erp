//! The purchases module's HTTP surface.
//!
//! Translation only, like `ledger_routes` and `sales_routes`. The third one of
//! these confirms what the second suggested: a module's route layer is a name, a
//! set of wire shapes, a module gate, and a rejection-to-status mapping — and
//! the only part that resists being shared is the mapping, because *which
//! rejection is a 409 and which is a 422* is exactly the part a shared helper
//! could not decide.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use ledger::VatCategory;
use purchases::{Draft, Payment, PurchaseError, Supplier};
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::ExecuteError;
use spa_i18n::{Locale, Localize};
use spa_types::{CurrencyCode, Timestamp};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::consistency::{Consistency, nudge};
use crate::error::ApiError;
use crate::extract::{Allowed, Language, PostEntries, Read};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::{Amount, Json, bad_request, metadata, parse_id, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_bills, record_bill))
        .routes(routes!(get_bill))
        .routes(routes!(pay_bill))
}

/// How many bills a list returns. ponytail: no cursor, same as sales.
const PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "ap-2026-0042",
    "supplier": { "name": "Najd Supplies", "vat_number": "311234567800003" },
    "reference": "NS-8891",
    "billed_on": "2026-02-03T00:00:00Z",
    "due_on": "2026-03-05T00:00:00Z",
    "currency": "SAR",
    "lines": [
        { "description": "Office rent, February", "account": "5100",
          "net": 1_200_000, "vat": "standard", "vat_rate": 1500, "tax": 180_000 }
    ]
}))]
struct NewBill {
    /// **Your own key for this bill.** Recording the same one twice is a no-op,
    /// which is what makes a retried request safe.
    ///
    /// Not the supplier's number — that is `reference`, and two suppliers can
    /// both call something `INV-001`.
    id: String,
    supplier: NewSupplier,
    /// **The supplier's own invoice number.** What a reclaim is evidenced by.
    /// Recording the same one twice for the same supplier is refused: it would
    /// be a duplicate claim.
    reference: String,
    /// The tax point, from their document. Not when you typed it in.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    billed_on: Timestamp,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    due_on: Option<Timestamp>,
    currency: String,
    lines: Vec<NewBillLine>,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewSupplier {
    name: String,
    /// Their VAT registration number. **Required if any line carries tax** —
    /// input VAT is reclaimed against a registered supplier's tax invoice, and a
    /// bill without one is not evidence of a reclaim.
    #[serde(default)]
    vat_number: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewBillLine {
    description: String,
    /// The expense or asset account this lands in. One bill routinely covers
    /// several.
    account: String,
    /// Minor units, excluding tax.
    net: i64,
    /// `standard`, `zero` or `exempt`, as the supplier treated it.
    vat: String,
    /// The rate they charged, in basis points. 1500 is 15%.
    ///
    /// Recorded rather than resolved. If it disagrees with today's statutory
    /// rate that is a thing worth being able to see, not a thing to correct
    /// silently.
    vat_rate: i32,
    /// **The tax the supplier charged, in minor units.** Not computed here: a
    /// reclaim is evidenced by their document, so the figure in the books has to
    /// be the figure on it. Must be zero on anything not standard-rated.
    tax: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "reference": "TRF-2210",
    "amount": { "minor": 1_380_000, "currency": "SAR" },
    "paid_on": "2026-03-04T00:00:00Z",
    "account": "1010"
}))]
struct NewBillPayment {
    /// Your own reference — a transfer number, a cheque. Recording the same one
    /// twice against the same bill is a no-op.
    reference: String,
    amount: Amount,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    paid_on: Timestamp,
    /// The cash or bank account it left.
    account: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Recorded {
    id: String,
    /// Where it landed in the log. A client that wants to read its own write
    /// back passes this as `?consistent_after=`.
    position: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BillView {
    id: String,
    supplier: String,
    supplier_vat: Option<String>,
    /// The supplier's own invoice number.
    reference: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    billed_on: Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    due_on: Option<Timestamp>,
    currency: String,
    net: i64,
    tax: i64,
    gross: i64,
    paid: i64,
    outstanding: i64,
    payments: i64,
    note: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct BillDetailView {
    #[serde(flatten)]
    bill: BillView,
    lines: Vec<BillLineView>,
    payments: Vec<BillPaymentView>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BillLineView {
    description: String,
    account: String,
    net: i64,
    vat: &'static str,
    vat_rate: i32,
    tax: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct BillPaymentView {
    reference: String,
    amount: i64,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    paid_on: Timestamp,
    account: String,
}

fn view(summary: purchases::BillSummary) -> BillView {
    BillView {
        id: summary.id,
        supplier: summary.supplier,
        supplier_vat: summary.supplier_vat,
        reference: summary.reference,
        billed_on: summary.billed_on,
        due_on: summary.due_on,
        currency: summary.gross.currency().to_string(),
        net: summary.net.minor(),
        tax: summary.tax.minor(),
        gross: summary.gross.minor(),
        paid: summary.paid.minor(),
        outstanding: summary.outstanding.minor(),
        payments: summary.payments,
        note: summary.note,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Record a supplier's bill, and post it to the ledger.
///
/// One transaction: the bill and its journal entry either both happen or neither
/// does. The tax is taken **as the supplier stated it** — see `tax` on a line.
#[utoipa::path(
    post,
    path = "/v1/purchases/bills",
    tag = "purchases",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = NewBill,
    responses(
        (status = CREATED, description = "Recorded, or already recorded under this key.", body = Recorded),
        (status = BAD_REQUEST, description = "No lines, mixed currencies, negative tax, tax on an untaxed line, or tax without the supplier's VAT number", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the purchases module is not enabled here", body = Problem),
        (status = CONFLICT, description = "Sustained contention on this bill. Retryable.", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "An account that does not exist or is closed, or a tax point in a closed period", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn record_bill(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewBill>,
) -> Result<(StatusCode, Json<Recorded>), Problem> {
    require_module(&tenant, &purchases::module_id(), locale)?;

    let id = parse_id(&body.id, locale)?;
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let mut lines = Vec::with_capacity(body.lines.len());
    for line in body.lines {
        let category: VatCategory = line.vat.parse().map_err(|_| {
            bad_request(
                crate::messages::UNKNOWN_VAT_CATEGORY,
                "vat",
                &line.vat,
                locale,
            )
        })?;
        lines.push(purchases::BillLine {
            description: line.description,
            account: parse_id(&line.account, locale)?,
            net: spa_types::Money::from_minor(line.net, currency),
            category,
            rate_bp: line.vat_rate,
            tax: spa_types::Money::from_minor(line.tax, currency),
        });
    }

    let mut supplier = Supplier::new(body.supplier.name);
    if let Some(number) = body.supplier.vat_number {
        supplier = supplier.with_vat_number(number);
    }

    let draft = Draft {
        supplier,
        supplier_reference: body.reference,
        billed_on: body.billed_on,
        due_on: body.due_on,
        currency,
        lines,
        note: body.note,
    };

    let committed = purchases::record_bill(&tenant.db, &id, &draft, &metadata(&tenant))
        .await
        .map_err(|e| purchase_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(Recorded {
            id: body.id,
            position: committed.at.map(spa_types::LogPosition::get),
        }),
    ))
}

/// Pay a supplier against a bill.
#[utoipa::path(
    post,
    path = "/v1/purchases/bills/{bill}/payments",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("bill" = String, Path, description = "Your key for the bill."),
    ),
    request_body = NewBillPayment,
    responses(
        (status = OK, description = "Recorded, or already recorded under this reference.", body = Recorded),
        (status = BAD_REQUEST, description = "A non-positive amount, or an unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "More than is outstanding — read the bill again and decide", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such bill, or a payment date in a closed period", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn pay_bill(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewBillPayment>,
) -> Result<Json<Recorded>, Problem> {
    require_module(&tenant, &purchases::module_id(), locale)?;

    let raw = params.get("bill").map_or("", String::as_str);
    let bill = parse_id(raw, locale)?;
    let account = parse_id(&body.account, locale)?;

    let committed = purchases::pay_bill(
        &tenant.db,
        &bill,
        &Payment {
            reference: body.reference,
            amount: body.amount.parse(locale)?,
            paid_on: body.paid_on,
            from: account,
        },
        &metadata(&tenant),
    )
    .await
    .map_err(|e| purchase_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(Recorded {
        id: raw.to_owned(),
        position: committed.at.map(spa_types::LogPosition::get),
    }))
}

/// Bills, most recently billed first.
#[utoipa::path(
    get,
    path = "/v1/purchases/bills",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = Vec<BillView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The projection did not reach `consistent_after` in time. Retryable.", body = Problem),
    ),
)]
async fn list_bills(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<BillView>>, Problem> {
    require_module(&tenant, &purchases::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, purchases::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let bills = purchases::bills(&mut conn, PAGE)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    Ok(Json(bills.into_iter().map(view).collect()))
}

/// One bill, with its lines and its payments.
#[utoipa::path(
    get,
    path = "/v1/purchases/bills/{bill}",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("bill" = String, Path, description = "Your key for the bill."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = BillDetailView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such bill, no such tenant, or the purchases module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_bill(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<BillDetailView>, Problem> {
    require_module(&tenant, &purchases::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, purchases::GROUP_NAME, locale)
        .await?;

    let id = params.get("bill").map_or("", String::as_str);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let detail = purchases::bill(&mut conn, id)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    let detail = detail.ok_or_else(|| {
        ApiError::NotFound(
            spa_i18n::Message::new(crate::messages::NO_SUCH_BILL)
                .with("bill", spa_i18n::MessageArg::text(id.to_owned())),
        )
        .into_problem(locale)
    })?;

    Ok(Json(BillDetailView {
        lines: detail
            .lines
            .iter()
            .map(|l| BillLineView {
                description: l.description.clone(),
                account: l.account.clone(),
                net: l.net.minor(),
                vat: l.category.as_str(),
                vat_rate: l.basis_points,
                tax: l.tax.minor(),
            })
            .collect(),
        payments: detail
            .payments
            .iter()
            .map(|p| BillPaymentView {
                reference: p.reference.clone(),
                amount: p.amount.minor(),
                paid_on: p.paid_on,
                account: p.account.clone(),
            })
            .collect(),
        bill: view(detail.summary),
    }))
}

// ---------------------------------------------------------------------------

/// Maps a command failure onto a status.
///
/// The third of these, and still its own function. What differs between them is
/// which rejection is a 409 and which is a 422 — the one part of a module's route
/// layer that a shared helper could not decide, and the one part worth reading
/// when a client asks why they got the status they got.
fn purchase_problem(error: &CommandError<PurchaseError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Well-formed, and about something that is not there or not in a
                // state that allows it. A closed period arrives here too, from
                // the ledger.
                PurchaseError::NotRecorded(_) | PurchaseError::Ledger(_) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                // The bill moved on between the client reading it and paying it.
                PurchaseError::Overpayment { .. } => StatusCode::CONFLICT,
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
            tracing::error!(error = %other, "purchases command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                spa_i18n::Message::new(spa_control::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}
