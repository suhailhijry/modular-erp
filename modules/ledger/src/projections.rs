//! The ledger's read models, and the invariant that checks them.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::account::{AccountEvent, AccountKind};
use crate::entry::JournalEntryEvent;

/// Accounts and postings, in one group.
///
/// One group because they must agree: a posting referencing an account that has
/// not appeared yet is a state nobody should be able to query. Separate groups
/// would replay at different rates and produce exactly that (architecture L3).
#[derive(Debug)]
pub struct Ledger;

impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

fn decode<E: serde::de::DeserializeOwned>(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
) -> Result<E, ProjectionError> {
    ctx.decode(envelope)
        .map_err(|source| ProjectionError::Decode {
            event_name: envelope.event_name.as_str().to_owned(),
            position: envelope.position,
            source,
        })
}

/// The chart of accounts.
#[derive(Debug)]
pub struct Accounts;

#[async_trait::async_trait]
impl Projection for Accounts {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "accounts"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !AccountEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        // The account code is the aggregate id.
        let code = envelope.stream.id.as_str();

        match decode::<AccountEvent>(ctx, envelope)? {
            AccountEvent::Opened {
                name,
                kind,
                currency,
            } => {
                sqlx::query(
                    "INSERT INTO account (code, name, kind, currency, closed, opened_at)
                     VALUES ($1, $2, $3, $4, false, $5)",
                )
                .bind(code)
                .bind(&name)
                .bind(kind.as_str())
                .bind(currency.as_str())
                // The event's time, never the wall clock (L2).
                .bind(ctx.event_time())
                .execute(&mut *conn)
                .await?;
            }
            AccountEvent::Renamed { name } => {
                sqlx::query("UPDATE account SET name = $2 WHERE code = $1")
                    .bind(code)
                    .bind(&name)
                    .execute(&mut *conn)
                    .await?;
            }
            event @ (AccountEvent::Closed | AccountEvent::Reopened) => {
                sqlx::query("UPDATE account SET closed = $2 WHERE code = $1")
                    .bind(code)
                    .bind(matches!(event, AccountEvent::Closed))
                    .execute(&mut *conn)
                    .await?;
            }
        }
        Ok(())
    }
}

/// Every line of every entry.
#[derive(Debug)]
pub struct Postings;

#[async_trait::async_trait]
impl Projection for Postings {
    type Group = Ledger;

    fn name(&self) -> &'static str {
        "postings"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if envelope.event_name.as_str() != JournalEntryEvent::NAMES[0] {
            return Ok(());
        }
        let JournalEntryEvent::Posted {
            occurred_on,
            lines,
            memo,
        } = decode::<JournalEntryEvent>(ctx, envelope)?
        else {
            return Ok(());
        };

        let entry_id = envelope.stream.id.as_str();

