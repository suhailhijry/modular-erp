//! The sales module's HTTP surface.
//!
//! Translation only, like `ledger_routes` — everything that matters lives in the
//! module. What the two files now have in common is a `Module` trait's whole
//! content: a name, an install description, a set of projection groups, a
//! router, and a mapping from the module's rejections onto statuses. That is
//! Phase 4's to build, and it is now a description rather than a guess.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use sales::{Customer, Draft, DraftLine, Receipt, SalesError, VatCategory};
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
use crate::extract::{Allowed, Language, ManageAccounts, PostEntries, Read};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::{Amount, Json, bad_request, metadata, parse_id, require_module};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_invoices, issue_invoice))
        .routes(routes!(get_invoice))
        .routes(routes!(record_payment))
        .routes(routes!(credit_note))
        // Typed on purpose. The store underneath is key-value; this is not, so
        // a value that reaches it has already been through the type that gives
        // it meaning. See `spa_eventlog::config`.
        .routes(routes!(posting_accounts, set_posting_accounts))
}

/// How many invoices a list returns. ponytail: no cursor until a tenant has a
/// list long enough to need one — see `sales::invoices`.
const PAGE: i64 = 200;

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
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewCustomer {
    name: String,
    /// The buyer's VAT registration number, printed on the invoice.
    #[serde(default)]
    vat_number: Option<String>,
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

fn view(summary: sales::InvoiceSummary) -> InvoiceView {
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
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
    require_module(&tenant, &sales::module_id(), locale)?;

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
        lines.push(DraftLine {
            description: line.description,
            net: spa_types::Money::from_minor(line.net, currency),
            category,
        });
    }

    let mut customer = Customer::new(body.customer.name);
    if let Some(number) = body.customer.vat_number {
        customer = customer.with_vat_number(number);
    }

    let draft = Draft {
        customer,
        issued_on: body.issued_on,
        due_on: body.due_on,
        currency,
        lines,
        note: body.note,
    };

    let committed = sales::issue_invoice(&tenant.db, &id, &draft, &metadata(&tenant))
        .await
        .map_err(|e| sales_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok((
        StatusCode::CREATED,
        Json(Issued {
            id: body.id,
            number: committed.number,
            position: committed.committed.at.map(spa_types::LogPosition::get),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &sales::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;
    let account = parse_id(&body.account, locale)?;

    let committed = sales::record_payment(
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
        position: committed.at.map(spa_types::LogPosition::get),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &sales::module_id(), locale)?;

    let raw = params.get("invoice").map_or("", String::as_str);
    let invoice = parse_id(raw, locale)?;

    let committed = sales::cancel_invoice(
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
        position: committed.committed.at.map(spa_types::LogPosition::get),
    }))
}

/// Invoices, most recently issued first.
///
/// ponytail: the most recent 200, with no cursor. See `sales::invoices`.
#[utoipa::path(
    get,
    path = "/v1/sales/invoices",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = Vec<InvoiceView>),
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
) -> Result<Json<Vec<InvoiceView>>, Problem> {
    require_module(&tenant, &sales::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, sales::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let invoices = sales::invoices(&mut conn, PAGE)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    Ok(Json(invoices.into_iter().map(view).collect()))
}

/// One invoice, with its lines, its tax breakdown and its payments.
#[utoipa::path(
    get,
    path = "/v1/sales/invoices/{invoice}",
    tag = "sales",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
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
    require_module(&tenant, &sales::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, sales::GROUP_NAME, locale)
        .await?;

    let id = params.get("invoice").map_or("", String::as_str);

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let detail = sales::invoice(&mut conn, id)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    let detail = detail.ok_or_else(|| {
        ApiError::NotFound(
            spa_i18n::Message::new(crate::messages::NO_SUCH_INVOICE)
                .with("invoice", spa_i18n::MessageArg::text(id.to_owned())),
        )
        .into_problem(locale)
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
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
    require_module(&tenant, &sales::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let stored = spa_eventlog::configuration::get::<sales::PostingAccounts>(
        &mut conn,
        sales::PostingAccounts::KEY,
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    let configured = stored.is_some();
    let accounts = stored.map_or_else(sales::PostingAccounts::conventional, |c| c.value);

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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
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
    require_module(&tenant, &sales::module_id(), locale)?;

    let accounts = sales::PostingAccounts {
        receivable: parse_id(&body.receivable, locale)?,
        revenue: parse_id(&body.revenue, locale)?,
        output_vat: parse_id(&body.output_vat, locale)?,
    };

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

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
                ApiError::Access(sqlx::Error::Decode(Box::new(e)).into()).into_problem(locale)
            })?;

        if !usable {
            return Err(ApiError::BadRequest(
                spa_i18n::Message::new(ledger::messages::NO_SUCH_ACCOUNT)
                    .with("code", spa_i18n::MessageArg::text(code.as_str().to_owned())),
            )
            .into_problem(locale));
        }
    }

    spa_eventlog::configuration::set(
        &mut conn,
        sales::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

fn config_problem(error: &spa_eventlog::ConfigError, locale: Locale) -> Problem {
    tracing::error!(error = %error, "configuration failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &error.message(),
        locale,
        &crate::catalog::CATALOG,
    )
}

// ---------------------------------------------------------------------------

/// Maps a command failure onto a status.
///
/// Same shape as `ledger_routes::ledger_problem`, and deliberately still its own
/// function: the interesting part is which rejection is a 409 and which is a
/// 422, and that is exactly the part a shared helper could not decide.
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

        CommandError::Pool(e @ spa_control::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            spa_i18n::Message::new(spa_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        other => {
            tracing::error!(error = %other, "sales command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                spa_i18n::Message::new(spa_control::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}
