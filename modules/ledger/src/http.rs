//! The ledger's HTTP surface.
//!
//! Translation only: the aggregates, the invariant and the read models are the
//! module; this turns a request into a call and a result into JSON.
//!
//! # Why it is in the module and not in `erp-api`
//!
//! Because a module you cannot read in one place is not a module. This file used
//! to live in the composition root, which meant the ledger's routes were written
//! by something the ledger could not see, and adding an endpoint meant editing
//! two crates. What made that necessary was the extractors — and those are in
//! [`erp_web`] now, below every module, so a module can name them.
//!
//! `erp-api` still decides what is *mounted*, which is the part that belongs to
//! the composition root.

use crate::messages;
use crate::{AccountKind, BalancedLines, Line};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use erp_eventlog::ExecuteError;
use erp_i18n::{Catalog as _, Locale, Localize, Message, MessageArg};
use erp_tenant::CommandError;
use erp_types::{CurrencyCode, Timestamp};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{After, Paged, Query};
use erp_web::{Allowed, Anonymous, IdempotencyKey, Language, ManageAccounts, PostEntries, Read};
use erp_web::{Amount, Json, bad_request, creating, metadata, parse_id, require_module};
use erp_web::{Consistency, nudge};
use erp_web::{IfMatch, Versioned};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_accounts, open_account))
        .routes(routes!(post_entry, list_entries))
        .routes(routes!(reverse_entry))
        .routes(routes!(trial_balance))
        .routes(routes!(books))
        .routes(routes!(fiscal_calendar, set_fiscal_calendar))
        .routes(routes!(periods))
        .routes(routes!(close_period))
        .routes(routes!(reopen_period))
        .routes(routes!(year))
        .routes(routes!(close_year))
        .routes(routes!(reopen_year))
        .routes(routes!(closing_accounts, set_closing_accounts))
        .routes(routes!(list_cost_centers, open_cost_center))
        .routes(routes!(rename_cost_center))
        .routes(routes!(close_cost_center))
        .routes(routes!(profit_and_loss_by_cost_center))
        .routes(routes!(balances))
        .routes(routes!(profit_and_loss))
        .routes(routes!(balance_sheet))
        .routes(routes!(journal_entry))
        .routes(routes!(vat_rates, set_vat_rates))
        // Unauthenticated on purpose: a signup form needs to show the choices
        // before anyone has an account. It is product information, not data.
        .routes(routes!(list_charts))
        .routes(routes!(install_chart))
        .routes(routes!(preview_chart))
}

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
    erp_i18n::Composite::new(&[&crate::CATALOG, &erp_web::CATALOG]);

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
    "occurred_on": "2026-08-15T00:00:00Z",
    "memo": "Opening the bank account",
    "lines": [
        { "account": "1000", "amount": { "minor": 100_000, "currency": "SAR" } },
        { "account": "3000", "amount": { "minor": -100_000, "currency": "SAR" } }
    ]
}))]
struct NewEntry {
    /// The date the business treats this as happening — not a clock reading.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    occurred_on: Timestamp,
    #[serde(default)]
    memo: String,
    /// Must sum to zero, in one currency. An unbalanced set is a 400 carrying
    /// the difference.
    lines: Vec<NewEntryLine>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewEntryLine {
    /// An account code from `GET /v1/ledger/accounts`.
    account: String,
    /// Positive debits, negative credits.
    amount: Amount,
    #[serde(default)]
    memo: Option<String>,
    /// Which department this line is for: a cost center from
    /// `GET /v1/ledger/cost-centers`, or an open branch. Absent, the entry's
    /// branch.
    #[serde(default)]
    cost_center: Option<String>,
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let accounts = crate::account_balances(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let code = parse_id(&body.code, locale)?;
    let kind: AccountKind = body.kind.parse().map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_ACCOUNT_KIND,
            "kind",
            &body.kind,
            locale,
        )
    })?;
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    crate::open_account(
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
    key: IdempotencyKey,
    Json(body): Json<NewEntry>,
) -> Result<Json<EntryPosted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();

    let mut lines = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        let account = parse_id(&line.account, locale)?;
        let mut parsed = Line::new(account, line.amount.parse(locale)?);
        parsed.memo.clone_from(&line.memo);
        parsed.cost_center = line
            .cost_center
            .as_deref()
            .map(|center| parse_id(center, locale))
            .transpose()?;
        lines.push(parsed);
    }

    // The type refuses an unbalanced set, so this is where a client's mistake
    // becomes a 400 with the difference in it.
    let balanced = BalancedLines::new(lines)
        .map_err(|e| ApiError::BadRequest(e.message()).into_problem(locale, &CATALOG))?;
    let line_count = balanced.len();
    within_limits(&tenant, &balanced, locale).await?;

    let committed = crate::post_entry(
        &tenant.db,
        &id,
        body.occurred_on,
        &body.memo,
        balanced,
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| ledger_problem(&e, locale))?;

    // Ask the worker to look at this tenant now. Without it, the first write
    // after a quiet period waits out the idle backoff before anything projects
    // it, and `?consistent_after=` would time out on a healthy system.
    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(EntryPosted {
        id: id.to_string(),
        position: committed.at.map(erp_types::LogPosition::get),
        lines: line_count,
    }))
}

