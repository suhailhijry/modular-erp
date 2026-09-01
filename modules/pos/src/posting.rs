//! The two entries the drawer makes for itself.
//!
//! # Why there are only two
//!
//! A till sale posts `Dr receivable, Cr revenue, Cr VAT` and its payment posts
//! `Dr cash, Cr receivable` — and **`sales` writes both of those**, because the
//! sale is its invoice and the tender is its payment. `pos` composes them; it
//! does not repeat them.
//!
//! What is left is the money that moves for reasons a sale cannot explain:
//!
//! | when | entry |
//! |---|---|
//! | Paid out — a banking run, a supplier in cash | `Dr` the account named, `Cr` cash |
//! | Closed short — the count is under the books | `Dr` cash over and short, `Cr` cash |
//! | Closed over — the count is above the books | `Dr` cash, `Cr` cash over and short |
//!
//! **The variance has to post, and that is the accounting reason it exists.** A
//! till that records a shortage and does not book it leaves the ledger saying
//! the drawer holds what it does not, for ever, and the next reconciliation
//! inherits the lie.

use erp_types::{AggregateId, Money};

/// Where the drawer moves value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// The drawer itself. `1000 Cash on hand` in every shipped chart — **not**
    /// `1010`, which is the bank, and a till that posts its takings to the bank
    /// is describing money it does not have yet.
    pub cash: AggregateId,
    /// Where everything that is not cash lands. `1010 Bank` in every shipped
    /// chart: a card settles to a bank and not to the box, which is the same
    /// distinction [`crate::Method::is_in_the_drawer`] makes.
    pub bank: AggregateId,
    /// Where a shortage or an overage lands. `5910` in every shipped chart.
    pub over_short: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "pos.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who never opens the settings gets [`Self::conventional`]. One
    /// who *has* configured it and stored something unusable gets an error
    /// rather than the default, for the reason `prepaid` does the same: a year
    /// of shortages posted to the wrong account is found at an audit.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            cash: code("1000"),
            bank: code("1010"),
            over_short: code("5910"),
        }
    }

    /// Which account a tender lands in.
    #[must_use]
    pub fn for_method(&self, method: crate::Method) -> &AggregateId {
        if method.is_in_the_drawer() {
            &self.cash
        } else {
            &self.bank
        }
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
}

/// Cash out of the drawer for something that is not a refund.
pub(crate) fn entry_for_pay_out(
    amount: Money,
    to: &AggregateId,
    accounts: &PostingAccounts,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    two_sided(to, &accounts.cash, amount)
}

/// What the count disagreed with the books by.
///
/// `variance` is `declared - expected`, so a **negative** one is short: the
/// drawer holds less than the books say, and the difference is an expense.
/// Returns nothing when it is exactly right, because an entry that moves
/// nothing is not an entry.
pub(crate) fn entry_for_variance(
    variance: Money,
    accounts: &PostingAccounts,
) -> Result<Option<ledger::BalancedLines>, ledger::Unbalanced> {
    if variance.is_zero() {
        return Ok(None);
    }
    let short = variance.is_negative();
    let amount = variance.checked_abs()?;

    // Short: the money is gone, so cash comes down and the loss is an expense.
    // Over: cash goes up, and the gain offsets the same expense account.
    let (debit, credit) = if short {
        (&accounts.over_short, &accounts.cash)
    } else {
        (&accounts.cash, &accounts.over_short)
    };
    two_sided(debit, credit, amount).map(Some)
}

fn two_sided(
    debit: &AggregateId,
    credit: &AggregateId,
    value: Money,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    let opposite = value.checked_neg()?;
    ledger::BalancedLines::new(vec![
        ledger::Line::new(debit.clone(), value),
        ledger::Line::new(credit.clone(), opposite),
    ])
}

/// Panics only on a literal in this crate that breaks `AggregateId`, which is a
/// build bug with no runtime recovery.
#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
fn code(literal: &str) -> AggregateId {
    AggregateId::new(literal).expect("account codes in this crate are valid literals")
}
