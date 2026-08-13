//! The ledger module's HTTP surface.
//!
//! # Why this is here and not in the module
//!
//! ponytail: with one module, a route layer in the API is 80 lines; a `Module`
//! trait that mounts routers is a trait with one implementation. When a second
//! module arrives, whatever these two have in common *is* the trait — and it
//! will be a description rather than a guess.
//!
//! What the module does own is everything that matters: the aggregates, the
//! invariant, and the read models. This file only translates.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use ledger::{AccountKind, BalancedLines, Line};
use serde::{Deserialize, Serialize};
use spa_control::CommandError;
use spa_eventlog::{ExecuteError, Metadata};
use spa_i18n::{Locale, Localize};
use spa_types::{AggregateId, CurrencyCode, Money, Timestamp};

use crate::error::ApiError;
use crate::extract::{Language, Tenant};
use crate::problem::Problem;
use crate::state::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/tenants/{slug}/ledger/accounts",
            routing::get(list_accounts).post(open_account),
        )
        .route(
            "/v1/tenants/{slug}/ledger/entries",
            routing::post(post_entry),
        )
        .route(
            "/v1/tenants/{slug}/ledger/trial-balance",
            routing::get(trial_balance),
        )
        // Unauthenticated on purpose: a signup form needs to show the choices
        // before anyone has an account. It is product information, not data.
        .route("/v1/ledger/charts", routing::get(list_charts))
        .route(
            "/v1/tenants/{slug}/ledger/chart",
            routing::post(install_chart),
        )
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// An amount, as a client sends it.
///
/// Minor units and an explicit currency — never a decimal string, and never a
/// float. A client that sends `10.50` has already lost the argument about how
/// many decimal places the currency has.
#[derive(Debug, Deserialize)]
struct Amount {
    minor: i64,
    currency: String,
}

impl Amount {
    fn parse(&self, locale: Locale) -> Result<Money, Problem> {
        let currency = CurrencyCode::new(&self.currency).map_err(|_| {
            ApiError::BadRequest(
                spa_i18n::Message::new(crate::messages::UNKNOWN_CURRENCY).with(
                    "currency",
                    spa_i18n::MessageArg::text(self.currency.clone()),
                ),
            )
            .into_problem(locale)
        })?;
        Ok(Money::from_minor(self.minor, currency))
    }
}

#[derive(Debug, Deserialize)]
struct NewAccount {
    code: String,
    name: String,
    kind: String,
    currency: String,
}

#[derive(Debug, Serialize)]
struct AccountView {
    code: String,
    name: String,
    kind: &'static str,
    balance: i64,
    currency: String,
    closed: bool,
    postings: i64,
}

#[derive(Debug, Deserialize)]
struct NewEntry {
    /// The client's own identifier for the entry. Posting the same one twice is
    /// a no-op, which is what makes a retried request safe.
    id: String,
    occurred_on: Timestamp,
    #[serde(default)]
    memo: String,
    lines: Vec<NewLine>,
}

#[derive(Debug, Deserialize)]
struct NewLine {
    account: String,
    /// Positive debits, negative credits.
    amount: Amount,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Debug, Serialize)]
struct EntryPosted {
    id: String,
    /// Where it landed in the log. A client that wants to read its own write
    /// back passes this as `?consistent_after=`.
    position: Option<i64>,
    lines: usize,
}

#[derive(Debug, Serialize)]
struct TrialBalanceView {
    currency: String,
    debits: i64,
    credits: i64,
    difference: i64,
    postings: i64,
    balances: bool,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_accounts(
    tenant: Tenant,
    Language(locale): Language,
) -> Result<Json<Vec<AccountView>>, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let accounts = ledger::account_balances(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    Ok(Json(
        accounts
            .into_iter()
            .map(|a| AccountView {
                code: a.code,
                name: a.name,
                kind: a.kind.as_str(),
                balance: a.balance.minor(),
                currency: a.balance.currency().to_string(),
                closed: a.closed,
                postings: a.postings,
            })
            .collect(),
    ))
}

async fn open_account(
    tenant: Tenant,
    Language(locale): Language,
    Json(body): Json<NewAccount>,
) -> Result<impl IntoResponse, Problem> {
    let code = parse_id(&body.code, locale)?;
    let kind: AccountKind = body.kind.parse().map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_ACCOUNT_KIND,
            "kind",
            &body.kind,
            locale,
        )
    })?;
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    ledger::open_account(
        &tenant.db,
        &code,
        &body.name,
        kind,
        currency,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| ledger_problem(&e, locale))?;

    Ok(StatusCode::CREATED)
}

