//! The ledger's read models, and the invariant that checks them.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Money};
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
