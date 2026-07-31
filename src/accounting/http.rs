use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post},
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::{
    accounting::*,
    event_sourcing::{AggregateMeta, load_aggregate},
    platform::{ApiError, AppState, DomainError, dispatch},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/accounts",
            post(create_account_handler).get(get_accounts_handler),
        )
        .route("/accounts/tree", get(account_tree))
        .route(
            "/accounts/{id}",
            patch(rename_handler).get(get_account_handler),
        )
        .route("/accounts/{id}/deactivate", post(deactivate_handler))
        .route("/accounts/{id}/reactivate", post(reactivate_handler))
        .route("/accounts/{id}/balance", get(get_balance))
        .route("/accounts/{id}/statement", get(get_statement))
        .route("/accounts/{id}/deposit", post(deposit))
        .route("/journal-entries", post(create_journal_entry))
        .route("/journal-entries/{id}", get(get_journal_entry))
        .route("/journal-entries/{id}/reverse", post(reverse_entry))
        .route("/reports/trial-balance", get(trial_balance_report))
        .route("/reports/balance-sheet", get(balance_sheet_report))
        .route("/reports/income-statement", get(income_statement_report))
        .route("/fiscal-periods", post(open_period).get(list_periods))
        .route("/fiscal-periods/resolve", get(resolve_period))
        .route("/fiscal-periods/{id}", get(get_period))
        .route("/fiscal-periods/{id}/close", post(close_period))
        .route("/fiscal-periods/{id}/reopen", post(reopen_period))
        .route("/fiscal-periods/{id}/lock", post(lock_period))
}