async fn post_entry(
    tenant: Tenant,
    Language(locale): Language,
    Json(body): Json<NewEntry>,
) -> Result<Json<EntryPosted>, Problem> {
    let id = parse_id(&body.id, locale)?;

    let mut lines = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        let account = parse_id(&line.account, locale)?;
        let mut parsed = Line::new(account, line.amount.parse(locale)?);
        parsed.memo.clone_from(&line.memo);
        lines.push(parsed);
    }

    // The type refuses an unbalanced set, so this is where a client's mistake
    // becomes a 400 with the difference in it.
    let balanced = BalancedLines::new(lines)
        .map_err(|e| ApiError::BadRequest(e.message()).into_problem(locale))?;
    let line_count = balanced.len();

    let committed = ledger::post_entry(
        &tenant.db,
        &id,
        body.occurred_on,
        &body.memo,
        balanced,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| ledger_problem(&e, locale))?;

    Ok(Json(EntryPosted {
        id: body.id,
        position: committed.at.map(spa_types::LogPosition::get),
        lines: line_count,
    }))
}

async fn trial_balance(
    tenant: Tenant,
    Language(locale): Language,
) -> Result<Json<Vec<TrialBalanceView>>, Problem> {
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    let rows = ledger::trial_balance(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;

    Ok(Json(
        rows.into_iter()
            .map(|t| TrialBalanceView {
                currency: t.currency.to_string(),
                debits: t.debits.minor(),
                credits: t.credits.minor(),
                difference: t.difference.minor(),
                postings: t.postings,
                balances: t.balances(),
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------

/// Records who did it. Every event carries this (architecture L5).
fn metadata(tenant: &Tenant) -> Metadata {
    Metadata {
        actor: Some(tenant.session.identity.to_string()),
        ..Metadata::default()
    }
}

fn parse_id(raw: &str, locale: Locale) -> Result<AggregateId, Problem> {
    AggregateId::new(raw).map_err(|_| bad_request(crate::messages::INVALID_ID, "id", raw, locale))
}

fn bad_request(code: spa_i18n::MessageCode, arg: &str, value: &str, locale: Locale) -> Problem {
    ApiError::BadRequest(
        spa_i18n::Message::new(code).with(arg, spa_i18n::MessageArg::text(value.to_owned())),
    )
    .into_problem(locale)
}

/// Maps a command failure onto a status.
///
/// The rejections are the client's fault and say why; everything else routes
/// through [`ApiError`], which already knows that a conflict is a 409 and an
/// exhausted lane is a 503.
fn ledger_problem(error: &CommandError<ledger::LedgerError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        // The client's fault, and the message says which part.
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // The code is taken; picking another is the client's move.
                ledger::LedgerError::AccountExists(_) => StatusCode::CONFLICT,
                // Well-formed, but refers to something that is not there.
                ledger::LedgerError::NoSuchAccount(_) | ledger::LedgerError::AccountClosed(_) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                _ => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        // Backpressure. Retryable, and saying so is the difference between a
        // client that backs off and one that hammers.
        CommandError::Pool(e @ spa_control::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        // Sustained contention on one aggregate. Also retryable, but it is a
        // conflict rather than a capacity problem.
        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            spa_i18n::Message::new(spa_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        other => {
            tracing::error!(error = %other, "ledger command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                spa_i18n::Message::new(spa_control::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}

// ---------------------------------------------------------------------------
// Charts of accounts
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChartView {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    accounts: usize,
    /// Enough to show the chart before installing it, which is the
    /// "modify before installing" story's first half — after installing, every
    /// account is renameable and closeable.
    preview: Vec<ChartAccountView>,
}

#[derive(Debug, Serialize)]
struct ChartAccountView {
    code: &'static str,
    name: &'static str,
    kind: &'static str,
}

/// The catalogue, in the caller's language.
async fn list_charts(Language(locale): Language) -> Json<Vec<ChartView>> {
    Json(
        ledger::CHARTS
            .iter()
            .map(|c| ChartView {
                id: c.id,
                name: c.name(locale),
                description: c.description(locale),
                accounts: c.accounts.len(),
                preview: c
                    .accounts
                    .iter()
                    .map(|a| ChartAccountView {
                        code: a.code,
                        name: a.name(locale),
                        kind: a.kind.as_str(),
                    })
                    .collect(),
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize)]
struct InstallChart {
    /// The chart's id, from `GET /v1/ledger/charts`.
    template: String,
    currency: String,
}

#[derive(Debug, Serialize)]
struct ChartInstalled {
    opened: usize,
    /// Accounts that were already there. Installing twice is not an error.
    skipped: usize,
}

async fn install_chart(
    tenant: Tenant,
    Language(locale): Language,
    Json(body): Json<InstallChart>,
) -> Result<Json<ChartInstalled>, Problem> {
    let chart = ledger::chart(&body.template).ok_or_else(|| {
        bad_request(
            crate::messages::UNKNOWN_CHART,
            "chart",
            &body.template,
            locale,
        )
    })?;
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            crate::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let installed = ledger::install_chart(&tenant.db, chart, currency, locale, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;

    Ok(Json(ChartInstalled {
        opened: installed.opened,
        skipped: installed.skipped,
    }))
}
