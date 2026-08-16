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
use ledger::{AccountKind, BalancedLines, Line};
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
        .routes(routes!(list_accounts, open_account))
        .routes(routes!(post_entry))
        .routes(routes!(reverse_entry))
        .routes(routes!(trial_balance))
        .routes(routes!(books, close_books))
        .routes(routes!(vat_rates, set_vat_rates))
        // Unauthenticated on purpose: a signup form needs to show the choices
        // before anyone has an account. It is product information, not data.
        .routes(routes!(list_charts))
        .routes(routes!(install_chart))
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "code": "1100", "name": "Trade receivables", "kind": "asset", "currency": "SAR"
}))]
struct NewAccount {
    /// The account code, as the tenant numbers their chart.
    code: String,
    name: String,
    /// `asset`, `liability`, `equity`, `revenue` or `expense`. Decides which
    /// side of the account a positive balance sits on.
    kind: String,
    /// ISO 4217. An account holds one currency, and postings in another are
    /// refused.
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct AccountView {
    code: String,
    name: String,
    kind: &'static str,
    balance: i64,
    currency: String,
    closed: bool,
    postings: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "JE-2026-0001",
    "occurred_on": "2026-08-15T00:00:00Z",
    "memo": "Opening the bank account",
    "lines": [
        { "account": "1000", "amount": { "minor": 100_000, "currency": "SAR" } },
        { "account": "3000", "amount": { "minor": -100_000, "currency": "SAR" } }
    ]
}))]
struct NewEntry {
    /// The client's own identifier for the entry. Posting the same one twice is
    /// a no-op, which is what makes a retried request safe.
    id: String,
    /// The date the business treats this as happening — not a clock reading.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    occurred_on: Timestamp,
    #[serde(default)]
    memo: String,
    /// Must sum to zero, in one currency. An unbalanced set is a 400 carrying
    /// the difference.
    lines: Vec<NewLine>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewLine {
    /// An account code from `GET /v1/ledger/accounts`.
    account: String,
    /// Positive debits, negative credits.
    amount: Amount,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct EntryPosted {
    id: String,
    /// Where it landed in the log. A client that wants to read its own write
    /// back passes this as `?consistent_after=`.
    position: Option<i64>,
    lines: usize,
}

#[derive(Debug, Serialize, ToSchema)]
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

/// Every account and what it holds.
///
/// Balances are summed from the postings rather than maintained, so there is no
/// second number that can be wrong.
#[utoipa::path(
    get,
    path = "/v1/ledger/accounts",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = Vec<AccountView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, not yours, or the ledger module is not enabled here", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The projection did not reach `consistent_after` in time. Retryable.", body = Problem),
    ),
)]
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

/// Open an account.
#[utoipa::path(
    post,
    path = "/v1/ledger/accounts",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = NewAccount,
    responses(
        (status = CREATED, description = "Opened."),
        (status = BAD_REQUEST, description = "An unusable code, an unknown kind, or an unknown currency", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "That code is already open", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
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

/// Post a journal entry.
///
/// `id` is the client's own identifier, and posting the same one twice is a
/// no-op — which is what makes a retried request safe without an
/// `Idempotency-Key` header.
#[utoipa::path(
    post,
    path = "/v1/ledger/entries",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = NewEntry,
    responses(
        (status = OK, description = "Posted, or already posted under this id.", body = EntryPosted),
        (status = BAD_REQUEST, description = "Lines that do not sum to zero, mixed currencies, or an unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Sustained contention on this entry. Retryable.", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "An account that does not exist or is closed", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
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

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "JE-2026-0002", "occurred_on": "2026-08-16T00:00:00Z", "memo": "Reverses JE-2026-0001"
}))]
struct NewReversal {
    /// The client's own identifier for the *reversing* entry. Sending the same
    /// one twice is a no-op; a different one against an already-reversed entry
    /// is refused.
    id: String,
    /// When the correction is treated as happening. Usually today, not the date
    /// of the mistake — reversing into a closed period is how a filed return
    /// stops matching the books.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    occurred_on: Timestamp,
    #[serde(default)]
    memo: String,
}

/// Undoes an entry by posting its opposite.
///
/// A `POST`, not a `DELETE`: nothing is removed. The books end up showing both
/// the mistake and the correction, which is what makes them auditable.
#[utoipa::path(
    post,
    path = "/v1/ledger/entries/{entry}/reversal",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("entry" = String, Path, description = "The id of the entry being undone."),
    ),
    request_body = NewReversal,
    responses(
        (status = OK, description = "Reversed, or already reversed by this id.", body = EntryPosted),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Already reversed by a *different* entry", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such entry", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
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

/// Debits and credits per currency, and whether they agree.
///
/// `balances: false` on a healthy system is impossible — the entry type refuses
/// an unbalanced set — so it is worth alerting on.
#[utoipa::path(
    get,
    path = "/v1/ledger/trial-balance",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One row per currency.", body = Vec<TrialBalanceView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
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
                // Well-formed, but refers to something that is not there — or
                // to a period nobody may write into any more.
                ledger::LedgerError::NoSuchAccount(_)
                | ledger::LedgerError::AccountClosed(_)
                | ledger::LedgerError::NoSuchEntry(_)
                | ledger::LedgerError::PeriodClosed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
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
// Closing the books
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "closed_before": "2026-02-01T00:00:00Z" }))]
struct BooksView {
    /// **The first instant still open.** Everything strictly before it is final
    /// and no entry may be dated into it — not a journal entry, not an invoice's
    /// tax point, not a credit note's.
    ///
    /// Closing January is `2026-02-01T00:00:00Z`. Exclusive, like the VAT
    /// return's `until`, because "closed through 31 January" is a comparison
    /// somebody gets wrong by exactly one day.
    ///
    /// `null` on a tenant that has never closed a period, and the way to reopen
    /// everything.
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    closed_before: Option<Timestamp>,
}