        for (index, line) in lines.as_slice().iter().enumerate() {
            let index = i32::try_from(index).unwrap_or(i32::MAX);
            sqlx::query(
                "INSERT INTO posting
                     (id, entry_id, line_index, account, amount, currency,
                      memo, branch, occurred_on, recorded_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            // Derived from the position, so a rebuild produces the same key.
            // `Uuid::new_v4()` here would make every replayed row differ.
            .bind(ctx.derive_id(&format!("line-{index}")))
            .bind(entry_id)
            .bind(index)
            .bind(line.account.as_str())
            .bind(line.amount.minor())
            .bind(line.amount.currency().as_str())
            .bind(line.memo.as_deref().or(Some(memo.as_str())))
            // **Read from the metadata**, which is where a request records
            // where it happened — see `Metadata::at_branch`. Every posting in
            // the system therefore carries it, without any module that posts
            // having to thread a field through.
            .bind(envelope.metadata.branch())
            .bind(occurred_on)
            .bind(ctx.event_time())
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Ledger>>> {
    vec![std::sync::Arc::new(Accounts), std::sync::Arc::new(Postings)]
}

// ---------------------------------------------------------------------------
// The invariant
// ---------------------------------------------------------------------------

/// One currency's side of the trial balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialBalance {
    pub currency: CurrencyCode,
    /// Debits minus credits. **Must be zero.**
    pub difference: Money,
    pub debits: Money,
    pub credits: Money,
    pub postings: i64,
}

impl TrialBalance {
    #[must_use]
    pub const fn balances(&self) -> bool {
        self.difference.is_zero()
    }
}

/// Reads the trial balance, per currency.
///
/// # What a non-zero row means
///
/// Not "someone posted badly" — [`BalancedLines`](crate::BalancedLines) makes
/// that unconstructable. It means the pipeline is broken: a projection applied
/// an event twice, or a rebuild diverged, or rows were written by something that
/// is not this code. It is the canary for an entire class of bug, which is why
/// it is worth checking continuously rather than at month end.
pub async fn trial_balance(conn: &mut PgConnection) -> Result<Vec<TrialBalance>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT currency as "currency!", difference as "difference!",
                  debits as "debits!", credits as "credits!",
                  postings as "postings!"
             FROM proj_ledger.trial_balance
            ORDER BY currency"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(TrialBalance {
                currency,
                difference: Money::from_minor(row.difference, currency),
                debits: Money::from_minor(row.debits, currency),
                credits: Money::from_minor(row.credits, currency),
                postings: row.postings,
            })
        })
        .collect()
}

/// What one branch did, per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchBalance {
    /// `None` on postings written without a branch — which is every posting a
    /// single-branch business makes, and every one written before branches
    /// existed.
    pub branch: Option<String>,
    pub code: String,
    pub name: String,
    pub balance: Money,
    pub postings: i64,
}

/// The chart of accounts, split by branch.
///
/// # What this answers, and what it does not
///
/// It answers *"what did Olaya do"*, and the branches sum to the whole — which
/// is the exit criterion for branches and the useful half of a per-branch
/// report.
///
/// It does **not** claim each branch is a balanced set of books. Debits equal
/// credits per *currency*, which is [`trial_balance`]; a transfer of cash from
/// one branch to another debits one and credits the other, so each side is out
/// by the transfer until inter-branch clearing accounts exist. Reporting a
/// per-branch difference as an error would be reporting a normal transfer as a
/// broken ledger.
pub async fn branch_balances(
    conn: &mut PgConnection,
    branch: Option<&str>,
) -> Result<Vec<BranchBalance>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT branch, account as "account!", name, currency as "currency!",
                  balance as "balance!", postings as "postings!"
             FROM proj_ledger.branch_balance
            WHERE $1::text IS NULL OR branch = $1
            ORDER BY branch NULLS FIRST, account"#,
        branch
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(BranchBalance {
                branch: row.branch,
                code: row.account,
                name: row.name.unwrap_or_default(),
                balance: Money::from_minor(row.balance, currency),
                postings: row.postings,
            })
        })
        .collect()
}

/// The health check this module contributes.
///
/// Returns the currencies that do not balance. Empty is healthy.
pub async fn imbalances(conn: &mut PgConnection) -> Result<Vec<TrialBalance>, sqlx::Error> {
    Ok(trial_balance(conn)
        .await?
        .into_iter()
        .filter(|t| !t.balances())
        .collect())
}

/// An account and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountBalance {
    pub code: String,
    pub name: String,
    pub kind: AccountKind,
    pub balance: Money,
    pub closed: bool,
    pub postings: i64,
}

/// The chart of accounts with balances.
pub async fn account_balances(conn: &mut PgConnection) -> Result<Vec<AccountBalance>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT code as "code!", name as "name!", kind as "kind!",
                  currency as "currency!", closed as "closed!",
                  balance as "balance!", postings as "postings!"
             FROM proj_ledger.account_balance
            ORDER BY code"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(AccountBalance {
                code: row.code,
                name: row.name,
                kind: row
                    .kind
                    .parse()
                    .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
                balance: Money::from_minor(row.balance, currency),
                closed: row.closed,
                postings: row.postings,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Statements