/// **The amount the edge could not know.** `Allowed<PostEntries>` decided
/// before the body was read, so a limit like "a bookkeeper may post entries
/// under ten thousand riyals" — the example `erp_tenant::roles` names — is
/// narrowed here, by the handler that has the lines. Both routes that post an
/// entry by hand call it: a reversal is an entry of the same size as the one it
/// undoes. Debits, because debits equal credits and either would do; the debit
/// side is what an accountant means by the size of an entry.
async fn within_limits(
    tenant: &Allowed<PostEntries>,
    lines: &BalancedLines,
    locale: Locale,
) -> Result<(), Problem> {
    let size = lines.total_debits().map_err(|_| {
        ApiError::BadRequest(erp_i18n::Message::new(crate::messages::ENTRY_TOO_LARGE))
            .into_problem(locale, &CATALOG)
    })?;
    tenant
        .still_permits(
            Some(&crate::module_id()),
            [(erp_tenant::limits::AMOUNT, erp_rules::Value::Money(size))],
            locale,
        )
        .await
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "occurred_on": "2026-08-16T00:00:00Z", "memo": "Reverses JE-2026-0001"
}))]
struct NewReversal {
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
    key: IdempotencyKey,
    Json(body): Json<NewReversal>,
) -> Result<Json<EntryPosted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let original = parse_id(params.get("entry").map_or("", String::as_str), locale)?;
    let reversal = key.id().clone();
    let undone = crate::posted_lines(&tenant.db, &original)
        .await
        .map_err(|e| ledger_problem(&e, locale))?;
    within_limits(&tenant, &undone, locale).await?;

    let committed = crate::reverse_entry(
        &tenant.db,
        &original,
        &reversal,
        body.occurred_on,
        &body.memo,
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| ledger_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(EntryPosted {
        id: reversal.to_string(),
        position: committed.at.map(erp_types::LogPosition::get),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

    let rows = crate::trial_balance(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;

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
// The fiscal calendar, and the statements read by it
// ---------------------------------------------------------------------------

/// The calendar a business keeps its periods by.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "starts_on": "2026-01-01", "pattern": "monthly" }))]
struct FiscalCalendarView {
    /// The first day of a fiscal year, once. Every other year starts on its
    /// anniversary — or, for a week pattern, on that weekday nearest to it.
    #[schema(value_type = String, example = "2026-01-01")]
    starts_on: chrono::NaiveDate,
    /// `monthly`, `quarterly`, `4-4-5`, `4-5-4`, `5-4-4` or `yearly`.
    pattern: String,
}

/// The calendar in force today, and every segment the tenant has kept.
#[derive(Debug, Serialize, ToSchema)]
struct FiscalCalendarsView {
    #[schema(value_type = String, example = "2026-01-01")]
    starts_on: chrono::NaiveDate,
    pattern: String,
    /// In order. A year is generated by the last segment starting on or before
    /// it, so a closed year keeps the periods it was closed under.
    segments: Vec<FiscalCalendarView>,
}

/// One period of one fiscal year.
#[derive(Debug, Serialize, ToSchema)]
struct PeriodView {
    /// `2026-P03`: the fiscal year, named by the calendar year it starts in,
    /// and the period's number in it.
    id: String,
    year: i32,
    index: u32,
    #[schema(value_type = String)]
    from: chrono::NaiveDate,
    /// Exclusive — the first day of the next period.
    #[schema(value_type = String)]
    until: chrono::NaiveDate,
    /// Whether the books are closed through the end of it.
    closed: bool,
}

#[derive(Debug, Deserialize)]
struct PeriodsQuery {
    year: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct BalancesQuery {
    #[serde(default)]
    until: Option<Timestamp>,
    #[serde(default)]
    all: bool,
}

#[derive(Debug, Deserialize)]
struct RangeQuery {
    #[serde(default)]
    from: Option<Timestamp>,
    #[serde(default)]
    until: Option<Timestamp>,
    period: Option<String>,
    branch: Option<String>,
    cost_center: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Debug, Deserialize)]
struct AsAtQuery {
    #[serde(default)]
    as_at: Option<Timestamp>,
    period: Option<String>,
    #[serde(default)]
    all: bool,
}

/// An account on a statement.
#[derive(Debug, Serialize, ToSchema)]
struct StatementLineView {
    code: String,
    name: String,
    /// Minor units, on the kind's natural side: a revenue account's credit
    /// balance and an asset's debit balance are both positive here.
    amount: i64,
    postings: i64,
}

#[derive(Debug, Serialize, ToSchema)]
struct ProfitAndLossCurrency {
    currency: String,
    revenue: Vec<StatementLineView>,
    expenses: Vec<StatementLineView>,
    total_revenue: i64,
    total_expenses: i64,
    /// Revenue less expenses. A loss is negative.
    result: i64,
}

/// What the trading accounts did in a range: one statement per currency.
#[derive(Debug, Serialize, ToSchema)]
struct ProfitAndLossView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// Exclusive.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_center: Option<String>,
    currencies: Vec<ProfitAndLossCurrency>,
}

/// One cost center's column of the profit and loss.
#[derive(Debug, Serialize, ToSchema)]
struct CostCenterColumn {
    /// `null` for lines that named none and had no branch to fall to.
    cost_center: Option<String>,
    currencies: Vec<ProfitAndLossCurrency>,
}

/// The profit and loss cut by cost center.
#[derive(Debug, Serialize, ToSchema)]
struct ProfitAndLossByCostCenterView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// Ordered by cost center; the unassigned column, when there is one, last.
    cost_centers: Vec<CostCenterColumn>,
}

/// A cost center as the list shows it.
#[derive(Debug, Serialize, ToSchema)]
struct CostCenterView {
    id: String,
    name: String,
    closed: bool,
    /// Lines that named it, or fell to it as their entry's branch.
    postings: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "id": "marketing", "name": "التسويق" }))]
struct NewCostCenter {
    /// Yours, and the same namespace as branches: a branch is a cost center
    /// already, so its id cannot be opened as one.
    id: String,
    name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "name": "Marketing" }))]
struct CostCenterName {
    name: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct BalanceSheetCurrency {
    currency: String,
    assets: Vec<StatementLineView>,
    liabilities: Vec<StatementLineView>,
    equity: Vec<StatementLineView>,
    /// Revenue less expenses since the fiscal year started, shown as equity
    /// because no closing entry has moved it into retained earnings.
    current_year_result: i64,
    /// The same for every year before this one that was never closed.
    prior_years_result: i64,
    total_assets: i64,
    total_liabilities: i64,
    /// The equity accounts plus the two results. Equals `total_assets` less
    /// `total_liabilities`, or the sheet would not have been shown.
    total_equity: i64,
}

/// What the business holds and owes as at an instant: one sheet per currency.
#[derive(Debug, Serialize, ToSchema)]
struct BalanceSheetView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    as_at: Timestamp,
    /// The first day of the fiscal year `as_at` falls in, which is where
    /// `current_year_result` starts counting.
    #[schema(value_type = String)]
    fiscal_year_started: chrono::NaiveDate,
    currencies: Vec<BalanceSheetCurrency>,
}

#[derive(Debug, Serialize, ToSchema)]
struct JournalLineView {
    account: String,
    name: String,
    debit: i64,
    credit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    memo: Option<String>,
    /// What the line named, or the entry's branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_center: Option<String>,
}

/// One entry with its lines, as the journal lists it.
#[derive(Debug, Serialize, ToSchema)]
struct JournalEntryRecord {
    id: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    occurred_on: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    recorded_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    currency: String,
    /// A year's closing entry, or its reversal: in every balance, out of the
    /// profit and loss.
    closing: bool,
    lines: Vec<JournalLineView>,
}

#[derive(Debug, Deserialize)]
struct JournalQuery {
    #[serde(default)]
    from: Option<Timestamp>,
    #[serde(default)]
    until: Option<Timestamp>,
    account: Option<String>,
    branch: Option<String>,
    cost_center: Option<String>,
    #[serde(flatten)]
    page: After,
}

/// The fiscal calendar: when this business's periods begin and end.
///
/// Calendar months from 1 January unless it was set.
#[utoipa::path(
    get,
    path = "/v1/ledger/fiscal-calendar",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = FiscalCalendarsView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn fiscal_calendar(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<FiscalCalendarsView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let calendars = crate::fiscal::fiscal_calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let current = calendars.current(clock.day(chrono::Utc::now()));
    Ok(Json(FiscalCalendarsView {
        starts_on: current.starts_on,
        pattern: current.pattern.as_str().to_owned(),
        segments: calendars
            .segments
            .iter()
            .map(|s| FiscalCalendarView {
                starts_on: s.starts_on,
                pattern: s.pattern.as_str().to_owned(),
            })
            .collect(),
    }))
}

