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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use ledger::{AccountKind, BalancedLines, Line};
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
            "/v1/tenants/{slug}/ledger/accounts",
            routing::get(list_accounts).post(open_account),
        )
        .route(
            "/v1/tenants/{slug}/ledger/entries",
            routing::post(post_entry),
        )
        .route(
            "/v1/tenants/{slug}/ledger/entries/{entry}/reversal",
            routing::post(reverse_entry),
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
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<AccountView>>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, ledger::GROUP_NAME, locale)
        .await?;

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
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewAccount>,
) -> Result<impl IntoResponse, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;
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

    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::CREATED)
}

async fn post_entry(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewEntry>,
) -> Result<Json<EntryPosted>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;
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

    // Ask the worker to look at this tenant now. Without it, the first write
    // after a quiet period waits out the idle backoff before anything projects
    // it, and `?consistent_after=` would time out on a healthy system.
    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(EntryPosted {
        id: body.id,
        position: committed.at.map(spa_types::LogPosition::get),
        lines: line_count,
    }))
}

#[derive(Debug, Deserialize)]
struct NewReversal {
    /// The client's own identifier for the *reversing* entry. Sending the same
    /// one twice is a no-op; a different one against an already-reversed entry
    /// is refused.
    id: String,
    /// When the correction is treated as happening. Usually today, not the date
    /// of the mistake — reversing into a closed period is how a filed return
    /// stops matching the books.
    occurred_on: Timestamp,
    #[serde(default)]
    memo: String,
}

/// Undoes an entry by posting its opposite.
///
/// A `POST`, not a `DELETE`: nothing is removed. The books end up showing both
/// the mistake and the correction, which is what makes them auditable.
async fn reverse_entry(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewReversal>,
) -> Result<Json<EntryPosted>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;

    let original = parse_id(params.get("entry").map_or("", String::as_str), locale)?;
    let reversal = parse_id(&body.id, locale)?;

    let committed = ledger::reverse_entry(
        &tenant.db,
        &original,
        &reversal,
        body.occurred_on,
        &body.memo,
        &metadata(&tenant),
    )
    .await
    .map_err(|e| ledger_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(EntryPosted {
        id: body.id,
        position: committed.at.map(spa_types::LogPosition::get),
        // The reversal has exactly the lines the original had. Reporting the
        // count would mean loading it again to say something the client already
        // knows.
        lines: committed.events.len(),
    }))
}

async fn trial_balance(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<TrialBalanceView>>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, ledger::GROUP_NAME, locale)
        .await?;

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
                // Both mean "look at what is there now and decide again": a
                // code somebody else took, and an entry somebody else undid.
                ledger::LedgerError::AccountExists(_)
                | ledger::LedgerError::AlreadyReversed { .. } => StatusCode::CONFLICT,
                // Well-formed, but refers to something that is not there.
                ledger::LedgerError::NoSuchAccount(_)
                | ledger::LedgerError::AccountClosed(_)
                | ledger::LedgerError::NoSuchEntry(_) => StatusCode::UNPROCESSABLE_ENTITY,
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
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<InstallChart>,
) -> Result<Json<ChartInstalled>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;
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

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(ChartInstalled {
        opened: installed.opened,
        skipped: installed.skipped,
    }))
}