//
// Every figure below is a sum over `posting` at the moment it is asked, never
// a maintained total — the same argument the balance views make. A statement
// is a question about a range of instants, so each query takes the range and
// does the arithmetic in SQL, where a sum is exact.
// ---------------------------------------------------------------------------

/// The chart with balances **as at** an instant: every posting dated before
/// `until` counts, and nothing on or after it. `None` is all-time, which is
/// [`account_balances`].
///
/// # Errors
/// If the database does.
pub async fn balances_at(
    conn: &mut PgConnection,
    until: Option<Timestamp>,
) -> Result<Vec<AccountBalance>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT a.code as "code!", a.name as "name!", a.kind as "kind!",
                  a.currency as "currency!", a.closed as "closed!",
                  COALESCE(sum(p.amount) FILTER (WHERE $1::timestamptz IS NULL OR p.occurred_on < $1), 0)::BIGINT
                      as "balance!",
                  count(p.id) FILTER (WHERE $1::timestamptz IS NULL OR p.occurred_on < $1)
                      as "postings!"
             FROM proj_ledger.account a
             LEFT JOIN proj_ledger.posting p ON p.account = a.code
            GROUP BY a.code, a.name, a.kind, a.currency, a.closed
            ORDER BY a.code"#,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(AccountBalance {
                code: row.code,
                name: row.name,
                kind: row
                    .kind
                    .parse()
                    .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
                balance: Money::from_minor(row.balance, currency),
                closed: row.closed,
                postings: row.postings,
            })
        })
        .collect()
}

/// An account and what it came to on a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementLine {
    pub code: String,
    pub name: String,
    pub kind: AccountKind,
    /// Signed as the postings are: positive debit, negative credit. The route
    /// presents each kind on its natural side.
    pub balance: Money,
    pub postings: i64,
}

/// **What the trading accounts did in `[from, until)`**, optionally at one
/// branch: every revenue and expense account, with the sum of its postings in
/// the range. Accounts with nothing in the range come back at zero so a
/// statement can show the whole chart when asked.
///
/// # Errors
/// If the database does.
pub async fn profit_and_loss(
    conn: &mut PgConnection,
    from: Timestamp,
    until: Timestamp,
    branch: Option<&str>,
) -> Result<Vec<StatementLine>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT a.code as "code!", a.name as "name!", a.kind as "kind!",
                  a.currency as "currency!",
                  COALESCE(sum(p.amount), 0)::BIGINT as "balance!",
                  count(p.id) as "postings!"
             FROM proj_ledger.account a
             LEFT JOIN proj_ledger.posting p
               ON p.account = a.code
              AND p.occurred_on >= $1 AND p.occurred_on < $2
              AND ($3::text IS NULL OR p.branch = $3)
            WHERE a.kind IN ('revenue', 'expense')
            GROUP BY a.code, a.name, a.kind, a.currency
            ORDER BY a.kind DESC, a.code"#,
        from,
        until,
        branch,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|r| statement_line(r.code, r.name, &r.kind, &r.currency, r.balance, r.postings))
        .collect()
}

/// What one currency's trading accounts have come to, split at the fiscal
/// year — the two equity lines a balance sheet carries that no account holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradingResult {
    pub currency: CurrencyCode,
    /// Revenue and expense postings from the fiscal year's start to `as_at`,
    /// signed as postings are — so a profit is negative.
    pub current_year: Money,
    /// The same before the fiscal year started: every prior year not yet
    /// closed into retained earnings.
    pub prior_years: Money,
    /// Every posting before `as_at`. Zero on a healthy ledger; anything else
    /// means the sheet cannot balance and must not be shown.
    pub difference: Money,
}

/// The parts of a balance sheet as at `as_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetParts {
    /// Every asset, liability and equity account with its balance at `as_at`.
    pub lines: Vec<StatementLine>,
    /// One per currency that has any posting before `as_at`.
    pub results: Vec<TradingResult>,
}

