//! The purchases module's HTTP surface.
//!
//! Translation only, like every module's — see [`ledger::http`] for why these
//! live in the module rather than in the composition root.
//!
//! What the three of them have in common is a set of wire shapes, a module gate,
//! and a rejection-to-status mapping; the only part that resists being shared is
//! the mapping, because *which rejection is a 409 and which is a 422* is exactly
//! the part a shared helper could not decide.

use crate::{Draft, Payment, PurchaseError, Supplier};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::{CurrencyCode, Timestamp};
use ledger::VatCategory;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{
    After, Amount, Json, Paged, Query, bad_request, creating, metadata, parse_id, require_module,
};
use erp_web::{Allowed, IdempotencyKey, Language, PostEntries, Read};
use erp_web::{Consistency, nudge};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_bills, record_bill))
        .routes(routes!(get_bill))
        .routes(routes!(pay_bill))
}

/// How many bills a page returns by default, and the most it will give. Same
/// shape as sales.
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
static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &ledger::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
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
struct BillPaymentRecorded {
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

fn view(summary: crate::BillSummary) -> BillView {
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = NewBill,
    responses(
        (status = CREATED, description = "BillPaymentRecorded, or already recorded under this key.", body = BillPaymentRecorded),
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
    key: IdempotencyKey,
    Json(body): Json<NewBill>,
) -> Result<(StatusCode, Json<BillPaymentRecorded>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let id = key.id().clone();
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let mut lines = Vec::with_capacity(body.lines.len());
    for line in body.lines {
        let category: VatCategory = line.vat.parse().map_err(|_| {
            bad_request(
                erp_web::messages::UNKNOWN_VAT_CATEGORY,
                "vat",
                &line.vat,
                locale,
            )
        })?;
        lines.push(crate::BillLine {
            description: line.description,
            account: parse_id(&line.account, locale)?,
            net: erp_types::Money::from_minor(line.net, currency),
            category,
            rate_bp: line.vat_rate,
            tax: erp_types::Money::from_minor(line.tax, currency),
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

    let committed = crate::record_bill(&tenant.db, &id, &draft, &creating(&tenant, &key))
        .await
        .map_err(|e| purchase_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(BillPaymentRecorded {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// Pay a supplier against a bill.
#[utoipa::path(
    post,
    path = "/v1/purchases/bills/{bill}/payments",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("bill" = String, Path, description = "Your key for the bill."),
    ),
    request_body = NewBillPayment,
    responses(
        (status = OK, description = "BillPaymentRecorded, or already recorded under this reference.", body = BillPaymentRecorded),
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
) -> Result<Json<BillPaymentRecorded>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let raw = params.get("bill").map_or("", String::as_str);
    let bill = parse_id(raw, locale)?;
    let account = parse_id(&body.account, locale)?;

    let committed = crate::pay_bill(
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

    Ok(Json(BillPaymentRecorded {
        id: raw.to_owned(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Bills, most recently billed first.
#[utoipa::path(
    get,
    path = "/v1/purchases/bills",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = Paged<BillView>),
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
    Query(page): Query<After>,
) -> Result<Json<Paged<BillView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = page.cursor(locale)?;
    let limit = page.limit(PAGE, PAGE);
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let bills = crate::bills(&mut conn, limit, after.as_ref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(Paged::of(bills, view)))
}

/// One bill, with its lines and its payments.
#[utoipa::path(
    get,
    path = "/v1/purchases/bills/{bill}",
    tag = "purchases",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let id = params.get("bill").map_or("", String::as_str);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let detail = crate::bill(&mut conn, id)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let detail = detail.ok_or_else(|| {
        ApiError::NotFound(
            erp_i18n::Message::new(erp_web::messages::NO_SUCH_BILL)
                .with("bill", erp_i18n::MessageArg::text(id.to_owned())),
        )
        .into_problem(locale, &CATALOG)
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
            tracing::error!(error = %other, "purchases command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}
