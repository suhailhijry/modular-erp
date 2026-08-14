//! The sales module's HTTP surface.
//!
//! Translation only, like `ledger_routes` — everything that matters lives in the
//! module. What the two files now have in common is a `Module` trait's whole
//! content: a name, an install description, a set of projection groups, a
//! router, and a mapping from the module's rejections onto statuses. That is
//! Phase 4's to build, and it is now a description rather than a guess.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing};
use sales::{Customer, Draft, InvoiceLine, PostingAccounts, Receipt, SalesError, Vat, VatCategory};
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::ExecuteError;
use spa_i18n::{Locale, Localize};
use spa_types::{CurrencyCode, Timestamp};

use crate::consistency::{Consistency, nudge};
use crate::error::ApiError;
use crate::extract::{Allowed, Language, PostEntries, Read};
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

#[derive(Debug, Serialize)]
struct InvoiceView {
    id: String,
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

    let committed = sales::issue_invoice(
        &tenant.db,
        &id,
        &draft,
        &PostingAccounts::conventional(),
        &metadata(&tenant),
    )
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
        &PostingAccounts::conventional(),
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
                SalesError::Overpayment { .. } => StatusCode::CONFLICT,
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