/// Set the fiscal calendar.
///
/// A start date and a pattern: `monthly`, `quarterly`, `4-4-5`, `4-5-4`,
/// `5-4-4` or `yearly`. Every period is generated from the two — `GET
/// /v1/ledger/periods` shows them — so a period is never typed in by hand. A
/// week pattern starts each year on the start date's weekday nearest its
/// anniversary and puts the 53rd week, when it comes, in the last period.
///
/// **While nothing is closed it replaces the calendar.** Once a period is
/// closed it becomes a new *segment*, in force from `starts_on` on, which must
/// be in open time and on a fiscal-year boundary of the calendar before it —
/// closed years keep the periods they were closed under. `ManageAccounts`, like
/// closing a period, because it is the accountant's call.
#[utoipa::path(
    put,
    path = "/v1/ledger/fiscal-calendar",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = FiscalCalendarView,
    responses(
        (status = NO_CONTENT, description = "Set. Periods and statements read by it from now on."),
        (status = BAD_REQUEST, description = "A pattern this build does not know — `ledger.not_a_pattern`; a start off a fiscal-year boundary — `ledger.not_a_year_start`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "The start is inside closed time — `ledger.calendar_locked`", body = Problem),
    ),
)]
async fn set_fiscal_calendar(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<FiscalCalendarView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let pattern = body
        .pattern
        .parse::<crate::Pattern>()
        .map_err(|_| bad_request(messages::NOT_A_PATTERN, "pattern", &body.pattern, locale))?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let calendar = crate::FiscalCalendar {
        starts_on: body.starts_on,
        pattern,
    };
    match crate::fiscal::set_fiscal_calendar(
        &mut conn,
        calendar,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(crate::CalendarError::Locked) => {
            let books = crate::period::books(&mut conn)
                .await
                .map_err(|e| config_problem(&e, locale))?;
            Err(Problem::new(
                StatusCode::CONFLICT,
                &Message::new(messages::CALENDAR_LOCKED).with(
                    "closed_before",
                    MessageArg::text(
                        books
                            .closed_before
                            .map(|c| c.to_rfc3339())
                            .unwrap_or_default(),
                    ),
                ),
                locale,
                &CATALOG,
            ))
        }
        Err(crate::CalendarError::NotAYearStart { starts_on, next }) => Err(Problem::new(
            StatusCode::BAD_REQUEST,
            &Message::new(messages::NOT_A_YEAR_START)
                .with("starts_on", MessageArg::text(starts_on.to_string()))
                .with("next", MessageArg::text(next.to_string())),
            locale,
            &CATALOG,
        )),
        Err(crate::CalendarError::Config(e)) => Err(config_problem(&e, locale)),
    }
}

