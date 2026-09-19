//! The sales module's HTTP surface.
//!
//! Translation only, like every module's — see [`ledger::http`] for why these
//! live in the module rather than in the composition root.

use crate::{Authority, Customer, Draft, DraftLine, Receipt, SalesError, VatCategory};
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
use erp_web::{
    After, Amount, Json, Paged, Query, bad_request, creating, metadata, parse_id, require_module,
};
use erp_web::{Allowed, IdempotencyKey, Language, ManageAccounts, ManageTenant, PostEntries, Read};
use erp_web::{Consistency, nudge};
use erp_web::{IfMatch, Versioned};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_invoices, issue_invoice))
        .routes(routes!(get_invoice))
        .routes(routes!(receivables))
        .routes(routes!(unmatched_customers))
        .routes(routes!(attach_customer))
        .routes(routes!(record_payment))
        .routes(routes!(refund_payment))
        .routes(routes!(credit_note))
        .routes(routes!(list_credit_notes, credit_invoice_part))
        // Typed on purpose. The store underneath is key-value; this is not, so
        // a value that reaches it has already been through the type that gives
        // it meaning. See `erp_eventlog::config`.
        .routes(routes!(posting_accounts, set_posting_accounts))
        .routes(routes!(document_limit, set_document_limit))
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
    lines: Vec<NewInvoiceLine>,
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
struct NewLineAllowance {
    /// Why, printed on the invoice — ZATCA shows it to the customer.
    reason: String,
    /// Minor units, **positive**: what comes off this line.
    amount: i64,
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
    /// The `crm` customer this is for. Optional, and checked when it is given:
    /// an id naming no record, or an archived one, refuses the invoice.
    ///
    /// The fields below are still yours and are still what gets printed. The
    /// reference is for grouping; the copy is what the document says.
    #[serde(default)]
    id: Option<String>,
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
struct NewInvoiceLine {
    description: String,
    /// Minor units, in the invoice's currency. Excluding tax, and **before this
    /// line's own allowances** — what is charged is this less them.
    ///
    /// **Per unit when `quantity` is given**, and the whole line otherwise. The
    /// line comes to `net × quantity`, worked out here and never divided back
    /// out: going backwards from a total is a division that does not always
    /// land on a whole halala, and the tax document has to balance.
    net: i64,
    /// **What this line sells off a shelf**, when it sells one: the product's
    /// id, as `/v1/inventory/products` declared it.
    ///
    /// Optional. A line with one depletes that product's stock at this branch
    /// in the same transaction as the invoice, and books what it cost; a line
    /// without one behaves exactly as before. A product nobody declared is
    /// refused rather than ignored.
    #[serde(default)]
    product: Option<String>,
    /// **How many units.** Optional: leave it out and the line is a single
    /// amount charged once, which is what every invoice issued before this
    /// field existed is.
    #[serde(default)]
    quantity: Option<i64>,
    /// **Which units**, on a serial-tracked product — one name per unit, as
    /// they were received. A line that names any must also name the `product`
    /// and charge for exactly that many. A name that is not on the shelf —
    /// unknown, already sold, written off — is refused.
    #[serde(default)]
    serials: Vec<String>,
    /// **Which lot** to take the units from, overriding the picking rule — a
    /// scanned batch, or stock promised to this customer. The lot's `id` as
    /// `GET /v1/inventory/lots` lists it, not its batch code. A line that names
    /// one must also name the `product`; a lot that is not open at this branch,
    /// or that holds fewer than the line takes, is refused. Leave it out and
    /// the earliest expiry goes first.
    #[serde(default)]
    lot: Option<String>,
    /// What comes off **this line**, each printed as its own figure.
    ///
    /// No treatment on them: the line already says how it is taxed, so an
    /// allowance on it reduces the taxable amount at that line's rate. A
    /// discount on the whole *document* is the other field, and that one has to
    /// name which treatment it comes off.
    #[serde(default)]
    allowances: Vec<NewLineAllowance>,
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

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "reference": "BANK-88245",
    "amount": { "minor": 575_000, "currency": "SAR" },
    "refunded_on": "2026-08-22T00:00:00Z",
    "account": "1000"
}))]
struct NewRefund {
    /// Your reference for handing the money back. Sending the same one twice
    /// against the same invoice is a no-op — **and it keys the credit note
    /// too**, so a retry does not issue a second document.
    reference: String,
    amount: Amount,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    refunded_on: Timestamp,
    /// The cash or bank account it went out of.
    account: String,
    /// Why, in the customer's language. **Printed on the credit note** and sent
    /// to ZATCA as the document's note, so a real sentence is worth more than
    /// the default.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct MatchCustomer {
    /// The `crm` record this invoice's buyer turned out to be.
    customer: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

/// A buyer named on invoices that no record has been matched to yet.
#[derive(Debug, Serialize, ToSchema)]
struct UnmatchedCustomerView {
    /// Exactly as the invoices printed it.
    name: String,
    /// Where any of them carried one. The strongest clue for matching.
    vat_number: Option<String>,
    invoices: i64,
    /// What those invoices came to, in minor units. Largest first, so the
    /// backlog worth clearing sorts to the top.
    gross: i64,
    currency: String,
    /// The most recent of them, so a name last seen years ago can be left.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    last_issued: Timestamp,
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
struct PaymentRecorded {
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
        (status = BAD_REQUEST, description = "No lines that come to anything, mixed currencies, an unknown VAT category, an unusable id, a quantity that is not one (`sales.not_a_quantity`), serials that do not match their line (`sales.named_units`, `inventory.needs_serials`), a lot named with no product (`sales.lot_without_a_product`), or a product id that cannot be one (`inventory.not_a_product_id`)", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, or over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the sales module is not enabled here", body = Problem),
        (status = CONFLICT, description = "Sustained contention on this invoice. Retryable.", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The posting accounts are missing or closed, no such customer, or the shelf refuses a line: a product nobody declared (`inventory.no_such_product`), a serial that is not on the shelf (`inventory.no_such_serial`), a named lot that is not open at this branch (`inventory.no_such_lot`) or holds fewer than the line takes (`inventory.lot_is_short`), or more of a lot- or serial-tracked product than the shelf holds (`inventory.not_enough_stock`). A plain product that names no lot never refuses for stock.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn issue_invoice(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewInvoice>,
) -> Result<(StatusCode, Json<Issued>), Problem> {
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

    let lines = drafted(body.lines, currency, locale)?;

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
    if let Some(reference) = &body.customer.id {
        // Parsed here so a malformed id is a 400 about the id, and not a
        // refusal from deep inside the issuing transaction about a customer
        // that could never have existed.
        customer = customer.of(parse_id(reference, locale)?);
    }
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
        // **Not a client's to declare.** A prepayment invoice is what
        // `payments` raises when a deposit settles; an ordinary caller issuing
        // one would be choosing a tax point.
        prepayment: false,
        prepaid: None,
        customer,
        issued_on: body.issued_on,
        due_on: body.due_on,
        currency,
        lines,
        discounts,
        note: body.note,
    };

    let committed = crate::issue_invoice(
        &tenant.db,
        &id,
        &draft,
        &creating(&tenant, &key),
        Authority::of(&tenant.db),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(Issued {
            id: id.to_string(),
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
        (status = OK, description = "PaymentRecorded, or already recorded under this reference.", body = PaymentRecorded),
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
) -> Result<Json<PaymentRecorded>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

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

    Ok(Json(PaymentRecorded {
        id: raw.to_owned(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Hand money back against an invoice.
///
/// The mirror of a payment, and what makes a paid invoice creditable: a credit
/// note is refused while the business is still holding the customer's money, so
/// the refund comes first and the credit note after.
///
/// Refunding more than is held is refused for the same reason overpaying is —
/// handing back money that was never taken is a decision somebody needs to see,
/// and a negative balance is how it never gets made.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices/{invoice}/refunds",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice the money is going back against."),
    ),
    request_body = NewRefund,
    responses(
        (status = OK, description = "Refunded, or already refunded under this reference.", body = PaymentRecorded),
        (status = BAD_REQUEST, description = "A non-positive amount, or an unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, over the tenant's document limit (`sales.over_document_limit`), or — when the refund clears the invoice, because that issues a whole-invoice credit note — without the `sales:approve_credit_note` claim once the tenant uses claims (`sales.not_approved`)", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "More than is held — read the invoice again and decide", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, or one that was never issued", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn refund_payment(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewRefund>,
) -> Result<Json<PaymentRecorded>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;
    let account = parse_id(&body.account, locale)?;

    let reason = body
        .reason
        .unwrap_or_else(|| format!("Refunded · {}", body.reference));

    let committed = crate::refund_invoice(
        &tenant.db,
        &invoice,
        &Receipt {
            reference: body.reference,
            amount: body.amount.parse(locale)?,
            received_on: body.refunded_on,
            into: account,
        },
        &reason,
        &metadata(&tenant),
        Authority::of(&tenant.db),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(PaymentRecorded {
        id: raw.to_owned(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Cancels an invoice by crediting it.
///
/// A `POST`, not a `DELETE`: the invoice stays, its journal entry is reversed,
/// and the books show both.
///
/// **The whole invoice.** For part of one, see
/// `POST /v1/sales/invoices/{invoice}/credit-notes` — a different shape, because
/// a partial credit note is a document with lines and a tax point of its own
/// rather than a fact about the invoice. The two are mutually exclusive: this
/// reverses the invoice's journal entry, which on an invoice already partly
/// credited would take the credited part back twice.
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
        (status = BAD_REQUEST, description = "An unusable id, or an invoice already partly credited (`sales.already_credited`)", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, without the `sales:approve_credit_note` claim once the tenant uses claims (`sales.not_approved`), or over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Already cancelled by a *different* credit note", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, one with payments against it — refund those first —, or a product line the shelf it was sold from has no record of (`inventory.not_consumed`)", body = Problem),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;

    let committed = crate::cancel_invoice(
        &tenant.db,
        &invoice,
        &body.id,
        &body.reason,
        body.on,
        &metadata(&tenant),
        Authority::of(&tenant.db),
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

/// One line of a credit note: which invoice line, and how much of it.
#[derive(Debug, Deserialize, ToSchema)]
struct NewCreditLine {
    /// **Which line of the invoice this credits**, counting from zero.
    ///
    /// The description and the tax treatment come from it. That is the point:
    /// a credit note can only describe something the invoice charged for, and
    /// is credited at the rate that invoice carried — one issued at 5% is
    /// credited at 5% for ever.
    against: u16,
    /// Excluding tax, and positive — stated the way the invoice stated it
    /// rather than as a negative. May be less than the line: part of a line
    /// can come back.
    amount: Amount,
    /// **How many units came back onto the shelf**, when the invoice line sold
    /// stock. Optional, and never worked out from `amount`: a partial credit is
    /// as often a price adjustment as a returned carton, and the two are
    /// different statements. Leave it out and nothing goes back on the shelf.
    #[serde(default)]
    quantity: Option<i64>,
    /// **Which units came back**, on a line that sold serial-tracked stock:
    /// one name per unit, as many as `quantity`. Each must be one this invoice
    /// line sold and that has not come back already. Leave it out and the
    /// units come back by quantity, which for named units only the whole line
    /// can.
    #[serde(default)]
    serials: Vec<String>,
}

/// A credit note against part of an invoice.
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "reference": "returned-the-shampoo",
    "reason": "Returned unopened",
    "on": "2026-03-04T09:00:00Z",
    "lines": [{"against": 1, "amount": {"minor": 5750, "currency": "SAR"}}]
}))]
struct NewPartialCreditNote {
    /// **Your own key for this credit note, not its number.** Sending the same
    /// one twice is a no-op. The number is allocated here, from the same
    /// gapless series a cancellation draws on.
    reference: String,
    lines: Vec<NewCreditLine>,
    #[serde(default)]
    reason: String,
    /// The credit note's **own** tax point — the period it falls in. Usually
    /// today, and not the invoice's date.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    on: Timestamp,
}

/// A credit note as a list shows it.
#[derive(Debug, Serialize, ToSchema)]
struct CreditNoteView {
    /// The statutory number.
    number: String,
    invoice: String,
    reference: String,
    net: i64,
    tax: i64,
    gross: i64,
    currency: String,
    reason: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    issued_on: Timestamp,
}

/// Credit notes against part of one invoice, newest first.
///
/// **A whole-invoice cancellation is not in here.** That is `credit_note` and
/// `cancelled_on` on the invoice itself, because it is a fact about the invoice
/// rather than a document with lines of its own.
#[utoipa::path(
    get,
    path = "/v1/sales/invoices/{invoice}/credit-notes",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = Vec<CreditNoteView>),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn list_credit_notes(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<CreditNoteView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let rows = crate::credit_notes(&mut conn, invoice.as_str())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| CreditNoteView {
                number: row.number,
                invoice: row.invoice,
                reference: row.reference,
                net: row.net.minor(),
                tax: row.tax.minor(),
                gross: row.gross.minor(),
                currency: row.net.currency().to_string(),
                reason: row.reason,
                issued_on: row.issued_on,
            })
            .collect(),
    ))
}

/// Credit part of an invoice.
///
/// # Not the same act as cancelling
///
/// Cancelling reverses the invoice's journal entry and says the whole supply is
/// undone. This posts its **own** entry for what it takes back, because there is
/// no such thing as reversing part of a journal entry — and it produces a
/// document with its own lines, which is what ZATCA computes a credit note's VAT
/// from.
///
/// An invoice can have several of these. It cannot have these **and** a
/// cancellation.
///
/// # It does not hand any money back
///
/// A credit note says the supply is undone; the cash is
/// `POST /v1/sales/invoices/{invoice}/refunds`, and a business may need both. An
/// invoice paid in full and then partly credited leaves the customer owed the
/// difference, which is `outstanding` going negative.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices/{invoice}/credit-notes",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice being credited."),
    ),
    request_body = NewPartialCreditNote,
    responses(
        (status = OK, description = "Credited, or already credited under this key.", body = Issued),
        (status = BAD_REQUEST, description = "An unusable id, an amount that is not one, no such line (`sales.no_such_line`), more than is left to credit (`sales.credit_too_large`), an invoice already cancelled outright (`sales.already_credited`), a quantity that is not one (`sales.not_a_quantity`), units coming back on a line that sold no product (`sales.not_a_stock_line`), or names that do not agree with the quantity (`sales.named_units`, `inventory.needs_serials`)", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a role that may, without the `sales:approve_credit_note` claim once the tenant uses claims (`sales.not_approved`), or over the tenant's document limit (`sales.over_document_limit`)", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Sustained contention on this invoice. Retryable.", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, or the shelf refuses the units coming back: no record of that line going out (`inventory.not_consumed`), more than that sale still has out (`inventory.more_than_was_taken`), part of a sale whose units have names given as a quantity rather than by name (`inventory.named_units_come_back_whole`), or a name that sale did not take or that has already come back (`inventory.not_out`)", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn credit_invoice_part(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewPartialCreditNote>,
) -> Result<Json<Issued>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;

    let lines = credit_lines(body.lines, locale)?;

    let credited = crate::credit_invoice_part(
        &tenant.db,
        &invoice,
        &crate::CreditNote {
            reference: body.reference.clone(),
            lines,
            reason: body.reason,
            on: body.on,
        },
        &metadata(&tenant),
        Authority::of(&tenant.db),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(Issued {
        id: body.reference,
        number: credited.number,
        position: credited.committed.at.map(erp_types::LogPosition::get),
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

    let invoices = crate::invoices(&mut conn, limit, after.as_ref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(Paged::of(invoices, view)))
}

/// One customer's debt, split by how late it is.
#[derive(Debug, Serialize, ToSchema)]
struct AgedCustomerView {
    /// What this row is grouped under: a customer id, or the frozen name when
    /// the invoices named no record.
    key: String,
    /// Whether `key` is a customer id.
    ///
    /// `false` means these invoices were grouped by the name they froze, so two
    /// spellings of one buyer are two rows. Record them in `crm` and reference
    /// them to merge.
    identified: bool,
    /// A name to show: the one the most recent invoice in this group froze.
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
        key: row.key,
        identified: row.identified,
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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

/// Buyer names on invoices that no customer record has been matched to.
///
/// The worklist for reconciling invoices issued before `crm` existed, or before
/// this buyer was recorded. One row per spelling, largest backlog first, because
/// the job is matching people and forty invoices for one name is one decision.
#[utoipa::path(
    get,
    path = "/v1/sales/unmatched-customers",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("limit" = Option<i64>, Query, description = "Rows to return. Clamped, not refused."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "The worklist, biggest first. Empty means everything is matched.", body = Vec<UnmatchedCustomerView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the sales module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The projection did not reach `consistent_after` in time. Retryable.", body = Problem),
    ),
)]
async fn unmatched_customers(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<After>,
) -> Result<Json<Vec<UnmatchedCustomerView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let limit = query.limit(PAGE, PAGE);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let rows = crate::unmatched_customers(&mut conn, limit)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| UnmatchedCustomerView {
                name: row.name,
                vat_number: row.vat_number,
                invoices: row.invoices,
                gross: row.gross.minor(),
                currency: row.gross.currency().to_string(),
                last_issued: row.last_issued,
            })
            .collect(),
    ))
}

/// Match a customer record to an invoice that was issued without one.
///
/// **This sets the reference and never the printed name.** What the invoice says
/// about its buyer was frozen when it was issued and stays frozen — that is what
/// the law requires the document to say, and a reconciliation does not get to
/// restate a document somebody has already filed a return against.
///
/// Sending the same record twice is a no-op. Sending a *different* one is a
/// correction and is recorded as one, because a match made to the wrong customer
/// has to be fixable.
#[utoipa::path(
    post,
    path = "/v1/sales/invoices/{invoice}/customer",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("invoice" = String, Path, description = "The invoice being matched."),
    ),
    request_body = MatchCustomer,
    responses(
        (status = OK, description = "Matched, or already matched to this record.", body = PaymentRecorded),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such invoice, one that was never issued, or no such customer", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn attach_customer(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<MatchCustomer>,
) -> Result<Json<PaymentRecorded>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;
    let customer = parse_id(&body.customer, locale)?;

    let committed = crate::attach_customer(
        &tenant.db,
        &invoice,
        &customer,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(PaymentRecorded {
        id: raw.to_owned(),
        position: committed.at.map(erp_types::LogPosition::get),
    }))
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
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
        (status = OK, body = ConfiguredAccounts, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn posting_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<ConfiguredAccounts>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

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
    let version = stored.as_ref().map_or(0, |c| c.version);
    let accounts = stored.map_or_else(crate::PostingAccounts::conventional, |c| c.value);

    Ok(Versioned(
        version,
        ConfiguredAccounts {
            accounts: AccountsView {
                receivable: accounts.receivable.as_str().to_owned(),
                revenue: accounts.revenue.as_str().to_owned(),
                output_vat: accounts.output_vat.as_str().to_owned(),
            },
            configured,
        },
    ))
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
    params(("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."), ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = AccountsView,
    responses(
        (status = NO_CONTENT, description = "Stored. Applies to the next invoice, not to past ones."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = BAD_REQUEST, description = "An unusable code, or one that is not an open account here", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_posting_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<AccountsView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

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
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// The document limit, as the owner reads and writes it.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "limit": { "amount": { "minor": 1_000_000, "currency": "SAR" }, "basis": "after_vat" } }))]
struct DocumentLimitView {
    /// `null`, or absent, is no limit — how every tenant starts, and how an
    /// owner removes one.
    limit: Option<LimitView>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct LimitView {
    /// The most one invoice, credit note or refund may come to. More than
    /// nothing. A document in another currency is refused as well, because it
    /// cannot be compared with this.
    amount: Amount,
    /// `before_vat` or `after_vat`: which of a document's totals is compared.
    #[schema(value_type = String, example = "after_vat")]
    basis: crate::Basis,
}

/// How large a document a member may issue.
///
/// Every invoice, credit note and refund a member other than the owner issues
/// — here, at the till, from the booking desk, by asking a gateway for a
/// refund, or by charging a deposit whose prepayment invoice the gateway's
/// settlement raises — is refused with `403 sales.over_document_limit` when it
/// comes to more, unless the org chart gives them `sales:exceed_document_limit`
/// in the branch they named. What nobody issues by hand is not limited: a
/// customer's own deposit, the worker billing a completed booking.
#[utoipa::path(
    get,
    path = "/v1/sales/document-limit",
    tag = "sales",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, description = "`limit: null` until the owner sets one.", body = DocumentLimitView, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = INTERNAL_SERVER_ERROR, description = "The stored limit is one this build cannot use. Every document a member issues is refused until it is set again, which this route's `PUT` still can", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn document_limit(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
) -> Result<Versioned<DocumentLimitView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, crate::DocumentLimit::KEY)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let limit = crate::DocumentLimit::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    Ok(Versioned(
        version,
        DocumentLimitView {
            limit: limit.map(|limit| LimitView {
                amount: Amount {
                    minor: limit.limit().minor(),
                    currency: limit.limit().currency().to_string(),
                },
                basis: limit.basis(),
            }),
        },
    ))
}

/// Set it, or remove it with `limit: null`.
///
/// **Applies to the next document.** Nothing already issued is judged again,
/// and a retry of a document issued before the limit was lowered answers with
/// that document.
#[utoipa::path(
    put,
    path = "/v1/sales/document-limit",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."),
    ),
    request_body = DocumentLimitView,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "An amount that is not more than nothing (`sales.document_limit_not_positive`), or a currency that is not one", body = Problem),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_document_limit(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<DocumentLimitView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let limit = match body.limit {
        Some(view) => Some(
            crate::DocumentLimit::new(view.amount.parse(locale)?, view.basis)
                .map_err(|e| ApiError::BadRequest(e.message()).into_problem(locale, &CATALOG))?,
        ),
        None => None,
    };
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::DocumentLimit::KEY,
        &limit,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

fn config_problem(error: &erp_eventlog::ConfigError, locale: Locale) -> Problem {
    erp_web::config_problem(error, locale, &CATALOG)
}

// ---------------------------------------------------------------------------

/// The request's lines as a draft's — the treatment parsed, the amounts put
/// into the invoice's currency, and the product id parsed **here** so a
/// malformed one is a 400 about the id rather than a refusal from inside the
/// issuing transaction. The same place, and the same reason, as the customer
/// reference.
fn drafted(
    sent: Vec<NewInvoiceLine>,
    currency: CurrencyCode,
    locale: Locale,
) -> Result<Vec<DraftLine>, Problem> {
    sent.into_iter()
        .map(|line| {
            let category: VatCategory = line.vat.parse().map_err(|_| {
                bad_request(
                    erp_web::messages::UNKNOWN_VAT_CATEGORY,
                    "vat",
                    &line.vat,
                    locale,
                )
            })?;
            Ok(DraftLine {
                description: line.description,
                net: erp_types::Money::from_minor(line.net, currency),
                category,
                product: line
                    .product
                    .as_deref()
                    .map(|id| parse_id(id, locale))
                    .transpose()?,
                quantity: line.quantity,
                serials: line.serials,
                lot: line.lot,
                allowances: line
                    .allowances
                    .into_iter()
                    .map(|a| crate::Allowance {
                        reason: a.reason,
                        amount: erp_types::Money::from_minor(a.amount, currency),
                    })
                    .collect(),
            })
        })
        .collect()
}

/// The request's credit lines as the command's: the amount parsed, and **the
/// units coming back carried through here** — how many and which. A field
/// dropped in this translation is a credit note whose money posts and whose
/// goods never reach the shelf.
fn credit_lines(
    sent: Vec<NewCreditLine>,
    locale: Locale,
) -> Result<Vec<crate::CreditLine>, Problem> {
    sent.into_iter()
        .map(|line| {
            Ok(crate::CreditLine {
                against: line.against,
                net: line.amount.parse(locale)?,
                quantity: line.quantity,
                serials: line.serials,
            })
        })
        .collect()
}

/// **The status of a sales refusal, for every door it surfaces from.** The till
/// asks this too, so one refusal is not two statuses depending on the route.
#[must_use]
pub fn rejection_status(rejection: &SalesError) -> StatusCode {
    match rejection {
        // Who is asking, not what was asked: a claim, or the document limit.
        refused if refused.refuses_the_caller() => StatusCode::FORBIDDEN,
        // The invoice moved on between the client reading it and paying it.
        // Look again and decide.
        SalesError::Overpayment { .. } | SalesError::AlreadyCancelled { .. } => {
            StatusCode::CONFLICT
        }
        // Well-formed, and refused on the state of something the request named:
        // the invoice, the posting accounts, or the customer record. None is a
        // 404 — the invoice is what was being created, and it is the *body*
        // that named what is missing or wrong.
        SalesError::NotIssued(_)
        | SalesError::Ledger(_)
        | SalesError::HasPayments(_)
        | SalesError::NoSuchCustomer(_)
        | SalesError::DocumentCurrency { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        // **A line of the wrong shape.**
        refused if refused.is_malformed() => StatusCode::BAD_REQUEST,
        // **The shelf's own split, not a second one.** A serial that is not
        // there and a product nobody declared are well-formed requests about a
        // world that says no; `inventory` already decides which is which.
        SalesError::Stock(_) => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// Maps a command failure onto a status.
///
/// Same shape as [`ledger::http`]'s, and deliberately still its own function:
/// the interesting part is which rejection is a 409 and which is a 422, and that
/// is exactly the part a shared helper could not decide.
fn sales_problem(error: &CommandError<SalesError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => {
            (rejection_status(rejection), rejection.message())
        }

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
            tracing::error!(error = %other, "sales command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The wire's product, quantity, serials and lot reach the draft**, which is
    /// the whole of what this layer does on the stock path: `issue_in`
    /// depletes, and a field dropped here is an invoice that takes nothing off
    /// a shelf and books no cost. The `net` stays **one unit's** — multiplying
    /// is `priced_lines`' job, and doing it twice is a division nobody can
    /// undo.
    #[test]
    fn a_line_carries_its_product_and_its_units_to_the_draft() {
        let sent: Vec<NewInvoiceLine> = serde_json::from_value(serde_json::json!([{
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
        let drafted = drafted(sent, sar, Locale::English).expect("a draft");
        let line = &drafted[0];
        assert_eq!(
            line.product.as_ref().map(erp_types::AggregateId::as_str),
            Some("f81d4fae-7dec-11d0-a765-00a0c91e6bf6")
        );
        assert_eq!(line.quantity, Some(3));
        assert_eq!(line.serials, ["A-1", "A-2", "A-3"]);
        assert_eq!(line.lot.as_deref(), Some("lot.x"));
        assert_eq!(
            line.net,
            erp_types::Money::from_minor(2_500, sar),
            "one unit's, not the line's"
        );
    }

    /// A line without them is what every invoice issued before they existed
    /// is: a single amount, charged once, off no shelf.
    #[test]
    fn a_line_without_them_is_a_bare_total() {
        let sent: Vec<NewInvoiceLine> = serde_json::from_value(serde_json::json!([
            { "description": "استشارة", "net": 2_500, "vat": "standard" }
        ]))
        .expect("a line");
        let sar = CurrencyCode::new("SAR").expect("a currency");
        let drafted = drafted(sent, sar, Locale::English).expect("a draft");
        assert_eq!(drafted[0].product, None);
        assert_eq!(drafted[0].quantity, None);
        assert!(drafted[0].serials.is_empty());
        assert_eq!(drafted[0].lot, None);
    }

    /// **A credit line carries the units coming back to the command** — how
    /// many and which. Every test that calls the command directly passes
    /// whatever this does, which is §74's lesson from the till.
    #[test]
    fn a_credit_line_carries_its_units_to_the_command() {
        let sent: Vec<NewCreditLine> = serde_json::from_value(serde_json::json!([
            {
                "against": 0,
                "amount": { "minor": 50_000, "currency": "SAR" },
                "quantity": 1,
                "serials": ["SN-2"]
            },
            { "against": 1, "amount": { "minor": 1_000, "currency": "SAR" } }
        ]))
        .expect("lines");
        let lines = credit_lines(sent, Locale::English).expect("parses");
        assert_eq!(lines[0].quantity, Some(1));
        assert_eq!(lines[0].serials, ["SN-2"]);
        assert_eq!(lines[1].quantity, None, "money only, and no goods");
        assert!(lines[1].serials.is_empty());
    }
}