/// How far the books are closed.
#[utoipa::path(
    get,
    path = "/v1/ledger/books",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = BooksView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn books(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<BooksView>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let books = ledger::period::books(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    Ok(Json(BooksView {
        closed_before: books.closed_before,
    }))
}

/// Close the books, or reopen them.
///
/// After this, an entry dated before `closed_before` is refused — including an
/// invoice with a back-dated tax point and a credit note dated into a quarter
/// that has already been declared. Corrections go into the period that is open,
/// which is where an auditor expects to find them.
///
/// **Moving it backwards reopens.** An accountant who closes the wrong month has
/// to be able to put it right, and a system that refuses is one they route
/// around by editing the database. Sending `null` reopens everything.
///
/// `ManageAccounts`, not `ManageTenant`: declaring the numbers final is the
/// accountant's call, and it is not something a clerk posting entries should be
/// able to do to them.
#[utoipa::path(
    put,
    path = "/v1/ledger/books",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = BooksView,
    responses(
        (status = NO_CONTENT, description = "Closed. Entries dated before it are refused from now on."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn close_books(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<BooksView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    ledger::period::close(
        &mut conn,
        body.closed_before,
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
// What the business charges
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "standard": 1500 }))]
struct RatesView {
    /// The standard rate in **basis points**. 1500 is 15% (Saudi Arabia), 500
    /// is 5% (the UAE).
    ///
    /// Zero-rated and exempt are 0% by definition and not configurable — a
    /// jurisdiction that taxed an exempt supply would not call it exempt.
    ///
    /// Applies to invoices issued **from now on**. Every document already issued
    /// carries the rate it was issued under, so changing this cannot restate a
    /// filed return.
    standard: i32,
}

/// What this business charges VAT at.
#[utoipa::path(
    get,
    path = "/v1/ledger/vat-rates",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = RatesView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn vat_rates(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<RatesView>, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    let rates = ledger::Rates::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    Ok(Json(RatesView {
        standard: rates.standard,
    }))
}

/// Set what this business charges VAT at.
///
/// **Not retrospective.** Every invoice already issued carries the rate it was
/// issued under, because the rate went into the event as a value (L5). Changing
/// this changes the next invoice and nothing before it — which is what stops a
/// rate change silently restating a return that has been filed.
///
/// ponytail: a country module would set this at signup rather than leaving it to
/// a settings screen. Until there is one, the shipped default is Saudi Arabia's
/// and this is how anyone else corrects it.
#[utoipa::path(
    put,
    path = "/v1/ledger/vat-rates",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = RatesView,
    responses(
        (status = NO_CONTENT, description = "Set. Applies to the next invoice, not to past ones."),
        (status = BAD_REQUEST, description = "A negative rate, or one over 100%", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_vat_rates(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<RatesView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant, &ledger::module_id(), locale)?;

    // A negative rate would credit VAT payable on every sale; one over 100%
    // would charge more tax than the supply. Neither is a rate anywhere.
    if !(0..=10_000).contains(&body.standard) {
        return Err(bad_request(
            crate::messages::UNUSABLE_VAT_RATE,
            "rate",
            &body.standard.to_string(),
            locale,
        ));
    }

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale))?;
    spa_eventlog::configuration::set(
        &mut conn,
        ledger::Rates::KEY,
        &ledger::Rates {
            standard: body.standard,
        },
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Charts of accounts
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
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

#[derive(Debug, Serialize, ToSchema)]
struct ChartAccountView {
    code: &'static str,
    name: &'static str,
    kind: &'static str,
}

/// Ready-made charts of accounts, in the caller's language.
///
/// Unauthenticated: a signup form needs to show the choices before anyone has an
/// account. Every account is renameable and closeable after installing, so the
/// `preview` is a starting point rather than a commitment.
#[utoipa::path(
    get,
    path = "/v1/ledger/charts",
    tag = "ledger",
    security(),
    responses((status = OK, body = Vec<ChartView>)),
)]
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

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "template": "sa_trading", "currency": "SAR" }))]
struct InstallChart {
    /// The chart's id, from `GET /v1/ledger/charts`.
    template: String,
    /// The currency every account is opened in.
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChartInstalled {
    opened: usize,
    /// Accounts that were already there. Installing twice is not an error.
    skipped: usize,
}

/// Open every account in a ready-made chart.
///
/// Installing twice is not an error: accounts that are already open are counted
/// as `skipped` and left exactly as they are, names included.
#[utoipa::path(
    post,
    path = "/v1/ledger/chart",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = InstallChart,
    responses(
        (status = OK, body = ChartInstalled),
        (status = BAD_REQUEST, description = "No such chart, or an unknown currency", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
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