impl DomainError for LedgerAccountError {
    fn status_code(&self) -> StatusCode {
        match self {
            LedgerAccountError::AlreadyExists(_) => StatusCode::CONFLICT,
            LedgerAccountError::NotYetCreated => StatusCode::NOT_FOUND,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

impl DomainError for JournalEntryError {
    fn status_code(&self) -> StatusCode {
        match self {
            JournalEntryError::Empty | JournalEntryError::Unbalanced { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            JournalEntryError::AlreadyDrafted
            | JournalEntryError::NotDraft
            | JournalEntryError::NotPosted => StatusCode::CONFLICT,
        }
    }
}

impl DomainError for FiscalPeriodError {
    fn status_code(&self) -> StatusCode {
        match self {
            FiscalPeriodError::AlreadyOpened
            | FiscalPeriodError::NotOpen
            | FiscalPeriodError::NotClosed => StatusCode::CONFLICT,
            FiscalPeriodError::Locked | FiscalPeriodError::InvalidBounds => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
        }
    }
}

#[derive(Deserialize)]
pub struct CreateLedgerAccountBody {
    code: String,
    name: String,
    account_type: AccountType,
    normal: BalanceSide,
    parent_code: Option<String>,
}

#[derive(Deserialize)]
pub struct RenameLedgerAccountBody {
    name: String,
}

#[derive(Serialize)]
pub struct AccountResponse {
    code: String,
    parent_code: Option<String>,
    name: String,
    account_type: AccountType,
    normal: BalanceSide,
}

impl From<LedgerAccount> for AccountResponse {
    fn from(a: LedgerAccount) -> Self {
        Self {
            code: a.id().to_string(),
            parent_code: a.parent_code(),
            name: a.name(),
            account_type: a.account_type(),
            normal: a.normal(),
        }
    }
}

async fn create_account_handler(
    State(state): State<AppState>,
    Json(body): Json<CreateLedgerAccountBody>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<LedgerAccount>(
        &state,
        &body.code.to_string(),
        LedgerAccountCommand::Create {
            code: body.code,
            name: body.name,
            account_type: body.account_type,
            normal: body.normal,
            parent_code: body.parent_code,
        },
    )
    .await?;
    Ok(Json(account.into()))
}

async fn rename_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<RenameLedgerAccountBody>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<LedgerAccount>(
        &state,
        &id,
        LedgerAccountCommand::Rename {
            new_name: body.name,
        },
    )
    .await?;
    Ok(Json(account.into()))
}

async fn deactivate_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<LedgerAccount>(&state, &id, LedgerAccountCommand::Deactivate).await?;
    Ok(Json(account.into()))
}

async fn reactivate_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = dispatch::<LedgerAccount>(&state, &id, LedgerAccountCommand::Reactivate).await?;
    Ok(Json(account.into()))
}

async fn get_account_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<AccountResponse>, ApiError> {
    let pool = state.read_pool.clone();
    let account = sqlx::query!(
        r#"SELECT account_code as code, parent_code, name, account_type as "account_type: AccountType", normal as "normal: BalanceSide", is_active FROM ledger_accounts WHERE account_code = $1"#,
        id,
    )
    .fetch_optional(&pool)
    .await
    .map_err(|e| ApiError::Internal(anyhow!(e)))?;
    match account {
        Some(account) => Ok(Json(AccountResponse {
            account_type: account.account_type,
            normal: account.normal,
            code: account.code,
            parent_code: account.parent_code,
            name: account.name,
        })),
        None => Err(ApiError::Domain(Box::new(
            LedgerAccountError::NotYetCreated,
        ))),
    }
}

async fn get_accounts_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<AccountResponse>>, ApiError> {
    let pool = state.read_pool.clone();
    let accounts = sqlx::query!(
        r#"SELECT account_code as code, parent_code, name, account_type as "account_type: AccountType", normal as "normal: BalanceSide", is_active FROM ledger_accounts ORDER BY account_code"#
    )
    .fetch_all(&pool)
    .await;
    match accounts {
        Ok(accounts) => {
            let result = accounts
                .iter()
                .map(|account| AccountResponse {
                    account_type: account.account_type,
                    normal: account.normal,
                    code: account.code.clone(),
                    parent_code: account.parent_code.clone(),
                    name: account.name.clone(),
                })
                .collect::<Vec<AccountResponse>>();
            Ok(Json(result))
        }
        Err(error) => Err(ApiError::Internal(anyhow!(error))),
    }
}

#[derive(Serialize)]
pub struct AccountTreeNode {
    pub account_code: String,
    pub name: String,
    pub account_type: AccountType,
    pub is_active: bool,
    pub children: Vec<AccountTreeNode>,
}

struct TreeRow {
    account_code: String,
    name: String,
    account_type: AccountType,
    parent_code: Option<String>,
    is_active: bool,
}

/// Chart of accounts as a tree, built from parent_code links.
///
/// Data-safety notes (parent links are NOT validated at account
/// creation - cross-aggregate checks were deliberately kept out of
/// LedgerAccount::handle):
/// - orphans (parent_code pointing at a nonexistent account) are
///   promoted to roots rather than dropped - a chart endpoint must
///   never silently hide accounts;
/// - a parent cycle (A->B->A) would recurse forever, so nodes already
///   placed are never revisited; any cycle members left unplaced are
///   likewise promoted to roots (flattened) instead of vanishing.
async fn account_tree(
    State(state): State<AppState>,
) -> Result<Json<Vec<AccountTreeNode>>, ApiError> {
    let rows = sqlx::query_as!(
        TreeRow,
        r#"SELECT account_code, name, account_type as "account_type: AccountType", parent_code, is_active
           FROM ledger_accounts
           ORDER BY account_code"#
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    use std::collections::{BTreeMap, HashSet};
    let known: HashSet<String> = rows.iter().map(|r| r.account_code.clone()).collect();

    let mut by_parent: BTreeMap<Option<String>, Vec<TreeRow>> = BTreeMap::new();
    for row in rows {
        let key = match &row.parent_code {
            Some(p) if known.contains(p) => Some(p.clone()),
            _ => None, // orphan promotion: missing parent -> root
        };
        by_parent.entry(key).or_default().push(row);
    }

    fn build(
        parent: Option<String>,
        by_parent: &mut BTreeMap<Option<String>, Vec<TreeRow>>,
        placed: &mut HashSet<String>,
    ) -> Vec<AccountTreeNode> {
        let Some(rows) = by_parent.remove(&parent) else {
            return vec![];
        };
        // Plain loop rather than .filter().map(): both adapters would
        // capture &mut placed in separate closures that coexist, which
        // the borrow checker rejects (E0524). In a loop the insert
        // borrow ends before the recursive borrow begins.
        let mut nodes = Vec::with_capacity(rows.len());
        for r in rows {
            if !placed.insert(r.account_code.clone()) {
                continue; // cycle guard: never place a node twice
            }
            let children = build(Some(r.account_code.clone()), by_parent, placed);
            nodes.push(AccountTreeNode {
                account_code: r.account_code,
                name: r.name,
                account_type: r.account_type,
                is_active: r.is_active,
                children,
            });
        }
        nodes
    }

    let mut placed = HashSet::new();
    let mut roots = build(None, &mut by_parent, &mut placed);

    for (_, leftovers) in std::mem::take(&mut by_parent) {
        for r in leftovers {
            if placed.insert(r.account_code.clone()) {
                roots.push(AccountTreeNode {
                    account_code: r.account_code,
                    name: r.name,
                    account_type: r.account_type,
                    is_active: r.is_active,
                    children: vec![],
                });
            }
        }
    }

    Ok(Json(roots))
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub account_code: String,
    pub account_type: AccountType,
    pub normal: BalanceSide, // "Debit" | "Credit"
    /// Per currency - one account can hold postings in several.
    pub balances: Vec<CurrencyBalance>,
}

#[derive(Serialize)]
pub struct CurrencyBalance {
    pub currency: String,
    pub debit_total: i64,
    pub credit_total: i64,
    /// debit - credit: positive = net debit position. Sign convention
    /// independent of account type - what an accountant reads raw.
    pub net_debit: i64,
    /// Interpreted through the account's normal side: positive = the
    /// account holds a balance on its normal side (a "healthy" balance).
    /// This is the number a UI shows as "the balance".
    pub balance: i64,
}

async fn get_balance(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<BalanceResponse>, ApiError> {
    let account = sqlx::query!(
        r#"SELECT account_type as "account_type: AccountType", normal as "normal: BalanceSide" FROM ledger_accounts WHERE account_code = $1"#,
        id
    )
    .fetch_optional(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
    .ok_or(ApiError::NotFound(None))?;

    let normal = account.normal;

    let rows = sqlx::query!(
        r#"SELECT currency, debit_total, credit_total
           FROM trial_balance
           WHERE account_code = $1
           ORDER BY currency"#,
        id
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let balances = rows
        .into_iter()
        .map(|r| {
            let net_debit = r.debit_total - r.credit_total;
            CurrencyBalance {
                currency: r.currency,
                debit_total: r.debit_total,
                credit_total: r.credit_total,
                net_debit: net_debit,
                balance: match normal {
                    BalanceSide::Debit => net_debit,
                    BalanceSide::Credit => -net_debit,
                },
            }
        })
        .collect();

    Ok(Json(BalanceResponse {
        account_code: id,
        account_type: account.account_type,
        normal,
        balances,
    }))
}

#[derive(Deserialize)]
pub struct StatementQuery {
    pub from: NaiveDate,
    pub to: NaiveDate, // inclusive
    pub currency: String,
}

#[derive(Serialize)]
pub struct StatementResponse {
    pub account_code: String,
    pub currency: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Signed balance (debit-positive) as of the instant before `from`.
    pub opening_balance_minor: i64,
    pub lines: Vec<StatementLineRow>,
    pub closing_balance_minor: i64,
}

#[derive(Serialize)]
pub struct StatementLineRow {
    pub date: NaiveDate,
    pub journal_entry_id: String,
    pub side: BalanceSide,
    pub amount: i64,
    /// Running balance AFTER this line, computed over the account's full
    /// history (not just the window), so it agrees with opening/closing.
    pub running_balance: i64,
}

async fn get_statement(
    Path(id): Path<String>,
    Query(q): Query<StatementQuery>,
    State(state): State<AppState>,
) -> Result<Json<StatementResponse>, ApiError> {
    if q.from > q.to {
        return Err(ApiError::BadRequest(Some("`from` is after `to`".into())));
    }

    // Opening balance: everything strictly before the window.
    let opening = sqlx::query!(
        r#"SELECT COALESCE(SUM(signed), 0)::bigint AS "opening!"
           FROM general_ledger
           WHERE account_code = $1 AND currency = $2 AND entry_date < $3"#,
        id,
        q.currency,
        q.from,
    )
    .fetch_one(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    // Lines in-window, running balance over FULL history via the view.
    // Predicate pushdown on the partition columns (account_code,
    // currency) is safe; the date filter applies after the window
    // computes - which is exactly the semantics a statement needs.
    let lines = sqlx::query!(
        r#"SELECT entry_date as "entry_date!",
                  journal_entry_id as "journal_entry_id!",
                  side as "side!: BalanceSide",
                  amount as "amount!",
                  running_balance AS "running_balance!"
           FROM general_ledger_with_balance
           WHERE account_code = $1 AND currency = $2
             AND entry_date >= $3 AND entry_date <= $4
           ORDER BY global_position"#,
        id,
        q.currency,
        q.from,
        q.to,
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let closing = lines
        .last()
        .map(|l| l.running_balance)
        .unwrap_or(opening.opening);

    Ok(Json(StatementResponse {
        account_code: id,
        currency: q.currency,
        from: q.from,
        to: q.to,
        opening_balance_minor: opening.opening,
        lines: lines
            .into_iter()
            .map(|l| StatementLineRow {
                date: l.entry_date,
                journal_entry_id: l.journal_entry_id,
                side: l.side,
                amount: l.amount,
                running_balance: l.running_balance,
            })
            .collect(),
        closing_balance_minor: closing,
    }))
}

#[derive(Deserialize)]
pub struct DepositBody {
    /// Where the money comes FROM - e.g. an equity account for a capital
    /// contribution, another cash/bank account for an internal transfer.
    pub source_account_code: String,
    pub amount: i64,
    pub currency: String,
    pub date: NaiveDate,
    pub memo: String,
}

#[derive(Deserialize)]
pub struct JournalLineBody {
    pub account_code: String,
    pub side: Side,
    pub amount: i64,
    pub currency: String,
}

#[derive(Deserialize)]
pub struct ManualEntryBody {
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<JournalEntryReference>,
    pub lines: Vec<JournalLineBody>,
}

async fn deposit(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(body): Json<DepositBody>,
) -> Result<(StatusCode, Json<JournalEntryCreatedResponse>), ApiError> {
    if body.amount <= 0 {
        return Err(ApiError::BadRequest(Some("amount must be positive".into())));
    }
    let entry_id = uuid::Uuid::new_v4().to_string();
    post_journal_entry(
        state.event_store.as_ref(),
        state.event_bus,
        &entry_id,
        body.date,
        body.memo,
        None,
        vec![
            JournalLine {
                account_code: code,
                side: Side::Debit,
                amount: body.amount,
                currency: body.currency.clone(),
            },
            JournalLine {
                account_code: body.source_account_code,
                side: Side::Credit,
                amount: body.amount,
                currency: body.currency,
            },
        ],
    )
    .await
    .map_err(map_journal_error)?;
    Ok((
        StatusCode::CREATED,
        Json(JournalEntryCreatedResponse {
            journal_entry_id: entry_id,
        }),
    ))
}

async fn create_journal_entry(
    State(state): State<AppState>,
    Json(body): Json<ManualEntryBody>,
) -> Result<(StatusCode, Json<JournalEntryCreatedResponse>), ApiError> {
    let entry_id = uuid::Uuid::new_v4().to_string();
    post_journal_entry(
        state.event_store.as_ref(),
        state.event_bus,
        &entry_id,
        body.date,
        body.memo,
        body.reference,
        body.lines
            .into_iter()
            .map(|l| JournalLine {
                account_code: l.account_code,
                side: l.side,
                amount: l.amount,
                currency: l.currency,
            })
            .collect(),
    )
    .await
    .map_err(map_journal_error)?;
    Ok((
        StatusCode::CREATED,
        Json(JournalEntryCreatedResponse {
            journal_entry_id: entry_id,
        }),
    ))
}

fn map_journal_error(e: anyhow::Error) -> ApiError {
    // post_journal_entry surfaces both aggregate errors (unbalanced) and
    // cross-aggregate check failures (inactive account, closed period) -
    // the latter arrive as plain anyhow messages and map to 422.
    match e.downcast::<JournalEntryError>() {
        Ok(domain) => ApiError::Domain(Box::new(domain)),
        Err(other) => ApiError::BadRequest(Some(other.to_string())),
    }
}

#[derive(Serialize)]
pub struct JournalEntryCreatedResponse {
    pub journal_entry_id: String,
}

#[derive(Serialize)]
pub struct JournalEntryResponse {
    pub id: String,
    pub status: Option<JournalEntryStatus>,
    pub date: NaiveDate,
    pub lines: Vec<JournalLineResponse>,
}

#[derive(Serialize)]
pub struct JournalLineResponse {
    pub account_code: String,
    pub side: String,
    pub amount: i64,
    pub currency: String,
}

async fn get_journal_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JournalEntryResponse>, ApiError> {
    let entry = load_aggregate::<JournalEntry>(state.event_store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    if entry.status().is_none() {
        return Err(ApiError::NotFound(None));
    }
    Ok(Json(JournalEntryResponse {
        id,
        status: entry.status(),
        date: entry.date().expect("already set"),
        lines: entry
            .lines()
            .iter()
            .map(|l| JournalLineResponse {
                account_code: l.account_code.clone(),
                side: format!("{:?}", l.side),
                amount: l.amount,
                currency: l.currency.clone(),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct ReverseBody {
    pub date: NaiveDate,
    pub reason: String,
}

async fn reverse_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReverseBody>,
) -> Result<(StatusCode, Json<JournalEntryCreatedResponse>), ApiError> {
    let reversal_id = uuid::Uuid::new_v4().to_string();
    reverse_journal_entry(
        state.event_store.as_ref(),
        state.event_bus,
        &id,
        &reversal_id,
        body.date,
        body.reason,
    )
    .await
    .map_err(map_journal_error)?;
    Ok((
        StatusCode::CREATED,
        Json(JournalEntryCreatedResponse {
            journal_entry_id: reversal_id,
        }),
    ))
}

#[derive(Serialize)]
pub struct TrialBalanceRow {
    pub account_code: String,
    pub name: String,
    pub account_type: AccountType,
    pub currency: String,
    pub debit_total: i64,
    pub credit_total: i64,
}

#[derive(Serialize)]
pub struct TrialBalanceResponse {
    pub rows: Vec<TrialBalanceRow>,
    /// Per currency: these MUST match. A mismatch means a projector bug
    /// or corruption - surfaced rather than hidden.
    pub totals: Vec<TrialBalanceTotals>,
}

#[derive(Serialize)]
pub struct TrialBalanceTotals {
    pub currency: String,
    pub debit_total: i64,
    pub credit_total: i64,
    pub balanced: bool,
}

async fn trial_balance_report(
    State(state): State<AppState>,
) -> Result<Json<TrialBalanceResponse>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT tb.account_code, l.name AS "name!", l.account_type AS "account_type!: AccountType",
                  tb.currency, tb.debit_total, tb.credit_total
           FROM trial_balance tb
           JOIN ledger_accounts l ON l.account_code = tb.account_code
           ORDER BY tb.account_code, tb.currency"#
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    let mut totals: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    for r in &rows {
        let t = totals.entry(r.currency.clone()).or_default();
        t.0 += r.debit_total;
        t.1 += r.credit_total;
    }

    Ok(Json(TrialBalanceResponse {
        rows: rows
            .into_iter()
            .map(|r| TrialBalanceRow {
                account_code: r.account_code,
                name: r.name,
                account_type: r.account_type,
                currency: r.currency,
                debit_total: r.debit_total,
                credit_total: r.credit_total,
            })
            .collect(),
        totals: totals
            .into_iter()
            .map(|(currency, (d, c))| TrialBalanceTotals {
                currency,
                debit_total: d,
                credit_total: c,
                balanced: d == c,
            })
            .collect(),
    }))
}

#[derive(Serialize)]
pub struct StatementReportRow {
    pub account_code: String,
    pub name: String,
    pub account_type: AccountType,
    pub currency: String,
    /// Sign-adjusted to the account's normal side (positive = normal).
    pub balance: i64,
}

async fn balance_sheet_report(
    State(state): State<AppState>,
) -> Result<Json<Vec<StatementReportRow>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT tb.account_code, l.name AS "name!", l.account_type AS "account_type!: AccountType", l.normal as "normal!: BalanceSide",
                  tb.currency, (tb.debit_total - tb.credit_total)::bigint AS "net_debit!"
           FROM trial_balance tb
           JOIN ledger_accounts l ON l.account_code = tb.account_code
           WHERE l.account_type IN ('asset', 'liability', 'equity')
           ORDER BY l.account_type, tb.account_code, tb.currency"#
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok(Json(sign_adjust(rows.into_iter().map(|r| {
        (
            r.account_code,
            r.name,
            r.account_type,
            r.normal,
            r.currency,
            r.net_debit,
        )
    }))?))
}

#[derive(Deserialize)]
pub struct PeriodQuery {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate, // inclusive
}

/// Income statement: Revenue/Expense activity WITHIN a period - summed
/// from the dated ledger lines, not the all-time trial balance.
async fn income_statement_report(
    State(state): State<AppState>,
    Query(q): Query<PeriodQuery>,
) -> Result<Json<Vec<StatementReportRow>>, ApiError> {
    if q.from > q.to {
        return Err(ApiError::BadRequest(Some("`from` is after `to`".into())));
    }
    let rows = sqlx::query!(
        r#"SELECT gl.account_code, l.name AS "name!", l.account_type AS "account_type!: AccountType", l.normal as "normal!: BalanceSide",
                  gl.currency, COALESCE(SUM(gl.signed), 0)::bigint AS "net_debit!"
           FROM general_ledger gl
           JOIN ledger_accounts l ON l.account_code = gl.account_code
           WHERE l.account_type IN ('revenue', 'expense')
             AND gl.entry_date >= $1 AND gl.entry_date <= $2
           GROUP BY gl.account_code, l.name, l.account_type, l.normal, gl.currency
           ORDER BY l.account_type, l.normal, gl.account_code, gl.currency"#,
           q.from,
           q.to,
    )
    .fetch_all(&state.read_pool)
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;

    Ok(Json(sign_adjust(rows.into_iter().map(|r| {
        (
            r.account_code,
            r.name,
            r.account_type,
            r.normal,
            r.currency,
            r.net_debit,
        )
    }))?))
}

fn sign_adjust(
    rows: impl Iterator<Item = (String, String, AccountType, BalanceSide, String, i64)>,
) -> Result<Vec<StatementReportRow>, ApiError> {
    rows.map(
        |(account_code, name, account_type, normal, currency, net_debit)| {
            let balance = match normal {
                BalanceSide::Debit => net_debit,
                BalanceSide::Credit => -net_debit,
            };
            Ok(StatementReportRow {
                account_code,
                name,
                account_type,
                currency,
                balance,
            })
        },
    )
    .collect()
}

#[derive(Deserialize)]
pub struct OpenPeriodBody {
    pub kind: PeriodKind,
    pub date: Option<NaiveDate>,
    pub custom: Option<CustomPeriodBody>,
}

#[derive(Deserialize)]
pub struct CustomPeriodBody {
    pub id: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

#[derive(Serialize)]
pub struct OpenedPeriodResponse {
    pub period_id: String,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
}

#[derive(Serialize)]
pub struct FiscalPeriodRow {
    pub period_id: String,
    pub kind: PeriodKind,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    /// "Open" | "Closed" | "Locked" - or null in the pathological case
    /// of a registered-but-never-opened period, which the atomic
    /// open_fiscal_period orchestration should make impossible; shown
    /// as null rather than hidden if it ever occurs (corruption signal).
    pub status: Option<FiscalPeriodStatus>,
}

/// All periods, calendar order. Reads the write model directly: the
/// calendar is one aggregate and period counts are tiny, so N+1 loads
/// here are a handful of indexed queries - and the answer is strongly
/// consistent, which matters on the screen an admin checks right after
/// opening/closing a period.
async fn list_periods(
    State(state): State<AppState>,
) -> Result<Json<Vec<FiscalPeriodRow>>, ApiError> {
    let calendar = load_aggregate::<FiscalCalendar>(state.event_store.as_ref(), FISCAL_CALENDAR_ID)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;

    let mut rows = Vec::with_capacity(calendar.periods().len());
    for p in calendar.periods() {
        let period = load_aggregate::<FiscalPeriod>(state.event_store.as_ref(), &p.period_id)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?;
        rows.push(FiscalPeriodRow {
            period_id: p.period_id.clone(),
            kind: p.kind,
            start_date: p.start_date,
            end_date: p.end_date,
            status: period.status(),
        });
    }
    rows.sort_by(|a, b| a.start_date.cmp(&b.start_date));
    Ok(Json(rows))
}

async fn get_period(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<FiscalPeriodRow>, ApiError> {
    let calendar = load_aggregate::<FiscalCalendar>(state.event_store.as_ref(), FISCAL_CALENDAR_ID)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let Some(registered) = calendar.periods().iter().find(|p| p.period_id == id) else {
        return Err(ApiError::NotFound(None));
    };
    let period = load_aggregate::<FiscalPeriod>(state.event_store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(FiscalPeriodRow {
        period_id: registered.period_id.clone(),
        kind: registered.kind,
        start_date: registered.start_date,
        end_date: registered.end_date,
        status: period.status(),
    }))
}

#[derive(Deserialize)]
pub struct ResolveQuery {
    pub date: chrono::NaiveDate,
}

/// Which period covers a date - the same routing posting uses, exposed
/// so UIs can answer "can I post to this date?" BEFORE submitting (e.g.
/// disabling locked dates in a date picker). 404 = no period covers it.
async fn resolve_period(
    State(state): State<AppState>,
    Query(q): Query<ResolveQuery>,
) -> Result<Json<FiscalPeriodRow>, ApiError> {
    let calendar = load_aggregate::<FiscalCalendar>(state.event_store.as_ref(), FISCAL_CALENDAR_ID)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let Some(registered) = calendar.period_covering(q.date) else {
        return Err(ApiError::NotFound(None));
    };
    let period = load_aggregate::<FiscalPeriod>(state.event_store.as_ref(), &registered.period_id)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(Json(FiscalPeriodRow {
        period_id: registered.period_id.clone(),
        kind: registered.kind,
        start_date: registered.start_date,
        end_date: registered.end_date,
        status: period.status(),
    }))
}

async fn open_period(
    State(state): State<AppState>,
    Json(body): Json<OpenPeriodBody>,
) -> Result<(StatusCode, Json<OpenedPeriodResponse>), ApiError> {
    let (id, start, end) = match (body.custom, body.date) {
        (Some(c), _) => (c.id, c.start_date, c.end_date),
        (None, Some(date)) => body.kind.canonical_for(date),
        (None, None) => {
            return Err(ApiError::BadRequest(Some(
                "provide either `date` (canonical period) or `custom` bounds".into(),
            )));
        }
    };

    open_fiscal_period(
        state.event_store.as_ref(),
        state.event_bus,
        &id,
        body.kind,
        start,
        end,
    )
    .await
    .map_err(|e| match e.downcast::<FiscalCalendarError>() {
        Ok(cal) => ApiError::BadRequest(Some(cal.to_string())),
        Err(other) => match other.downcast::<FiscalPeriodError>() {
            Ok(p) => ApiError::Domain(Box::new(p)),
            Err(rest) => ApiError::Internal(rest),
        },
    })?;

    Ok((
        StatusCode::CREATED,
        Json(OpenedPeriodResponse {
            period_id: id,
            start_date: start,
            end_date: end,
        }),
    ))
}

async fn close_period(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    dispatch::<FiscalPeriod>(&state, &id, FiscalPeriodCommand::Close).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn reopen_period(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    dispatch::<FiscalPeriod>(&state, &id, FiscalPeriodCommand::Reopen).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn lock_period(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    dispatch::<FiscalPeriod>(&state, &id, FiscalPeriodCommand::Lock).await?;
    Ok(StatusCode::NO_CONTENT)
}