/// The periods of one fiscal year, and which are closed.
///
/// The year is named by the calendar year it starts in; absent, the one today
/// falls in.
#[utoipa::path(
    get,
    path = "/v1/ledger/periods",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("year" = Option<i32>, Query, description = "The fiscal year, by the calendar year it starts in. Defaults to the one today falls in."),
    ),
    responses(
        (status = OK, body = Vec<PeriodView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn periods(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Query(query): Query<PeriodsQuery>,
) -> Result<Json<Vec<PeriodView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let fiscal = crate::fiscal::fiscal_calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let books = crate::period::books(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    let year = query
        .year
        .unwrap_or_else(|| fiscal.fiscal_year_of(clock.day(chrono::Utc::now())));
    Ok(Json(
        fiscal
            .periods(year)
            .into_iter()
            .map(|p| PeriodView {
                closed: books
                    .closed_before
                    .is_some_and(|closed| closed >= clock.start_of(p.until)),
                id: p.id,
                year: p.year,
                index: p.index,
                from: p.from,
                until: p.until,
            })
            .collect(),
    ))
}

/// Every account and what it held **as at** an instant.
///
/// `GET /v1/ledger/accounts` with a date: every posting before `until` counts
/// and nothing after it. Absent, all-time. Accounts with nothing to show are
/// left out unless `all` is asked for.
#[utoipa::path(
    get,
    path = "/v1/ledger/balances",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("until" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Exclusive. Postings dated before this count. Absent means all-time."),
        ("all" = Option<bool>, Query, description = "Include accounts with no balance and no postings."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<AccountView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn balances(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<BalancesQuery>,
) -> Result<Json<Vec<AccountView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let accounts = crate::balances_at(&mut conn, query.until)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    Ok(Json(
        accounts
            .into_iter()
            .filter(|a| query.all || a.postings > 0 || a.balance.minor() != 0)
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

/// The instants a statement's range means, from a period or a pair of them.
///
/// A period is resolved on the tenant's own calendar, so `2026-P03` starts at
/// local midnight in Riyadh and not in UTC.
async fn range_of(
    conn: &mut sqlx::PgConnection,
    period: Option<&str>,
    from: Option<Timestamp>,
    until: Option<Timestamp>,
    locale: Locale,
) -> Result<(Timestamp, Timestamp), Problem> {
    if let Some(id) = period {
        let fiscal = crate::fiscal::fiscal_calendar(&mut *conn)
            .await
            .map_err(|e| config_problem(&e, locale))?;
        let clock = erp_eventlog::configuration::calendar(&mut *conn)
            .await
            .map_err(|e| config_problem(&e, locale))?;
        let period = fiscal
            .period(id)
            .ok_or_else(|| bad_request(messages::NO_SUCH_PERIOD, "period", id, locale))?;
        return Ok((clock.start_of(period.from), clock.start_of(period.until)));
    }
    match (from, until) {
        (Some(from), Some(until)) if from < until => Ok((from, until)),
        _ => Err(ApiError::BadRequest(Message::new(messages::NOT_A_RANGE))
            .into_problem(locale, &CATALOG)),
    }
}

/// Presents a line on its kind's natural side.
fn natural(line: crate::StatementLine) -> StatementLineView {
    let signed = line.balance.minor();
    StatementLineView {
        code: line.code,
        name: line.name,
        amount: if line.kind.is_debit_normal() {
            signed
        } else {
            -signed
        },
        postings: line.postings,
    }
}

/// Profit and loss: what the trading accounts did over a range.
///
/// One statement per currency, because postings carry a currency and nothing
/// here converts. Give a `period` (`2026-P03`) or `from` and `until`, exclusive
/// at the far end; `branch` narrows it to one branch's postings, and a member
/// confined to a branch gets theirs. Accounts with nothing in the range are left
/// out unless `all` is asked for.
#[utoipa::path(
    get,
    path = "/v1/ledger/statements/profit-and-loss",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("period" = Option<String>, Query, description = "A period of the fiscal calendar, `2026-P03`. Instead of `from` and `until`."),
        ("from" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Inclusive."),
        ("until" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Exclusive."),
        ("branch" = Option<String>, Query, description = "One branch's postings. Absent means the company; a confined member gets their branch."),
        ("cost_center" = Option<String>, Query, description = "One cost center's lines — a cost center or a branch. Beside `branch`, not instead of it: a confined member still sees only their branch."),
        ("all" = Option<bool>, Query, description = "Include accounts with nothing in the range."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = ProfitAndLossView),
        (status = BAD_REQUEST, description = "No range, or a backwards one — `ledger.not_a_range`; a period this calendar does not have — `ledger.no_such_period`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not permitted, or a branch that is not one of yours — `access.wrong_branch`", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn profit_and_loss(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<RangeQuery>,
) -> Result<Json<ProfitAndLossView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let (from, until) = range_of(
        &mut conn,
        query.period.as_deref(),
        query.from,
        query.until,
        locale,
    )
    .await?;
    let lines = crate::profit_and_loss(
        &mut conn,
        from,
        until,
        branch.as_deref(),
        query.cost_center.as_deref(),
    )
    .await
    .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    Ok(Json(ProfitAndLossView {
        from,
        until,
        period: query.period,
        branch,
        cost_center: query.cost_center,
        currencies: currencies_of(lines, query.all),
    }))
}

/// One profit and loss per currency, from the trading lines.
fn currencies_of(lines: Vec<crate::StatementLine>, all: bool) -> Vec<ProfitAndLossCurrency> {
    let mut by_currency: std::collections::BTreeMap<String, ProfitAndLossCurrency> =
        std::collections::BTreeMap::new();
    for line in lines {
        if !all && line.postings == 0 && line.balance.minor() == 0 {
            continue;
        }
        let currency = line.balance.currency().to_string();
        let statement =
            by_currency
                .entry(currency.clone())
                .or_insert_with(|| ProfitAndLossCurrency {
                    currency,
                    revenue: Vec::new(),
                    expenses: Vec::new(),
                    total_revenue: 0,
                    total_expenses: 0,
                    result: 0,
                });
        let kind = line.kind;
        let shown = natural(line);
        if kind == AccountKind::Revenue {
            statement.total_revenue += shown.amount;
            statement.revenue.push(shown);
        } else {
            statement.total_expenses += shown.amount;
            statement.expenses.push(shown);
        }
        statement.result = statement.total_revenue - statement.total_expenses;
    }
    by_currency.into_values().collect()
}

/// The profit and loss cut by cost center.
///
/// One column per cost center — what its lines named, or the branch they fell
/// to — with the same shape as `profit-and-loss` inside each, and lines that
/// had neither last, unassigned. The same range and branch parameters;
/// closing entries left out the same way. The balance sheet has no such cut,
/// because a cost center's books do not balance on their own.
#[utoipa::path(
    get,
    path = "/v1/ledger/statements/profit-and-loss/by-cost-center",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("period" = Option<String>, Query, description = "A period of the fiscal calendar, `2026-P03`. Instead of `from` and `until`."),
        ("from" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Inclusive."),
        ("until" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Exclusive."),
        ("branch" = Option<String>, Query, description = "One branch's postings. Absent means the company; a confined member gets their branch."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = ProfitAndLossByCostCenterView),
        (status = BAD_REQUEST, description = "No range, or a backwards one — `ledger.not_a_range`; a period this calendar does not have — `ledger.no_such_period`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn profit_and_loss_by_cost_center(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<RangeQuery>,
) -> Result<Json<ProfitAndLossByCostCenterView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let (from, until) = range_of(
        &mut conn,
        query.period.as_deref(),
        query.from,
        query.until,
        locale,
    )
    .await?;
    let rows = crate::profit_and_loss_by_cost_center(&mut conn, from, until, branch.as_deref())
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    // Rows arrive ordered by cost center, unassigned last; keep that order.
    let mut columns: Vec<CostCenterColumn> = Vec::new();
    let mut lines: Vec<crate::StatementLine> = Vec::new();
    let mut current: Option<Option<String>> = None;
    for row in rows {
        if current.as_ref() != Some(&row.cost_center) {
            if let Some(cost_center) = current.take() {
                columns.push(CostCenterColumn {
                    cost_center,
                    currencies: currencies_of(std::mem::take(&mut lines), false),
                });
            }
            current = Some(row.cost_center.clone());
        }
        lines.push(row.line);
    }
    if let Some(cost_center) = current {
        columns.push(CostCenterColumn {
            cost_center,
            currencies: currencies_of(lines, false),
        });
    }
    Ok(Json(ProfitAndLossByCostCenterView {
        from,
        until,
        period: query.period,
        branch,
        cost_centers: columns,
    }))
}

/// The balance sheet as at an instant.
///
/// One sheet per currency. Give `as_at` (exclusive: postings dated before it
/// count), or a `period`, whose end is the instant; absent, now. The trading
/// result since the fiscal year started, and every earlier year's, appear as
/// two equity lines beside the chart's own retained earnings, because no
/// closing entry has moved them there yet. Company-wide only: a branch's books
/// do not balance on their own, since a transfer between branches debits one and
/// credits the other.
///
/// **Refused, not rendered, when the postings do not balance** — the ledger's
/// one invariant, and a sheet that did not balance would be a sheet somebody
/// acts on.
#[utoipa::path(
    get,
    path = "/v1/ledger/statements/balance-sheet",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("as_at" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Exclusive. Postings dated before this count. Absent means now."),
        ("period" = Option<String>, Query, description = "A period of the fiscal calendar; the sheet is as at its end."),
        ("all" = Option<bool>, Query, description = "Include accounts with no balance."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = BalanceSheetView),
        (status = BAD_REQUEST, description = "A period this calendar does not have — `ledger.no_such_period`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The postings in some currency do not balance as at this instant — `ledger.sheet_does_not_balance` — or the read model is behind", body = Problem),
    ),
)]
async fn balance_sheet(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<AsAtQuery>,
) -> Result<Json<BalanceSheetView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let fiscal = crate::fiscal::fiscal_calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let as_at = match query.period.as_deref() {
        Some(id) => {
            let period = fiscal
                .period(id)
                .ok_or_else(|| bad_request(messages::NO_SUCH_PERIOD, "period", id, locale))?;
            clock.start_of(period.until)
        }
        None => query.as_at.unwrap_or_else(chrono::Utc::now),
    };
    // The day before `as_at`, on the tenant's clock, is the last day the
    // sheet covers; its fiscal year is the one the current result counts from.
    let last_day = clock.day(as_at - chrono::Duration::seconds(1));
    let fiscal_year_started = fiscal.year_start(fiscal.fiscal_year_of(last_day));
    let parts = crate::balance_sheet(&mut conn, as_at, clock.start_of(fiscal_year_started))
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    drop(conn);

    let mut by_currency: std::collections::BTreeMap<String, BalanceSheetCurrency> =
        std::collections::BTreeMap::new();
    for line in parts.lines {
        if !query.all && line.postings == 0 && line.balance.minor() == 0 {
            continue;
        }
        let sheet = sheet_for(&mut by_currency, line.balance.currency().to_string());
        let kind = line.kind;
        let shown = natural(line);
        match kind {
            AccountKind::Asset => {
                sheet.total_assets += shown.amount;
                sheet.assets.push(shown);
            }
            AccountKind::Liability => {
                sheet.total_liabilities += shown.amount;
                sheet.liabilities.push(shown);
            }
            _ => {
                sheet.total_equity += shown.amount;
                sheet.equity.push(shown);
            }
        }
    }
    for result in parts.results {
        if result.difference.minor() != 0 {
            return Err(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                &Message::new(messages::SHEET_DOES_NOT_BALANCE)
                    .with("currency", MessageArg::text(result.currency.to_string()))
                    .with("as_at", MessageArg::text(as_at.to_rfc3339()))
                    .with(
                        "difference",
                        MessageArg::text(result.difference.to_string()),
                    ),
                locale,
                &CATALOG,
            ));
        }
        let sheet = sheet_for(&mut by_currency, result.currency.to_string());
        // Postings are signed debit-positive, so a profit — net credits on
        // the trading accounts — is negative; equity shows it positive.
        sheet.current_year_result = -result.current_year.minor();
        sheet.prior_years_result = -result.prior_years.minor();
        sheet.total_equity += sheet.current_year_result + sheet.prior_years_result;
    }
    Ok(Json(BalanceSheetView {
        as_at,
        fiscal_year_started,
        currencies: by_currency.into_values().collect(),
    }))
}

/// The sheet for one currency, made on first sight.
fn sheet_for(
    sheets: &mut std::collections::BTreeMap<String, BalanceSheetCurrency>,
    currency: String,
) -> &mut BalanceSheetCurrency {
    sheets
        .entry(currency.clone())
        .or_insert_with(|| BalanceSheetCurrency {
            currency,
            assets: Vec::new(),
            liabilities: Vec::new(),
            equity: Vec::new(),
            current_year_result: 0,
            prior_years_result: 0,
            total_assets: 0,
            total_liabilities: 0,
            total_equity: 0,
        })
}

/// One entry as the journal shows it.
fn journal_record(entry: crate::JournalEntryView) -> JournalEntryRecord {
    JournalEntryRecord {
        id: entry.id,
        occurred_on: entry.occurred_on,
        recorded_at: entry.recorded_at,
        branch: entry.branch,
        closing: entry.closing,
        currency: entry
            .lines
            .first()
            .map(|l| l.amount.currency().to_string())
            .unwrap_or_default(),
        lines: entry
            .lines
            .into_iter()
            .map(|l| JournalLineView {
                account: l.account,
                name: l.name,
                debit: l.amount.minor().max(0),
                credit: (-l.amount.minor()).max(0),
                memo: l.memo,
                cost_center: l.cost_center,
            })
            .collect(),
    }
}

/// The journal: every entry, newest first, with its lines.
///
/// Filter by a range (exclusive at the far end), an account the entry touches,
/// or a branch; a member confined to a branch reads theirs.
#[utoipa::path(
    get,
    path = "/v1/ledger/entries",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("from" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Inclusive."),
        ("until" = Option<chrono::DateTime<chrono::Utc>>, Query, description = "Exclusive."),
        ("account" = Option<String>, Query, description = "Entries with a line on this account."),
        ("branch" = Option<String>, Query, description = "One branch's entries. A confined member gets their branch."),
        ("cost_center" = Option<String>, Query, description = "Entries with a line in this cost center."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Entries per page. Clamped, never refused."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<JournalEntryRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_entries(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<JournalQuery>,
) -> Result<Json<Paged<JournalEntryRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let after = query.page.cursor(locale)?;
    let branch = tenant.branch_scope(query.branch.as_deref(), locale)?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let page = crate::journal(
        &mut conn,
        &crate::JournalFilter {
            from: query.from,
            until: query.until,
            account: query.account.as_deref(),
            branch: branch.as_deref(),
            cost_center: query.cost_center.as_deref(),
        },
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    Ok(Json(Paged::of(page, journal_record)))
}

/// One entry, with its lines.
#[utoipa::path(
    get,
    path = "/v1/ledger/entries/{entry}",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("entry" = String, Path, description = "The entry's id, as it was posted."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = JournalEntryRecord),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such entry — `ledger.no_such_entry`", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn journal_entry(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(entry): Path<String>,
) -> Result<Json<JournalEntryRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let found = crate::journal_entry(&mut conn, &entry)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?
        .ok_or_else(|| {
            ApiError::NotFound(
                Message::new(messages::NO_SUCH_ENTRY)
                    .with("entry", MessageArg::text(entry.clone())),
            )
            .into_problem(locale, &CATALOG)
        })?;
    Ok(Json(journal_record(found)))
}

// ---------------------------------------------------------------------------

/// Maps a command failure onto a status.
///
/// The rejections are the client's fault and say why; everything else routes
/// through [`ApiError`], which already knows that a conflict is a 409 and an
/// exhausted lane is a 503.
fn ledger_problem(error: &CommandError<crate::LedgerError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        // The client's fault, and the message says which part.
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Both mean "look at what is there now and decide again": a
                // code somebody else took, and an entry somebody else undid.
                crate::LedgerError::AccountExists(_)
                | crate::LedgerError::CostCenterExists(_)
                | crate::LedgerError::AlreadyReversed { .. } => StatusCode::CONFLICT,
                // Well-formed, but refers to something that is not there — or
                // to a period nobody may write into any more.
                crate::LedgerError::NoSuchAccount(_)
                | crate::LedgerError::AccountClosed(_)
                | crate::LedgerError::NoSuchCostCenter(_)
                | crate::LedgerError::CostCenterClosed(_)
                | crate::LedgerError::NoSuchEntry(_)
                | crate::LedgerError::PeriodClosed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
                _ => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        // Backpressure. Retryable, and saying so is the difference between a
        // client that backs off and one that hammers.
        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        // Sustained contention on one aggregate. Also retryable, but it is a
        // conflict rather than a capacity problem.
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
            tracing::error!(error = %other, "ledger command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

// ---------------------------------------------------------------------------
// Closing the books
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "closed_before": "2026-02-01T00:00:00Z", "booked": [2025] }))]
struct BooksView {
    /// **The first instant still open.** Everything strictly before it is final
    /// and no entry may be dated into it — not a journal entry, not an invoice's
    /// tax point, not a credit note's.
    ///
    /// Closing January is `2026-02-01T00:00:00Z`. Exclusive, like the VAT
    /// return's `until`, because "closed through 31 January" is a comparison
    /// somebody gets wrong by exactly one day.
    ///
    /// `null` on a tenant that has never closed a period. Moved by closing and
    /// reopening periods — `POST /v1/ledger/periods/{period}/close` — and by a
    /// VAT return being filed.
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    closed_before: Option<Timestamp>,
    /// The fiscal years whose closing entries stand.
    booked: Vec<i32>,
}

/// How far the books are closed.
#[utoipa::path(
    get,
    path = "/v1/ledger/books",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let books = crate::period::books(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    Ok(Json(BooksView {
        closed_before: books.closed_before,
        booked: books
            .years
            .iter()
            .filter(|(_, y)| y.booked)
            .map(|(year, _)| *year)
            .collect(),
    }))
}

/// Close a period.
///
/// After this, an entry dated into it is refused — including an invoice with
/// a back-dated tax point and a credit note dated into a quarter that has
/// already been declared. Corrections go into the period that is open, which
/// is where an auditor expects to find them.
///
/// **Periods close in order.** The one to close is the one the books are open
/// from; any period may be the first ever closed, and everything before it
/// closes with it. A period already closed is a no-op.
///
/// `ManageAccounts`, not `ManageTenant`: declaring the numbers final is the
/// accountant's call, and it is not something a clerk posting entries should be
/// able to do to them.
#[utoipa::path(
    post,
    path = "/v1/ledger/periods/{period}/close",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("period" = String, Path, description = "A period of the fiscal calendar, `2026-P03`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Closed. Entries dated into it are refused from now on."),
        (status = BAD_REQUEST, description = "A period this calendar does not have — `ledger.no_such_period`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "An earlier period is still open — `ledger.period_out_of_order`", body = Problem),
    ),
)]
async fn close_period(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Path(period): Path<String>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    crate::period::close_period(
        &tenant.db,
        &period,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| close_problem(e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reopen a period.
///
/// **An accountant who closes the wrong month has to be able to put it
/// right**, and a system that refuses is one they route around by editing the
/// database. Only the latest closed period reopens, and not while its year is
/// booked — reopen the year first. A period already open is a no-op.
#[utoipa::path(
    post,
    path = "/v1/ledger/periods/{period}/reopen",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("period" = String, Path, description = "A period of the fiscal calendar, `2026-P03`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Open again. Entries may be dated into it."),
        (status = BAD_REQUEST, description = "A period this calendar does not have — `ledger.no_such_period`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Not the latest closed period — `ledger.period_not_latest`; its year is booked — `ledger.year_booked`", body = Problem),
    ),
)]
async fn reopen_period(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Path(period): Path<String>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    crate::period::reopen_period(
        &tenant.db,
        &period,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| close_problem(e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

/// One fiscal year: whether it is closed, and whether it is booked.
#[derive(Debug, Serialize, ToSchema)]
struct YearView {
    /// Named by the calendar year it starts in.
    year: i32,
    #[schema(value_type = String)]
    from: chrono::NaiveDate,
    /// Exclusive — the first day of the next fiscal year.
    #[schema(value_type = String)]
    until: chrono::NaiveDate,
    /// Every period of it is closed.
    closed: bool,
    /// Its closing entries stand: the result is in retained earnings and the
    /// trading accounts start the next year at zero.
    booked: bool,
    /// The closing entries standing, one per currency.
    closing_entries: Vec<String>,
}

/// A fiscal year, and where it stands.
#[utoipa::path(
    get,
    path = "/v1/ledger/years/{year}",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("year" = i32, Path, description = "The fiscal year, by the calendar year it starts in."),
    ),
    responses(
        (status = OK, body = YearView),
        (status = BAD_REQUEST, description = "Not a year — `ledger.not_a_year`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn year(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Path(year): Path<String>,
) -> Result<Json<YearView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let year = year_of(&year, locale)?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let fiscal = crate::fiscal::fiscal_calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let books = crate::period::books(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);
    Ok(Json(year_view(year, &fiscal, clock, &books)))
}

/// A fiscal year from its path segment.
///
/// Parsed here rather than by `Path<i32>`, so a bad year is a problem+json like
/// every other refusal and not axum's plain-text rejection.
fn year_of(raw: &str, locale: Locale) -> Result<i32, Problem> {
    raw.parse()
        .map_err(|_| bad_request(messages::NOT_A_YEAR, "year", raw, locale))
}

fn year_view(
    year: i32,
    fiscal: &crate::FiscalCalendars,
    clock: erp_types::Calendar,
    books: &crate::Books,
) -> YearView {
    let from = fiscal.year_start(year);
    let until = fiscal.year_start(year + 1);
    let booked = books.years.get(&year);
    YearView {
        year,
        from,
        until,
        closed: books
            .closed_before
            .is_some_and(|closed| closed >= clock.start_of(until)),
        booked: booked.is_some_and(|b| b.booked),
        closing_entries: booked.map(|b| b.entries.clone()).unwrap_or_default(),
    }
}

/// Book a fiscal year — the year-end close.
///
/// Every period of the year must be closed. Then, per currency, every revenue
/// and expense balance of the year is posted away and the result posted to the
/// retained-earnings account for that currency (`GET /v1/ledger/closing-accounts`;
/// `3100` when it holds the currency), dated the year's last day and flagged
/// `closing`, so the profit and loss leaves it out while every balance keeps it.
/// A year already booked is a no-op; years need not be booked in order.
///
/// The figures come from the ledger's read model, which must have caught up
/// with the log — a 503 says try again in a moment.
#[utoipa::path(
    post,
    path = "/v1/ledger/years/{year}/close",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("year" = i32, Path, description = "The fiscal year, by the calendar year it starts in."),
    ),
    responses(
        (status = OK, description = "Booked. The closing entries are named.", body = YearView),
        (status = BAD_REQUEST, description = "Not a year — `ledger.not_a_year`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "A period is still open — `ledger.year_open`; a currency has no retained-earnings account — `ledger.closing_needs_account`", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The closing entry was refused — a closed account, say", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The read model is behind the log — `ledger.read_model_behind`", body = Problem),
    ),
)]
async fn close_year(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Path(year): Path<String>,
) -> Result<Json<YearView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let year = year_of(&year, locale)?;
    let memo = CATALOG.render_or_code(
        locale,
        &Message::new(messages::CLOSING_MEMO).with("year", MessageArg::Int(i64::from(year))),
    );
    let books = crate::period::close_year(
        &tenant.db,
        year,
        &memo,
        &metadata(&tenant),
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| close_problem(e, locale))?;
    year_after(&tenant, year, &books, locale).await
}

