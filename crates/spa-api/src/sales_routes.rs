//! The sales module's HTTP surface.
//!
//! Translation only, like `ledger_routes` — everything that matters lives in the
//! module. What the two files now have in common is a `Module` trait's whole
//! content: a name, an install description, a set of projection groups, a
//! router, and a mapping from the module's rejections onto statuses. That is
//! Phase 4's to build, and it is now a description rather than a guess.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing};
use sales::{Customer, Draft, InvoiceLine, Receipt, SalesError, Vat, VatCategory};
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::ExecuteError;
use spa_i18n::{Locale, Localize};
use spa_types::{CurrencyCode, Timestamp};

use crate::consistency::{Consistency, nudge};
use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageAccounts, PostEntries, Read};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::{Amount, bad_request, metadata, parse_id, require_module};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/tenants/{slug}/sales/invoices",
            routing::get(list_invoices).post(issue_invoice),
        )
        .route(
            "/v1/tenants/{slug}/sales/invoices/{invoice}",
            routing::get(get_invoice),
        )
        .route(
            "/v1/tenants/{slug}/sales/invoices/{invoice}/payments",
            routing::post(record_payment),
        )
        .route(
            "/v1/tenants/{slug}/sales/invoices/{invoice}/credit-note",
            routing::post(credit_note),
        )
        .route(
            "/v1/tenants/{slug}/sales/vat-return",
            routing::get(vat_return),
        )
        // Typed on purpose. The store underneath is key-value; this is not, so
        // a value that reaches it has already been through the type that gives
        // it meaning. See `spa_eventlog::config`.
        .route(
            "/v1/tenants/{slug}/sales/posting-accounts",
            routing::get(posting_accounts).put(set_posting_accounts),
        )
}

