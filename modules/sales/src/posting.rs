//! Turning a sale into a journal entry.
//!
//! This is the whole of the cross-module integration, and it is deliberately a
//! pure function: an invoice and a set of account codes go in, [`BalancedLines`]
//! comes out. Nothing here touches a database, so what the ledger will be told
//! is decided — and testable — before any transaction is open.

use ledger::{BalancedLines, Line, Unbalanced};
use spa_types::{AggregateId, Money};

use crate::vat::Totals;

/// Which ledger accounts a sale moves.
///
/// # Why this is a struct and not four constants
///
/// It is the seam. Account determination is configuration in every real ERP —
/// by customer group, by item, by branch — and Phase 6 makes it so. Until then
/// [`Self::conventional`] fills it from the codes the shipped charts use, and
/// the only thing that changes when configuration arrives is where this value
/// comes from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// Debited by what customers owe.
    pub receivable: AggregateId,
    /// Credited by what was earned, excluding tax.
    pub revenue: AggregateId,
    /// Credited by tax charged and owed to ZATCA.
    pub output_vat: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "sales.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who never opens the settings gets [`Self::conventional`], which
    /// is the whole of "simplify for the people who do not want the dynamism".
    /// A tenant who *has* configured it and stored something unusable gets an
    /// error rather than the default — silently falling back would hide a
    /// misconfiguration until a month-end reconciliation found it.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, spa_eventlog::ConfigError> {
        Ok(spa_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships.
    ///
    /// A tenant that renamed them is fine — names are cosmetic, codes are the
    /// identity. A tenant that closed or never opened them gets a rejection from
    /// the ledger naming the account, which is the right error to see.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            receivable: code("1100"),
            revenue: code("4000"),
            output_vat: code("2100"),
        }
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
}

/// Panics only on a literal in this crate that breaks `AggregateId`, which
/// `conventional_codes_are_valid` catches at test time.
#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
fn code(literal: &'static str) -> AggregateId {
    AggregateId::new(literal).expect("account codes in this crate are valid literals")
}

/// What a sale does to the books, when the invoice is issued.
///
/// Debit the customer for the whole bill; credit revenue for the part that is
/// income and VAT payable for the part that belongs to the authority. The tax
/// line is omitted when there is no tax rather than posted as zero, because a
/// zero line is not a posting and the ledger refuses one.
pub fn entry_for_issue(
    totals: &Totals,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    let lines = [
        Line::new(accounts.receivable.clone(), totals.gross),
        Line::new(accounts.revenue.clone(), negate(totals.net)?),
        Line::new(accounts.output_vat.clone(), negate(totals.tax)?),
    ];
    BalancedLines::new(lines.into_iter().filter(|l| !l.amount.is_zero()).collect())
}

/// What a receipt does to the books.
///
/// Debit whatever took the money, credit the customer's balance. Nothing here
/// touches revenue — that was recognised when the invoice was issued, and
/// recognising it again on payment is the classic way to double-count a year.
pub fn entry_for_payment(
    amount: Money,
    into: &AggregateId,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    BalancedLines::new(vec![
        Line::new(into.clone(), amount),
        Line::new(accounts.receivable.clone(), negate(amount)?),
    ])
}

fn negate(amount: Money) -> Result<Money, Unbalanced> {
    amount.checked_neg().map_err(Unbalanced::Money)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vat::{Vat, VatCategory, total};
    use spa_types::CurrencyCode;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }

    fn money(minor: i64) -> Money {
        Money::from_minor(minor, sar())
    }

    fn amount_on(lines: &BalancedLines, account: &str) -> i64 {
        lines
            .as_slice()
            .iter()
            .filter(|l| l.account.as_str() == account)
            .map(|l| l.amount.minor())
            .sum()
    }

    #[test]
    fn conventional_codes_are_valid() {
        // `code` expects, so this is the test that keeps it honest.
        let accounts = PostingAccounts::conventional();
        assert_eq!(accounts.receivable.as_str(), "1100");
        assert_eq!(accounts.revenue.as_str(), "4000");
        assert_eq!(accounts.output_vat.as_str(), "2100");
    }

    #[test]
    fn conventional_codes_exist_in_every_shipped_chart() {
        // The real assertion: the default mapping is not a guess about what a
        // tenant's chart contains — it is checked against the charts we ship.
        let accounts = PostingAccounts::conventional();
        for chart in ledger::CHARTS {
            for needed in [
                &accounts.receivable,
                &accounts.revenue,
                &accounts.output_vat,
            ] {
                assert!(
                    chart.accounts.iter().any(|a| a.code == needed.as_str()),
                    "chart {:?} has no account {}",
                    chart.id,
                    needed
                );
            }
        }
    }

    #[test]
    fn issuing_debits_the_customer_and_splits_revenue_from_tax() {
        let standard = Vat::shipped(VatCategory::Standard);
        let totals = total([(standard, money(10_000))], [], sar()).unwrap();
        let entry = entry_for_issue(&totals, &PostingAccounts::conventional()).unwrap();

        assert_eq!(
            amount_on(&entry, "1100"),
            11_500,
            "receivable takes the gross"
        );
        assert_eq!(amount_on(&entry, "4000"), -10_000, "revenue takes the net");
        assert_eq!(amount_on(&entry, "2100"), -1_500, "VAT takes the tax");
    }

    #[test]
    fn a_zero_rated_invoice_posts_two_lines_not_a_zero_tax_line() {
        let zero = Vat::shipped(VatCategory::Zero);
        let totals = total([(zero, money(10_000))], [], sar()).unwrap();
        let entry = entry_for_issue(&totals, &PostingAccounts::conventional()).unwrap();

        assert_eq!(entry.len(), 2);
        assert_eq!(amount_on(&entry, "2100"), 0, "no VAT line at all");
    }

    #[test]
    fn an_invoice_that_comes_to_nothing_is_refused_rather_than_posted_empty() {
        let totals = total([], [], sar()).unwrap();
        assert!(matches!(
            entry_for_issue(&totals, &PostingAccounts::conventional()),
            Err(Unbalanced::TooFewLines(0))
        ));
    }

    #[test]
    fn a_payment_moves_money_without_touching_revenue() {
        let entry = entry_for_payment(
            money(11_500),
            &AggregateId::new("1010").unwrap(),
            &PostingAccounts::conventional(),
        )
        .unwrap();

        assert_eq!(amount_on(&entry, "1010"), 11_500);
        assert_eq!(amount_on(&entry, "1100"), -11_500);
        assert_eq!(
            amount_on(&entry, "4000"),
            0,
            "recognising revenue again on payment is how a year gets double-counted"
        );
    }

    #[test]
    fn a_discount_that_cancels_the_sale_still_balances() {
        // Net zero overall but tax on one band only: +100 standard, -100 exempt.
        let standard = Vat::shipped(VatCategory::Standard);
        let exempt = Vat::shipped(VatCategory::Exempt);
        let totals = total(
            [(standard, money(10_000)), (exempt, money(-10_000))],
            [],
            sar(),
        )
        .unwrap();

        assert_eq!(totals.net, money(0));
        assert_eq!(totals.tax, money(1_500));

        let entry = entry_for_issue(&totals, &PostingAccounts::conventional()).unwrap();
        // The revenue line is zero and dropped; receivable and VAT remain.
        assert_eq!(entry.len(), 2);
        assert_eq!(amount_on(&entry, "1100"), 1_500);
        assert_eq!(amount_on(&entry, "2100"), -1_500);
    }
}