/// Reopen a fiscal year: reverse its closing entries.
///
/// Its periods stay closed — reopen those separately, once the year is. Refused
/// while a later year is booked, because that year's opening figures were built
/// on this one's close. A year not booked is a no-op.
#[utoipa::path(
    post,
    path = "/v1/ledger/years/{year}/reopen",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("year" = i32, Path, description = "The fiscal year, by the calendar year it starts in."),
    ),
    responses(
        (status = OK, description = "Reopened. The closing entries are reversed.", body = YearView),
        (status = BAD_REQUEST, description = "Not a year — `ledger.not_a_year`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "A later year is booked — `ledger.later_year_booked`", body = Problem),
    ),
)]
async fn reopen_year(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Path(year): Path<String>,
) -> Result<Json<YearView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let year = year_of(&year, locale)?;
    let memo = CATALOG.render_or_code(
        locale,
        &Message::new(messages::REOPENING_MEMO).with("year", MessageArg::Int(i64::from(year))),
    );
    let books = crate::period::reopen_year(
        &tenant.db,
        year,
        &memo,
        &metadata(&tenant),
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| close_problem(e, locale))?;
    year_after(&tenant, year, &books, locale).await
}

/// The year as it stands after a close or reopen.
async fn year_after(
    tenant: &Allowed<ManageAccounts>,
    year: i32,
    books: &crate::Books,
    locale: Locale,
) -> Result<Json<YearView>, Problem> {
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let fiscal = crate::fiscal::fiscal_calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let clock = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    Ok(Json(year_view(year, &fiscal, clock, books)))
}