/// **The balance sheet's figures as at an instant.** The standing accounts at
/// their balances, and per currency the trading result since the fiscal year
/// started and before it, which the sheet shows as equity because no closing
/// entry has moved them there.
///
/// # Errors
/// If the database does.
pub async fn balance_sheet(
    conn: &mut PgConnection,
    as_at: Timestamp,
    fiscal_year_start: Timestamp,
) -> Result<SheetParts, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT a.code as "code!", a.name as "name!", a.kind as "kind!",
                  a.currency as "currency!",
                  COALESCE(sum(p.amount), 0)::BIGINT as "balance!",
                  count(p.id) as "postings!"
             FROM proj_ledger.account a
             LEFT JOIN proj_ledger.posting p
               ON p.account = a.code AND p.occurred_on < $1
            WHERE a.kind IN ('asset', 'liability', 'equity')
            GROUP BY a.code, a.name, a.kind, a.currency
            ORDER BY a.kind, a.code"#,
        as_at,
    )
    .fetch_all(&mut *conn)
    .await?;
    let lines = rows
        .into_iter()
        .map(|r| statement_line(r.code, r.name, &r.kind, &r.currency, r.balance, r.postings))
        .collect::<Result<Vec<_>, _>>()?;

    let results = sqlx::query!(
        r#"SELECT p.currency as "currency!",
                  COALESCE(sum(p.amount) FILTER (
                      WHERE a.kind IN ('revenue', 'expense') AND p.occurred_on >= $2), 0)::BIGINT
                      as "current_year!",
                  COALESCE(sum(p.amount) FILTER (
                      WHERE a.kind IN ('revenue', 'expense') AND p.occurred_on < $2), 0)::BIGINT
                      as "prior_years!",
                  COALESCE(sum(p.amount), 0)::BIGINT as "difference!"
             FROM proj_ledger.posting p
             LEFT JOIN proj_ledger.account a ON a.code = p.account
            WHERE p.occurred_on < $1
            GROUP BY p.currency
            ORDER BY p.currency"#,
        as_at,
        fiscal_year_start,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| {
        let currency =
            CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        Ok(TradingResult {
            currency,
            current_year: Money::from_minor(row.current_year, currency),
            prior_years: Money::from_minor(row.prior_years, currency),
            difference: Money::from_minor(row.difference, currency),
        })
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok(SheetParts { lines, results })
}

/// One row of a statement query into a line. Both statement queries return
/// the same six columns, and this is the one place they are decoded.
fn statement_line(
    code: String,
    name: String,
    kind: &str,
    currency: &str,
    balance: i64,
    postings: i64,
) -> Result<StatementLine, sqlx::Error> {
    let currency = CurrencyCode::new(currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(StatementLine {
        code,
        name,
        kind: kind
            .parse()
            .map_err(|e: String| sqlx::Error::Decode(Box::new(std::io::Error::other(e))))?,
        balance: Money::from_minor(balance, currency),
        postings,
    })
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

/// One line of one entry, as the journal shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalLine {
    pub account: String,
    /// The account's name, or empty for a code the chart no longer has.
    pub name: String,
    /// Signed as posted: positive debit, negative credit.
    pub amount: Money,
    pub memo: Option<String>,
}

/// One entry with its lines — what the journal lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntryView {
    pub id: String,
    pub occurred_on: Timestamp,
    pub recorded_at: Timestamp,
    pub branch: Option<String>,
    pub lines: Vec<JournalLine>,
}

/// What the journal is filtered by. Every field optional; an absent range
/// is the whole log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalFilter<'a> {
    pub from: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// Entries with a line on this account.
    pub account: Option<&'a str>,
    pub branch: Option<&'a str>,
}

