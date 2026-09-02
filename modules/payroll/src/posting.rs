//! The entry a payroll run makes.
//!
//! # One entry for the run, not one per person
//!
//! A hundred employees is a hundred payslips and **one** journal entry. The
//! ledger records that the business incurred a wage cost and owes it; who it
//! owes it to is the payroll module's business, and a chart with a hundred lines
//! a month is a chart nobody can read.
//!
//! ```text
//! Dr  Wages and salaries      gross
//!     Cr  Payroll deductions            deductions
//!     Cr  Wages payable                 net
//! ```
//!
//! **Gross is the expense, and net is what is owed.** Deductions are money the
//! business is holding on somebody's behalf — a repayment of an advance, a loan
//! instalment — so they are a liability and not a reduction of cost. Netting
//! them against the expense would understate what the business spent on wages,
//! which is the number every management report is about.
//!
//! # What this module does not post
//!
//! **The payment.** Money actually leaving the bank is a separate act, days
//! later, in one transfer that covers everybody — and pretending the run paid
//! people would say the bank balance moved when it did not.

use erp_types::{AggregateId, Money};

/// Where a payroll run moves value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// The cost. `5000 Salaries and wages` in every shipped chart.
    ///
    /// **Not `5100`**, which is Rent — a mistake the first version of this made
    /// and the exit-criterion test caught, because a wrong default here posts a
    /// year of wages somewhere nobody looks until an audit.
    pub expense: AggregateId,
    /// What is owed to people until it is paid. `2200 Salaries payable`.
    pub payable: AggregateId,
    /// What is being held on their behalf. `2210 Payroll deductions`.
    pub withheld: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "payroll.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who has *configured* it and stored something unusable gets an
    /// error rather than the default, for the reason `pos` and `prepaid` do the
    /// same: a year of wages posted to the wrong account is found at an audit.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    #[must_use]
    pub fn conventional() -> Self {
        Self {
            expense: code("5000"),
            payable: code("2200"),
            withheld: code("2210"),
        }
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
}

/// What a run posts.
///
/// The deduction line is **omitted when it is zero**, because an entry that
/// moves nothing on a line is a line nobody wants on a report — and most runs
/// have no deductions at all.
pub(crate) fn entry_for_run(
    gross: Money,
    deductions: Money,
    net: Money,
    accounts: &PostingAccounts,
) -> Result<ledger::BalancedLines, ledger::Unbalanced> {
    let mut lines = vec![
        ledger::Line::new(accounts.expense.clone(), gross),
        ledger::Line::new(accounts.payable.clone(), net.checked_neg()?),
    ];
    if !deductions.is_zero() {
        lines.push(ledger::Line::new(
            accounts.withheld.clone(),
            deductions.checked_neg()?,
        ));
    }
    ledger::BalancedLines::new(lines)
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

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("a real code")),
        )
    }

    /// **Gross is the expense, not net.**
    ///
    /// Netting deductions against the cost would understate what the business
    /// spent on wages, which is the number every management report is about —
    /// and the money withheld is a liability, because it is somebody else's.
    #[test]
    fn the_expense_is_the_whole_cost_and_what_is_withheld_is_owed() {
        let entry = entry_for_run(
            sar(10_000),
            sar(1_500),
            sar(8_500),
            &PostingAccounts::conventional(),
        )
        .expect("balances");

        let lines = entry.as_slice();
        let expense = lines
            .iter()
            .find(|l| l.account.as_str() == "5000")
            .expect("an expense line");
        assert_eq!(expense.amount, sar(10_000), "the cost was netted down");

        let payable = lines
            .iter()
            .find(|l| l.account.as_str() == "2200")
            .expect("a payable line");
        assert_eq!(payable.amount, sar(-8_500));

        let withheld = lines
            .iter()
            .find(|l| l.account.as_str() == "2210")
            .expect("a withheld line");
        assert_eq!(withheld.amount, sar(-1_500));
    }

    /// A run with nothing taken off makes a two-line entry, because a line that
    /// moves nothing is a line nobody wants on a report.
    #[test]
    fn a_run_with_no_deductions_posts_two_lines() {
        let entry = entry_for_run(
            sar(10_000),
            sar(0),
            sar(10_000),
            &PostingAccounts::conventional(),
        )
        .expect("balances");
        assert_eq!(entry.len(), 2);
    }
}