/// Where a year's result goes when it is booked, by currency.
///
/// `{ "SAR": "3100" }`. Only what was configured: `3100` serves any currency it
/// holds without being named here. A currency with a trading balance and no
/// account here or at `3100` refuses the year close.
#[utoipa::path(
    get,
    path = "/v1/ledger/closing-accounts",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = std::collections::BTreeMap<String, String>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn closing_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<std::collections::BTreeMap<String, String>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let accounts = crate::ClosingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    Ok(Json(
        accounts
            .by_currency
            .iter()
            .map(|(currency, code)| (currency.to_string(), code.as_str().to_owned()))
            .collect(),
    ))
}

/// Set where each currency's result goes when a year is booked.
///
/// The whole map, replacing what was there. Checked for shape only — an
/// account that does not exist, is closed, or holds another currency is
/// refused by the year close that would use it, naming the account.
#[utoipa::path(
    put,
    path = "/v1/ledger/closing-accounts",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = std::collections::BTreeMap<String, String>,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "A currency or an account code that is not one", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_closing_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<std::collections::BTreeMap<String, String>>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut by_currency = std::collections::BTreeMap::new();
    for (currency, code) in &body {
        let currency = CurrencyCode::new(currency).map_err(|_| {
            bad_request(
                erp_web::messages::UNKNOWN_CURRENCY,
                "currency",
                currency,
                locale,
            )
        })?;
        by_currency.insert(currency, parse_id(code, locale)?);
    }
    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::ClosingAccounts::KEY,
        &crate::ClosingAccounts { by_currency },
        Some(&tenant.session.identity.to_string()),
        None,
    )
    .await
    .map_err(|e| config_problem(&e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Cost centers
// ---------------------------------------------------------------------------

/// The cost centers opened as such, with how much has landed in each.
///
/// Every open branch is a cost center too and is not listed here — read
/// `GET /v1/branches` for those. A line that names neither lands in its
/// entry's branch.
#[utoipa::path(
    get,
    path = "/v1/ledger/cost-centers",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<CostCenterView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_cost_centers(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<CostCenterView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let mut conn = tenant
        .db
        .read()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let centers = crate::cost_centers(&mut conn)
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    Ok(Json(
        centers
            .into_iter()
            .map(|c| CostCenterView {
                id: c.id,
                name: c.name,
                closed: c.closed,
                postings: c.postings,
            })
            .collect(),
    ))
}

/// Open a cost center.
///
/// A department, a project — whatever the business wants the result of. A
/// line of a journal entry or a purchase bill may then name it.
#[utoipa::path(
    post,
    path = "/v1/ledger/cost-centers",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = NewCostCenter,
    responses(
        (status = CREATED, description = "Opened."),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "That id is already open — `ledger.cost_center_exists`", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn open_cost_center(
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewCostCenter>,
) -> Result<impl IntoResponse, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = parse_id(&body.id, locale)?;
    crate::open_cost_center(&tenant.db, &id, &body.name, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;
    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::CREATED)
}

/// Rename a cost center. A no-op when the name already matches.
#[utoipa::path(
    put,
    path = "/v1/ledger/cost-centers/{cost_center}",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("cost_center" = String, Path, description = "Its id."),
    ),
    request_body = CostCenterName,
    responses(
        (status = NO_CONTENT, description = "Renamed."),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such cost center — `ledger.no_such_cost_center`", body = Problem),
    ),
)]
async fn rename_cost_center(
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(cost_center): Path<String>,
    Json(body): Json<CostCenterName>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = parse_id(&cost_center, locale)?;
    crate::rename_cost_center(&tenant.db, &id, &body.name, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;
    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Close a cost center. Its history stays; new lines are refused.
#[utoipa::path(
    post,
    path = "/v1/ledger/cost-centers/{cost_center}/close",
    tag = "ledger",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("cost_center" = String, Path, description = "Its id."),
    ),
    responses(
        (status = NO_CONTENT, description = "Closed, or already was."),
        (status = BAD_REQUEST, description = "An unusable id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such cost center — `ledger.no_such_cost_center`", body = Problem),
    ),
)]
async fn close_cost_center(
    tenant: Allowed<ManageAccounts>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(cost_center): Path<String>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = parse_id(&cost_center, locale)?;
    crate::close_cost_center(&tenant.db, &id, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;
    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// What a refused close or reopen is to the caller.
fn close_problem(error: crate::CloseError, locale: Locale) -> Problem {
    use crate::CloseError;
    let conflict =
        |message: Message| Problem::new(StatusCode::CONFLICT, &message, locale, &CATALOG);
    match error {
        CloseError::NoSuchPeriod(period) => {
            bad_request(messages::NO_SUCH_PERIOD, "period", &period, locale)
        }
        CloseError::OutOfOrder { period, next } => conflict(
            Message::new(messages::PERIOD_OUT_OF_ORDER)
                .with("period", MessageArg::text(period))
                .with("next", MessageArg::text(next)),
        ),
        CloseError::NotLatest { period, latest } => conflict(
            Message::new(messages::PERIOD_NOT_LATEST)
                .with("period", MessageArg::text(period))
                .with("latest", MessageArg::text(latest)),
        ),
        CloseError::YearBooked { year, period } => conflict(
            Message::new(messages::YEAR_BOOKED)
                .with("year", MessageArg::Int(i64::from(year)))
                .with("period", MessageArg::text(period)),
        ),
        CloseError::YearOpen { year, period } => conflict(
            Message::new(messages::YEAR_OPEN)
                .with("year", MessageArg::Int(i64::from(year)))
                .with("period", MessageArg::text(period)),
        ),
        CloseError::LaterYearBooked { year, later } => conflict(
            Message::new(messages::LATER_YEAR_BOOKED)
                .with("year", MessageArg::Int(i64::from(year)))
                .with("later", MessageArg::Int(i64::from(later))),
        ),
        CloseError::NeedsAccount { year, currency } => conflict(
            Message::new(messages::CLOSING_NEEDS_ACCOUNT)
                .with("year", MessageArg::Int(i64::from(year)))
                .with("currency", MessageArg::text(currency.to_string())),
        ),
        CloseError::ReadModelBehind { behind } => Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &Message::new(messages::LEDGER_BEHIND).with("behind", MessageArg::Int(behind)),
            locale,
            &CATALOG,
        ),
        CloseError::Ledger(e) => ledger_problem(&CommandError::Execute(e), locale),
        CloseError::Pool(e) => ledger_problem(&CommandError::Pool(e), locale),
        CloseError::Config(e) => config_problem(&e, locale),
        CloseError::Database(e) => {
            ledger_problem(&CommandError::Execute(ExecuteError::from(e)), locale)
        }
    }
}

fn config_problem(error: &erp_eventlog::ConfigError, locale: Locale) -> Problem {
    erp_web::config_problem(error, locale, &CATALOG)
}

// ---------------------------------------------------------------------------
// What the business charges
// ---------------------------------------------------------------------------

/// An empty string is a field a form submitted without filling in, and storing
/// it would make "no reason chosen" indistinguishable from "the reason is the
/// empty string" — which the issuing command would then accept.
fn none_if_blank(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "standard": 1500, "exempt_reason": "VATEX-SA-30" }))]
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

    /// **Why your zero-rated supplies carry no tax**, as the tax authority's
    /// own code. In Saudi Arabia one of ZATCA's `VATEX-SA-*` codes — an
    /// exporter of goods is `VATEX-SA-32`, a private school teaching citizens
    /// is `VATEX-SA-EDU`.
    ///
    /// Required before a zero-rated invoice can be issued, and there is no
    /// default: two businesses are zero-rated for entirely different articles
    /// and a code nobody chose is a false statement to a tax authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    zero_reason: Option<String>,

    /// **Why your exempt supplies carry no tax.** In Saudi Arabia a landlord
    /// letting residential property is `VATEX-SA-30` (real estate
    /// transactions); a financial services business is `VATEX-SA-29`.
    ///
    /// Required before an exempt invoice can be issued. See
    /// [`Self::zero_reason`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exempt_reason: Option<String>,
}