/// **The journal, newest first**, one page at a time.
///
/// Two queries: the page of entries, then their lines. The cursor is
/// `(occurred_on, entry_id)`, which is also the order, so a page is
/// stable under new postings — they land at the front, not in the middle.
///
/// # Errors
/// If the database does.
pub async fn journal(
    conn: &mut PgConnection,
    filter: &JournalFilter<'_>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<JournalEntryView>, sqlx::Error> {
    let (before_on, before_id) = match after.map(Cursor::parts) {
        Some([on, id, ..]) => (on.parse::<Timestamp>().ok(), Some(id.clone())),
        _ => (None, None),
    };
    let heads = sqlx::query!(
        r#"SELECT p.entry_id as "entry_id!",
                  min(p.occurred_on) as "occurred_on!",
                  min(p.recorded_at) as "recorded_at!",
                  min(p.branch) as branch
             FROM proj_ledger.posting p
            WHERE ($1::timestamptz IS NULL OR p.occurred_on >= $1)
              AND ($2::timestamptz IS NULL OR p.occurred_on < $2)
              AND ($3::text IS NULL OR p.branch = $3)
              AND ($4::text IS NULL OR p.entry_id IN (
                      SELECT entry_id FROM proj_ledger.posting WHERE account = $4))
              AND ($5::timestamptz IS NULL OR (p.occurred_on, p.entry_id) < ($5, $6))
            GROUP BY p.entry_id
            ORDER BY min(p.occurred_on) DESC, p.entry_id DESC
            LIMIT $7"#,
        filter.from,
        filter.until,
        filter.branch,
        filter.account,
        before_on,
        before_id,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    let ids: Vec<String> = heads.iter().map(|h| h.entry_id.clone()).collect();
    let mut lines = lines_of(&mut *conn, &ids).await?;
    let items = heads
        .into_iter()
        .map(|h| JournalEntryView {
            lines: lines.remove(&h.entry_id).unwrap_or_default(),
            id: h.entry_id,
            occurred_on: h.occurred_on,
            recorded_at: h.recorded_at,
            branch: h.branch,
        })
        .collect();
    Ok(Page::of(items, limit, |e| {
        let on = e.occurred_on.to_rfc3339();
        Cursor::over(&[on.as_str(), e.id.as_str()])
    }))
}

/// One entry by id, with its lines, or `None`.
///
/// # Errors
/// If the database does.
pub async fn journal_entry(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<JournalEntryView>, sqlx::Error> {
    let head = sqlx::query!(
        r#"SELECT min(occurred_on) as "occurred_on!", min(recorded_at) as "recorded_at!",
                  min(branch) as branch
             FROM proj_ledger.posting
            WHERE entry_id = $1
            HAVING count(*) > 0"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(head) = head else {
        return Ok(None);
    };
    let mut lines = lines_of(&mut *conn, &[id.to_owned()]).await?;
    Ok(Some(JournalEntryView {
        lines: lines.remove(id).unwrap_or_default(),
        id: id.to_owned(),
        occurred_on: head.occurred_on,
        recorded_at: head.recorded_at,
        branch: head.branch,
    }))
}

/// The lines of the named entries, by entry, in posting order.
async fn lines_of(
    conn: &mut PgConnection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<JournalLine>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT p.entry_id as "entry_id!", p.account as "account!", a.name as "name?",
                  p.amount as "amount!", p.currency as "currency!", p.memo
             FROM proj_ledger.posting p
             LEFT JOIN proj_ledger.account a ON a.code = p.account
            WHERE p.entry_id = ANY($1)
            ORDER BY p.entry_id, p.line_index"#,
        ids,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut by_entry: std::collections::HashMap<String, Vec<JournalLine>> =
        std::collections::HashMap::new();
    for row in rows {
        let currency =
            CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        by_entry.entry(row.entry_id).or_default().push(JournalLine {
            account: row.account,
            name: row.name.unwrap_or_default(),
            amount: Money::from_minor(row.amount, currency),
            memo: row.memo,
        });
    }
    Ok(by_entry)
}
