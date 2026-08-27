//! The sales module's HTTP surface.
//!
//! Translation only, like every module's — see [`ledger::http`] for why these
//! live in the module rather than in the composition root.

use crate::{Customer, Draft, DraftLine, Receipt, SalesError, VatCategory};
use axum::extract::{Path, State};
use axum::http::StatusCode;
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
use erp_web::{After, Amount, Json, Paged, Query, bad_request, metadata, parse_id, require_module};
use erp_web::{Allowed, Language, ManageAccounts, PostEntries, Read};
use erp_web::{Consistency, nudge};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_invoices, issue_invoice))
        .routes(routes!(get_invoice))
        .routes(routes!(receivables))
        .routes(routes!(record_payment))
        .routes(routes!(credit_note))
        // Typed on purpose. The store underneath is key-value; this is not, so
        // a value that reaches it has already been through the type that gives
        // it meaning. See `erp_eventlog::config`.
        .routes(routes!(posting_accounts, set_posting_accounts))
}

/// How many invoices a page returns when the caller does not say, and the most
/// it will give when they ask for more. Paged from there — see [`erp_web::After`].
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
    "id": "INV-2026-0001",
    "customer": { "name": "Al-Faisal Trading", "vat_number": "310000000000003" },
    "issued_on": "2026-08-15T00:00:00Z",
    "due_on": "2026-09-14T00:00:00Z",
    "currency": "SAR",
    "lines": [
        { "description": "Consulting, August", "net": 500_000, "vat": "standard" }
    ],
    "note": ""
}))]
struct NewInvoice {
    /// **Your own key for this invoice, not its number.** Issuing the same one
    /// twice is a no-op, which is what makes a retried request safe.
    ///
    /// The invoice *number* is allocated here, from a gapless statutory series,
    /// and comes back as `number`. Saudi law requires that sequence to have no
    /// holes in it, which is not something a client can guarantee.
    id: String,
    /// Copied onto the invoice as values, never as a reference. A tax invoice
    /// is a legal document; last year's copy must not change when a customer
    /// record does.
    customer: NewCustomer,
    /// The tax point. A date the business chose, not a clock reading.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    issued_on: Timestamp,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    due_on: Option<Timestamp>,
    /// ISO 4217. Every line is in this currency.
    currency: String,
    /// At least one line that comes to something.
    lines: Vec<NewLine>,
    /// What comes off the whole invoice, printed as its own figure rather than
    /// folded into a smaller total.
    ///
    /// **The tax comes off with it.** A 15 discount on a standard-rated invoice
    /// reduces the taxable amount by 15 and the tax by 2.25 — which is why a
    /// discount is not a negative line, and why it has to say which treatment
    /// it comes off.
    #[serde(default)]
    discounts: Vec<NewDiscount>,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewDiscount {
    /// Why, printed on the invoice — ZATCA shows it to the customer.
    reason: String,
    /// Minor units, **positive**: what comes off. A negative one is a charge,
    /// which this system does not issue.
    amount: i64,
    /// Which treatment it comes off: `standard`, `zero` or `exempt`.
    ///
    /// **A discount reduces the tax only on what carried any.** Discounting the
    /// standard-rated part of a mixed invoice is a different number from
    /// discounting the exempt part, so the invoice has to say which.
    vat: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewCustomer {
    name: String,
    /// The buyer's VAT registration number, printed on the invoice.
    ///
    /// **Giving one makes this a standard invoice**, which ZATCA clears before
    /// the buyer may be given it. Leaving it out makes it a simplified one,
    /// reported within twenty-four hours.
    #[serde(default)]
    vat_number: Option<String>,
    /// Where they are, as they are today. Snapshotted onto the invoice, so a
    /// customer moving does not rewrite what was already issued.
    ///
    /// ZATCA wants street, city and country on a standard invoice; without them
    /// it accepts the document and warns, which is a warning that becomes a
    /// finding at an inspection.
    #[serde(default)]
    address: Option<NewAddress>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewAddress {
    street: String,
    city: String,
    /// ISO 3166-1 alpha-2. `SA` for a Saudi buyer.
    country: String,
    #[serde(default)]
    district: Option<String>,
    #[serde(default)]
    building: Option<String>,
    #[serde(default)]
    postal_code: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewLine {
    description: String,
    /// Minor units, in the invoice's currency. Excluding tax.
    net: i64,
    /// `standard`, `zero` or `exempt`. The *rate* is not a client's to choose —
    /// it is statutory, and resolved here. Zero-rated and exempt are both 0%
    /// and mean different things on a return, so both are kept.
    vat: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "reference": "BANK-88231",
    "amount": { "minor": 575_000, "currency": "SAR" },
    "received_on": "2026-08-20T00:00:00Z",
    "account": "1000"
}))]
struct NewPayment {
    /// The payer's or the bank's reference. Recording the same one twice against
    /// the same invoice is a no-op.
    reference: String,
    amount: Amount,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    received_on: Timestamp,
    /// The cash or bank account it landed in.
    account: String,
}

/// A statutory document that now exists.
#[derive(Debug, Serialize, ToSchema)]
struct Issued {
    /// The key you sent, which is what addresses this document from here on.
    id: String,
    /// The statutory number, allocated here from the tenant's gapless series.
    /// What goes on the printed document.
    ///
    /// On a repeated request this is the number the document already has, not a
    /// new one — the series does not move for a retry.
    number: String,
    /// Where it landed in the log. A client that wants to read its own write
    /// back passes this as `?consistent_after=`.
    position: Option<i64>,
}

/// Something recorded against a document, which is not a document itself.
///
/// A payment carries no statutory number: the invoice it settles is the numbered
/// thing, and a receipt references that.
#[derive(Debug, Serialize, ToSchema)]
struct Recorded {
    /// The invoice it was recorded against.
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "CN-2026-0001", "reason": "Returned in full", "on": "2026-08-25T00:00:00Z"
}))]
struct NewCreditNote {
    /// **Your own key for this cancellation, not the credit note's number.**
    /// Sending the same one twice is a no-op; a different one against an invoice
    /// that is already credited is refused.
    ///
    /// The credit note's number is allocated here, from its own gapless series —
    /// ZATCA numbers credit notes separately from the invoices they credit.
    id: String,
    #[serde(default)]
    reason: String,
    /// When the credit is treated as happening. Usually today, not the date of
    /// the invoice.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    on: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct InvoiceView {
    /// The key the client sent when issuing, and what addresses this invoice.
    id: String,
    /// The statutory number. Sequential, gapless, and what the document prints.
    number: String,
    /// Set once a credit note has cancelled it. A cancelled invoice owes
    /// nothing, and `outstanding` says so too.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    cancelled_on: Option<Timestamp>,
    credit_note: Option<String>,
    customer: String,
    customer_vat: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    issued_on: Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    due_on: Option<Timestamp>,
    currency: String,
    net: i64,
    tax: i64,
    gross: i64,
    paid: i64,
    outstanding: i64,
    /// How many payments have been recorded. The payments themselves are on
    /// `GET /v1/sales/invoices/{invoice}`.
    ///
    /// Not `payments`: the detail view flattens this one and adds the list, and
    /// two shapes under one name on the same resource is a client generator's
    /// worst afternoon.
    payment_count: i64,
    note: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct InvoiceDetailView {
    #[serde(flatten)]
    invoice: InvoiceView,
    lines: Vec<LineView>,
    /// One row per rate, which is what a Saudi tax invoice has to print.
    tax_breakdown: Vec<TaxView>,
    payments: Vec<PaymentView>,
}

#[derive(Debug, Serialize, ToSchema)]
struct LineView {
    description: String,
    net: i64,
    vat: &'static str,
    /// Basis points — 1500 is 15%. The rate that applied when it was issued, not
    /// today's.
    vat_rate: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct TaxView {
    vat: &'static str,
    vat_rate: i32,
    net: i64,
    tax: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct PaymentView {
    reference: String,
    amount: i64,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    received_on: Timestamp,
    account: String,
}

fn view(summary: crate::InvoiceSummary) -> InvoiceView {
    InvoiceView {
        id: summary.id,
        number: summary.number,
        cancelled_on: summary.cancelled_on,
        credit_note: summary.credit_note,
        customer: summary.customer,
        customer_vat: summary.customer_vat,
        issued_on: summary.issued_on,
        due_on: summary.due_on,
        currency: summary.gross.currency().to_string(),
        net: summary.net.minor(),
        tax: summary.tax.minor(),
        gross: summary.gross.minor(),
        paid: summary.paid.minor(),
        outstanding: summary.outstanding.minor(),
        payment_count: summary.payments,
        note: summary.note,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Issue a tax invoice, and post it to the ledger.
///
/// One transaction: the invoice and its journal entry either both happen or
/// neither does. `id` is the invoice number the tenant chose, and issuing the
/// same one twice is a no-op — which is what makes a retried request safe.
///
/// Tax is charged once per rate band on the band's subtotal, not line by line,
/// which is the breakdown a Saudi invoice has to print.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices",
    tag = "sales",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = NewInvoice,
    responses(
        (status = CREATED, description = "Issued, or already issued under this key.", body = Issued),
        (status = BAD_REQUEST, description = "No lines that come to anything, mixed currencies, an unknown VAT category, or an unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the sales module is not enabled here", body = Problem),
        (status = CONFLICT, description = "Sustained contention on this invoice. Retryable.", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The posting accounts are missing or closed", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn issue_invoice(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewInvoice>,
) -> Result<(StatusCode, Json<Issued>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;

    let id = parse_id(&body.id, locale)?;
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
        lines.push(DraftLine {
            description: line.description,
            net: erp_types::Money::from_minor(line.net, currency),
            category,
        });
    }

    let mut discounts = Vec::with_capacity(body.discounts.len());
    for discount in body.discounts {
        let category: VatCategory = discount.vat.parse().map_err(|_| {
            bad_request(
                erp_web::messages::UNKNOWN_VAT_CATEGORY,
                "vat",
                &discount.vat,
                locale,
            )
        })?;
        discounts.push(crate::DraftDiscount {
            reason: discount.reason.trim().to_owned(),
            amount: erp_types::Money::from_minor(discount.amount, currency),
            category,
        });
    }

    let mut customer = Customer::new(body.customer.name);
    if let Some(number) = body.customer.vat_number {
        customer = customer.with_vat_number(number);
    }
    if let Some(address) = body.customer.address {
        customer = customer.at(crate::Address {
            street: address.street.trim().to_owned(),
            city: address.city.trim().to_owned(),
            country: address.country.trim().to_uppercase(),
            district: address.district.filter(|v| !v.trim().is_empty()),
            building: address.building.filter(|v| !v.trim().is_empty()),
            postal_code: address.postal_code.filter(|v| !v.trim().is_empty()),
        });
    }

    let draft = Draft {
        customer,
        issued_on: body.issued_on,
        due_on: body.due_on,
        currency,
        lines,
        discounts,
        note: body.note,
    };

    let committed = crate::issue_invoice(&tenant.db, &id, &draft, &metadata(&tenant))
        .await
        .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(Issued {
            id: body.id,
            number: committed.number,
            position: committed.committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// Record a payment against an invoice.
///
/// `reference` is the payer's or the bank's; recording the same one twice
/// against the same invoice is a no-op. Paying more than is outstanding is
/// refused rather than left as a negative balance.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices/{invoice}/payments",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice number."),
    ),
    request_body = NewPayment,
    responses(
        (status = OK, description = "Recorded, or already recorded under this reference.", body = Recorded),
        (status = BAD_REQUEST, description = "A non-positive amount, or an unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "More than is outstanding — read the invoice again and decide", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, or one that was never issued", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn record_payment(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewPayment>,
) -> Result<Json<Recorded>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;
    let account = parse_id(&body.account, locale)?;

    let committed = crate::record_payment(
        &tenant.db,
        &invoice,
        &Receipt {
            reference: body.reference,
            amount: body.amount.parse(locale)?,
            received_on: body.received_on,
            into: account,
        },
        &metadata(&tenant),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(Recorded {
        id: raw.to_owned(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Cancels an invoice by crediting it.
///
/// A `POST`, not a `DELETE`: the invoice stays, its journal entry is reversed,
/// and the books show both.
///
/// Cancels the whole invoice. ponytail: partial credit notes are not built —
/// they need the credit note to be a document with its own tax point.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices/{invoice}/credit-note",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice number being credited."),
    ),
    request_body = NewCreditNote,
    responses(
        (status = OK, description = "Credited, or already credited under this key.", body = Issued),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Already cancelled by a *different* credit note", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, or one with payments against it — refund those first", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn credit_note(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewCreditNote>,
) -> Result<Json<Issued>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;

    let committed = crate::cancel_invoice(
        &tenant.db,
        &invoice,
        &body.id,
        &body.reason,
        body.on,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(Issued {
        id: body.id,
        number: committed.number,
        position: committed.committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Invoices, most recently issued first.
///
/// Paged. `next` absent means the list ended; pass it back as `?after=`.
#[utoipa::path(
    get,
    path = "/v1/sales/invoices",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = Paged<InvoiceView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The projection did not reach `consistent_after` in time. Retryable.", body = Problem),
    ),
)]
async fn list_invoices(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(page): Query<After>,
) -> Result<Json<Paged<InvoiceView>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
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

    let invoices = crate::invoices(&mut conn, limit, after.as_ref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(Paged::of(invoices, view)))
}

/// One customer's debt, split by how late it is.
#[derive(Debug, Serialize, ToSchema)]
struct AgedCustomerView {
    /// As invoices name them. There is no customer record, so two spellings are
    /// two rows — see the note on `AgedCustomer`.
    customer: String,
    currency: String,
    /// Owed, but not late yet.
    not_yet_due: i64,
    days_1_30: i64,
    days_31_60: i64,
    days_61_90: i64,
    over_90: i64,
    /// Every bucket together, in this currency.
    total: i64,
    invoices: i64,
    /// The due date of the oldest unpaid invoice. What a collections call opens
    /// with.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    oldest_due: Timestamp,
}

fn aged_view(row: crate::AgedCustomer) -> AgedCustomerView {
    AgedCustomerView {
        customer: row.customer,
        currency: row.currency.as_str().to_owned(),
        not_yet_due: row.not_yet_due.minor(),
        days_1_30: row.days_1_30.minor(),
        days_31_60: row.days_31_60.minor(),
        days_61_90: row.days_61_90.minor(),
        over_90: row.over_90.minor(),
        total: row.total.minor(),
        invoices: row.invoices,
        oldest_due: row.oldest_due,
    }
}

/// `?as_of=` on top of the usual paging.
#[derive(Debug, serde::Deserialize)]
struct AgedQuery {
    #[serde(flatten)]
    page: After,
    /// Only the date part is used. Absent means today.
    #[serde(default)]
    as_of: Option<Timestamp>,
}

/// **Who owes what, and for how long.**
///
/// Aged from the due date, or from the issue date when an invoice carries no
/// terms — an invoice with no due date was due when it was issued, and treating
/// those as "not yet due" for ever is how debts stop being chased.
///
/// Cancelled invoices owe nothing and do not appear.
///
/// Biggest debtor first. A customer trading in two currencies appears once per
/// currency, because adding them would produce a number that is true in neither.
#[utoipa::path(
    get,
    path = "/v1/sales/receivables",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("as_of" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Age the debt as at this date. Absent means today. Closing a period needs the figure as at the closing date, not as at whenever the report was run."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, not refused."),
        ("after" = Option<String>, Query, description = "The `next` cursor from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page, biggest debtor first. `next` is absent when the list ended.", body = Paged<AgedCustomerView>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the sales module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The projection did not reach `consistent_after` in time. Retryable.", body = Problem),
    ),
)]
async fn receivables(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<AgedQuery>,
) -> Result<Json<Paged<AgedCustomerView>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let after = query.page.cursor(locale)?;
    let limit = query.page.limit(PAGE, PAGE);
    // The report's own clock, and the only one in this handler. A read may take
    // the wall clock — it is a projection that may not (L2).
    let as_of = query.as_of.unwrap_or_else(chrono::Utc::now);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let page = crate::receivables(&mut conn, as_of, limit, after.as_ref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(Paged::of(page, aged_view)))
}

/// One invoice, with its lines, its tax breakdown and its payments.
#[utoipa::path(
    get,
    path = "/v1/sales/invoices/{invoice}",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice number."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = InvoiceDetailView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such invoice, no such tenant, or the sales module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_invoice(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<InvoiceDetailView>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let id = params.get("invoice").map_or("", String::as_str);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let detail = crate::invoice(&mut conn, id)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let detail = detail.ok_or_else(|| {
        ApiError::NotFound(
            erp_i18n::Message::new(erp_web::messages::NO_SUCH_INVOICE)
                .with("invoice", erp_i18n::MessageArg::text(id.to_owned())),
        )
        .into_problem(locale, &CATALOG)
    })?;

    Ok(Json(InvoiceDetailView {
        lines: detail
            .lines
            .iter()
            .map(|l| LineView {
                description: l.description.clone(),
                net: l.net.minor(),
                vat: l.category.as_str(),
                vat_rate: l.basis_points,
            })
            .collect(),
        tax_breakdown: detail
            .tax
            .iter()
            .map(|t| TaxView {
                vat: t.category.as_str(),
                vat_rate: t.basis_points,
                net: t.net.minor(),
                tax: t.tax.minor(),
            })
            .collect(),
        payments: detail
            .payments
            .iter()
            .map(|p| PaymentView {
                reference: p.reference.clone(),
                amount: p.amount.minor(),
                received_on: p.received_on,
                account: p.account.clone(),
            })
            .collect(),
        invoice: view(detail.summary),
    }))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "receivable": "1100", "revenue": "4000", "output_vat": "2100" }))]
struct AccountsView {
    /// Debited by what customers owe. Defaults to 1100.
    receivable: String,
    /// Credited by what was earned, excluding tax. Defaults to 4000.
    revenue: String,
    /// Credited by tax charged and owed to ZATCA. Defaults to 2100.
    output_vat: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ConfiguredAccounts {
    #[serde(flatten)]
    accounts: AccountsView,
    /// `false` when nothing has been configured and these are the shipped
    /// defaults — so a settings screen can say "using the standard chart"
    /// rather than implying somebody chose this.
    configured: bool,
}

/// What sales posts to.
///
/// Answers with the shipped defaults when the tenant has never chosen, and says
/// so with `configured: false`.
#[utoipa::path(
    get,
    path = "/v1/sales/posting-accounts",
    tag = "sales",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = ConfiguredAccounts),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn posting_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<ConfiguredAccounts>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let stored = erp_eventlog::configuration::get::<crate::PostingAccounts>(
        &mut conn,
        crate::PostingAccounts::KEY,
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    let configured = stored.is_some();
    let accounts = stored.map_or_else(crate::PostingAccounts::conventional, |c| c.value);

    Ok(Json(ConfiguredAccounts {
        accounts: AccountsView {
            receivable: accounts.receivable.as_str().to_owned(),
            revenue: accounts.revenue.as_str().to_owned(),
            output_vat: accounts.output_vat.as_str().to_owned(),
        },
        configured,
    }))
}

/// Chooses what sales posts to.
///
/// `ManageAccounts`, not `ManageTenant`: this is a decision about the chart of
/// accounts, and the person who maintains the chart is the person who should
/// make it.
///
/// **Not retrospective.** Invoices already issued keep the accounts they were
/// posted to, because those went into the journal entry as values. Changing
/// this changes the next invoice and nothing before it (L5).
///
/// Each account is checked against the tenant's own chart before it is stored —
/// a configuration that looks fine and refuses every invoice is worse than an
/// error here.
#[utoipa::path(
    put,
    path = "/v1/sales/posting-accounts",
    tag = "sales",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = AccountsView,
    responses(
        (status = NO_CONTENT, description = "Stored. Applies to the next invoice, not to past ones."),
        (status = BAD_REQUEST, description = "An unusable code, or one that is not an open account here", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_posting_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<AccountsView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;

    let accounts = crate::PostingAccounts {
        receivable: parse_id(&body.receivable, locale)?,
        revenue: parse_id(&body.revenue, locale)?,
        output_vat: parse_id(&body.output_vat, locale)?,
    };

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    // Checked against the tenant's own chart before storing. The alternative is
    // a configuration that looks fine and refuses every invoice, discovered by
    // whoever raises the next one.
    //
    // Asked of the **log**, not of `proj_ledger.account`. The read model is
    // driven by a worker and lags, so validating against it refuses a chart the
    // tenant installed a second ago — which is exactly what the first version of
    // this check did. `ledger::accepts_postings` is the same question
    // `post_entry_in` asks, asked the same way, and a guard that disagrees with
    // the command it guards is worse than no guard.
    for code in [
        &accounts.receivable,
        &accounts.revenue,
        &accounts.output_vat,
    ] {
        let usable = ledger::accepts_postings(&mut conn, code)
            .await
            .map_err(|e| {
                ApiError::Access(sqlx::Error::Decode(Box::new(e)).into())
                    .into_problem(locale, &CATALOG)
            })?;

        if !usable {
            return Err(ApiError::BadRequest(
                erp_i18n::Message::new(ledger::messages::NO_SUCH_ACCOUNT)
                    .with("code", erp_i18n::MessageArg::text(code.as_str().to_owned())),
            )
            .into_problem(locale, &CATALOG));
        }
    }

    erp_eventlog::configuration::set(
        &mut conn,
        crate::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

fn config_problem(error: &erp_eventlog::ConfigError, locale: Locale) -> Problem {
    tracing::error!(error = %error, "configuration failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &error.message(),
        locale,
        &CATALOG,
    )
}

// ---------------------------------------------------------------------------

/// Maps a command failure onto a status.
///
/// Same shape as [`ledger::http`]'s, and deliberately still its own function:
/// the interesting part is which rejection is a 409 and which is a 422, and that
/// is exactly the part a shared helper could not decide.
fn sales_problem(error: &CommandError<SalesError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Well-formed, but about something that is not there or not in a
                // state that allows it.
                SalesError::NotIssued(_) | SalesError::Ledger(_) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                // The invoice moved on between the client reading it and paying
                // it. Look again and decide.
                SalesError::Overpayment { .. } | SalesError::AlreadyCancelled { .. } => {
                    StatusCode::CONFLICT
                }
                // Well-formed, and refused on the state of the invoice.
                SalesError::HasPayments(_) => StatusCode::UNPROCESSABLE_ENTITY,
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
            tracing::error!(error = %other, "sales command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}