/// What this business charges VAT at.
#[utoipa::path(
    get,
    path = "/v1/ledger/vat-rates",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = RatesView, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn vat_rates(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<RatesView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    let version = erp_eventlog::configuration::version_of(&mut conn, crate::Rates::KEY)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    let rates = crate::Rates::resolve(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale))?;
    drop(conn);

    Ok(Versioned(
        version,
        RatesView {
            standard: rates.standard,
            zero_reason: rates.zero_reason.clone(),
            exempt_reason: rates.exempt_reason.clone(),
        },
    ))
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
    params(("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."), ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    request_body = RatesView,
    responses(
        (status = NO_CONTENT, description = "Set. Applies to the next invoice, not to past ones."),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = BAD_REQUEST, description = "A negative rate, or one over 100%", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_vat_rates(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<RatesView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    // A negative rate would credit VAT payable on every sale; one over 100%
    // would charge more tax than the supply. Neither is a rate anywhere.
    if !(0..=10_000).contains(&body.standard) {
        return Err(bad_request(
            erp_web::messages::UNUSABLE_VAT_RATE,
            "rate",
            &body.standard.to_string(),
            locale,
        ));
    }

    let mut conn = tenant
        .db
        .acquire()
        .await
        .map_err(|e| ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::Rates::KEY,
        &crate::Rates {
            standard: body.standard,
            zero_reason: none_if_blank(body.zero_reason.as_deref()),
            exempt_reason: none_if_blank(body.exempt_reason.as_deref()),
        },
        Some(&tenant.session.identity.to_string()),
        expected,
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
    responses(
        (status = OK, body = Vec<ChartView>),
        (status = TOO_MANY_REQUESTS, description = "Too many attempts from this address, or against this account. `args.seconds` says how long to wait.", body = Problem),
    ),
)]
async fn list_charts(_anonymous: Anonymous, Language(locale): Language) -> Json<Vec<ChartView>> {
    Json(
        crate::CHARTS
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
#[schema(example = json!({
    "opened": 26, "skipped": 2,
    "opened_codes": ["1000", "1010"], "skipped_codes": ["4000", "2100"]
}))]
struct ChartInstalled {
    opened: usize,
    /// Accounts that were already there. Installing twice is not an error.
    skipped: usize,
    /// **Which ones**, in the order the chart lists them.
    ///
    /// The counts alone are not something a person can check, and this shape is
    /// also what a *preview* answers — the same field names, because the
    /// preview is the same run against a transaction that is rolled back.
    opened_codes: Vec<String>,
    skipped_codes: Vec<String>,
}