/// How many invoices a list returns. ponytail: no cursor until a tenant has a
/// list long enough to need one — see `sales::invoices`.
const PAGE: i64 = 200;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NewInvoice {
    /// The invoice number, chosen by the client. Issuing the same one twice is a
    /// no-op, which is what makes a retried request safe.
    id: String,
    customer: NewCustomer,
    issued_on: Timestamp,
    #[serde(default)]
    due_on: Option<Timestamp>,
    currency: String,
    lines: Vec<NewLine>,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct NewCustomer {
    name: String,
    #[serde(default)]
    vat_number: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NewLine {
    description: String,
    /// Minor units, in the invoice's currency. Excluding tax.
    net: i64,
    /// `standard`, `zero` or `exempt`. The *rate* is not a client's to choose —
    /// it is statutory, and resolved here.
    vat: String,
}

#[derive(Debug, Deserialize)]
struct NewPayment {
    /// The payer's or the bank's reference. Recording the same one twice against
    /// the same invoice is a no-op.
    reference: String,
    amount: Amount,
    received_on: Timestamp,
    /// The cash or bank account it landed in.
    account: String,
}

#[derive(Debug, Serialize)]
struct Written {
    id: String,
    /// Where it landed in the log. A client that wants to read its own write
    /// back passes this as `?consistent_after=`.
    position: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct NewCreditNote {
    /// The credit note's own number. Crediting the same invoice twice with the
    /// same one is a no-op; a different one is refused.
    id: String,
    #[serde(default)]
    reason: String,
    /// When the credit is treated as happening. Usually today, not the date of
    /// the invoice.
    on: Timestamp,
}

#[derive(Debug, Serialize)]
struct InvoiceView {
    id: String,
    /// Set once a credit note has cancelled it. A cancelled invoice owes
    /// nothing, and `outstanding` says so too.
    cancelled_on: Option<Timestamp>,
    credit_note: Option<String>,
    customer: String,
    customer_vat: Option<String>,
    issued_on: Timestamp,
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

#[derive(Debug, Serialize)]
struct InvoiceDetailView {
    #[serde(flatten)]
    invoice: InvoiceView,
    lines: Vec<LineView>,
    /// One row per rate, which is what a Saudi tax invoice has to print.
    tax_breakdown: Vec<TaxView>,
    payments: Vec<PaymentView>,
}

#[derive(Debug, Serialize)]
struct LineView {
    description: String,
    net: i64,
    vat: &'static str,
    /// Basis points — 1500 is 15%. The rate that applied when it was issued, not
    /// today's.
    vat_rate: i32,
}

#[derive(Debug, Serialize)]
struct TaxView {
    vat: &'static str,
    vat_rate: i32,
    net: i64,
    tax: i64,
}

#[derive(Debug, Serialize)]
struct PaymentView {
    reference: String,
    amount: i64,
    received_on: Timestamp,
    account: String,
}

fn view(summary: sales::InvoiceSummary) -> InvoiceView {
    InvoiceView {
        id: summary.id,
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
        payments: summary.payments,
        note: summary.note,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn issue_invoice(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewInvoice>,
) -> Result<(StatusCode, Json<Written>), Problem> {
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
        lines.push(InvoiceLine {
            description: line.description,
            net: spa_types::Money::from_minor(line.net, currency),
            vat: Vat::current(category),
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
        Json(Written {
            id: body.id,
            position: committed.at.map(spa_types::LogPosition::get),
        }),
    ))
}

async fn record_payment(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewPayment>,
) -> Result<Json<Written>, Problem> {
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

    Ok(Json(Written {
        id: raw.to_owned(),
        position: committed.at.map(spa_types::LogPosition::get),
    }))
}

/// Cancels an invoice by crediting it.
///
/// A `POST`, not a `DELETE`: the invoice stays, its journal entry is reversed,
/// and the books show both.
async fn credit_note(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewCreditNote>,
) -> Result<Json<Written>, Problem> {
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

    Ok(Json(Written {
        id: body.id,
        position: committed.at.map(spa_types::LogPosition::get),
    }))
}

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

#[derive(Debug, Deserialize)]
struct Period {
    /// Inclusive.
    from: Timestamp,
    /// **Exclusive.** A period ending "31 March inclusive" is a comparison
    /// somebody gets wrong once a quarter, and two consecutive returns built
    /// that way either double-count the boundary or drop it.
    until: Timestamp,
    currency: String,
}

#[derive(Debug, Serialize)]
struct VatBandView {
    vat: &'static str,
    vat_rate: i32,
    net: i64,
    tax: i64,
    invoices: i64,
}

#[derive(Debug, Serialize)]
struct VatReturnView {
    from: Timestamp,
    until: Timestamp,
    currency: String,
    bands: Vec<VatBandView>,
    net: i64,
    /// What goes on the return.
    tax: i64,
}

/// The output-tax side of a VAT return, for a period.
///
/// What the business *charged*. A full return also nets off input tax on
/// purchases, which needs a purchases module — this is the side that exists.
async fn vat_return(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(period): Query<Period>,
) -> Result<Json<VatReturnView>, Problem> {
    require_module(&tenant, &sales::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, sales::GROUP_NAME, locale)
        .await?;

    let currency = CurrencyCode::new(&period.currency).map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            &period.currency,
            locale,
        )
    })?;

    if period.until <= period.from {
        return Err(bad_request(
            crate::messages::EMPTY_PERIOD,
            "period",
            &period.from.to_rfc3339(),
            locale,
        ));
    }

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let filed = sales::vat_return(&mut conn, currency, period.from, period.until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    drop(conn);

    Ok(Json(VatReturnView {
        from: filed.from,
        until: filed.until,
        currency: filed.currency.to_string(),
        bands: filed
            .bands
            .iter()
            .map(|b| VatBandView {
                vat: b.category.as_str(),
                vat_rate: b.basis_points,
                net: b.net.minor(),
                tax: b.tax.minor(),
                invoices: b.invoices,
            })
            .collect(),
        net: filed.net.minor(),
        tax: filed.tax.minor(),
    }))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct AccountsView {
    /// Debited by what customers owe. Defaults to 1100.
    receivable: String,
    /// Credited by what was earned, excluding tax. Defaults to 4000.
    revenue: String,
    /// Credited by tax charged and owed to ZATCA. Defaults to 2100.
    output_vat: String,
}

#[derive(Debug, Serialize)]
struct ConfiguredAccounts {
    #[serde(flatten)]
    accounts: AccountsView,
    /// `false` when nothing has been configured and these are the shipped
    /// defaults — so a settings screen can say "using the standard chart"
    /// rather than implying somebody chose this.
    configured: bool,
}

/// What sales posts to. Answers with the shipped defaults when the tenant has
/// never chosen.
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