impl From<crate::Installed> for ChartInstalled {
    fn from(installed: crate::Installed) -> Self {
        Self {
            opened: installed.opened(),
            skipped: installed.skipped(),
            opened_codes: installed.opened.iter().map(|c| (*c).to_owned()).collect(),
            skipped_codes: installed.skipped.iter().map(|c| (*c).to_owned()).collect(),
        }
    }
}

/// **What installing a chart would do**, without doing it.
///
/// Runs the real installation against a transaction and rolls it back, so this
/// answers what the install *did* rather than what a second implementation
/// predicts it would. The two cannot disagree, because they are the same code.
///
/// The body and the answer are the same shape as `POST /v1/ledger/chart`.
#[utoipa::path(
    post,
    path = "/v1/ledger/chart/preview",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = InstallChart,
    responses(
        (status = OK, description = "What would happen. Nothing was written.", body = ChartInstalled),
        (status = BAD_REQUEST, description = "No such chart, or a currency that is not one", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn preview_chart(
    tenant: Allowed<Read>,
    State(_state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<InstallChart>,
) -> Result<Json<ChartInstalled>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let chart = crate::chart(&body.template).ok_or_else(|| {
        bad_request(
            erp_web::messages::UNKNOWN_CHART,
            "chart",
            &body.template,
            locale,
        )
    })?;
    let currency = body.currency.parse().map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let would = crate::preview_chart(&tenant.db, chart, currency, locale, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;

    // **No `nudge`.** Nothing was written, so there is nothing for a worker to
    // come and project.
    Ok(Json(would.into()))
}

/// Open every account in a ready-made chart.
///
/// Installing twice is not an error: accounts that are already open are counted
/// as `skipped` and left exactly as they are, names included.
#[utoipa::path(
    post,
    path = "/v1/ledger/chart",
    tag = "ledger",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let chart = crate::chart(&body.template).ok_or_else(|| {
        bad_request(
            erp_web::messages::UNKNOWN_CHART,
            "chart",
            &body.template,
            locale,
        )
    })?;
    let currency = CurrencyCode::new(&body.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &body.currency,
            locale,
        )
    })?;

    let installed = crate::install_chart(&tenant.db, chart, currency, locale, &metadata(&tenant))
        .await
        .map_err(|e| ledger_problem(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;

    Ok(Json(installed.into()))
}
