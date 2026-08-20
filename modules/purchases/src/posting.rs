//! Turning a bill into a journal entry.
//!
//! A pure function, for the same reason `sales::posting` is one: what the ledger
//! will be told is decided — and testable — before any transaction is open.
//!
//! # The entry a bill makes
//!
//! Mirror image of a sale. Where an invoice debits what a customer owes and
//! credits revenue, a bill debits what was bought and credits what is owed:
//!
//! ```text
//! Dr  each line's expense or asset account   net
//! Dr  input VAT                              tax          (reclaimable only)
//!     Cr  accounts payable                        gross
//! ```
//!
//! # Why exempt tax does not go to input VAT
//!
//! Input tax on an exempt supply is **not reclaimable** — it is a cost of the
//! purchase, not a debt ZATCA owes back. Putting it in `1200 Input VAT` would
//! claim it, and the reclaim would be disallowed. It goes to the line's own
//! account instead, which is where an irrecoverable cost belongs.
//!
//! In practice a supplier charges no tax on an exempt supply, so this arm is
//! rarely reached — but "rarely" is not "never", and a rule that only holds for
//! the common case is the one that produces an unexplainable balance.

use ledger::{BalancedLines, Line, Unbalanced};
use erp_types::{AggregateId, Money};

/// Which ledger accounts a purchase moves.
///
/// The line's own expense account is on the line, because one bill routinely
/// covers rent and stationery. These two are the ones every bill touches.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// Credited by what is owed to suppliers.
    pub payable: AggregateId,
    /// Debited by reclaimable tax paid, and owed back by ZATCA.
    pub input_vat: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "purchases.posting_accounts";

    /// What this tenant has configured, or what ships.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships — and `charts::tests`
    /// asserts they all do.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            payable: code("2000"),
            input_vat: code("1200"),
        }
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
}

/// The journal entry a bill makes.
///
/// Takes the lines as the supplier stated them; the arithmetic here is only
/// summation, so it cannot disagree with the document.
pub fn entry_for_bill(
    lines: &[crate::bill::BillLine],
    gross: Money,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    let currency = gross.currency();
    let mut entry: Vec<Line> = Vec::with_capacity(lines.len() + 2);
    let mut reclaimable = Money::zero(currency);

    for line in lines {
        // Irrecoverable tax is part of what the thing cost, so it rides on the
        // line's own account rather than going to input VAT.
        let cost = if line.category.input_is_reclaimable() {
            line.net
        } else {
            line.net.checked_add(line.tax)?
        };
        if line.category.input_is_reclaimable() {
            reclaimable = reclaimable.checked_add(line.tax)?;
        }

        // A zero line is not a posting and the ledger refuses one, so a line
        // that comes to nothing is left out rather than sent to be rejected.
        if !cost.is_zero() {
            entry.push(Line::new(line.account.clone(), cost));
        }
    }

    if !reclaimable.is_zero() {
        entry.push(Line::new(accounts.input_vat.clone(), reclaimable));
    }
    entry.push(Line::new(accounts.payable.clone(), gross.checked_neg()?));

    BalancedLines::new(entry)
}

/// The payment a bill settles: what was owed goes down, and the cash goes out.
pub fn entry_for_payment(
    amount: Money,
    from: &AggregateId,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    BalancedLines::new(vec![
        Line::new(accounts.payable.clone(), amount),
        Line::new(from.clone(), amount.checked_neg()?),
    ])
}

/// Panics only on a literal in this crate that breaks `AggregateId`, which is a
/// build bug rather than a runtime condition.
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
    use ledger::VatCategory;
    use erp_types::CurrencyCode;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }
    fn money(minor: i64) -> Money {
        Money::from_minor(minor, sar())
    }
    fn line(account: &str, net: i64, category: VatCategory, tax: i64) -> crate::bill::BillLine {
        crate::bill::BillLine {
            description: "something".to_owned(),
            account: code(account),
            net: money(net),
            category,
            rate_bp: ledger::Rates::saudi_arabia().of(category),
            tax: money(tax),
        }
    }

    /// The shape of a bill's entry: cost in, tax reclaimable, payable owed.
    #[test]
    fn a_standard_rated_bill_debits_the_cost_and_the_tax() {
        let lines = vec![line("5000", 100_000, VatCategory::Standard, 15_000)];
        let entry = entry_for_bill(&lines, money(115_000), &PostingAccounts::conventional())
            .expect("balances");

        let at = |account: &str| {
            entry
                .as_slice()
                .iter()
                .find(|l| l.account.as_str() == account)
                .map(|l| l.amount)
        };
        assert_eq!(at("5000"), Some(money(100_000)), "the cost");
        assert_eq!(at("1200"), Some(money(15_000)), "reclaimable input tax");
        assert_eq!(at("2000"), Some(money(-115_000)), "owed to the supplier");
    }

    /// **The rule that only matters occasionally, and is unexplainable when
    /// wrong.** Irrecoverable tax is a cost, not a receivable.
    #[test]
    fn exempt_tax_is_a_cost_and_never_reaches_input_vat() {
        let lines = vec![line("5100", 50_000, VatCategory::Exempt, 2_000)];
        let entry = entry_for_bill(&lines, money(52_000), &PostingAccounts::conventional())
            .expect("balances");

        let codes: Vec<&str> = entry
            .as_slice()
            .iter()
            .map(|l| l.account.as_str())
            .collect();
        assert!(
            !codes.contains(&"1200"),
            "exempt tax was claimed as reclaimable: {codes:?}"
        );
        assert_eq!(
            entry
                .as_slice()
                .iter()
                .find(|l| l.account.as_str() == "5100")
                .map(|l| l.amount),
            Some(money(52_000)),
            "it rides on the line's own account, as part of what the thing cost"
        );
    }

    /// One bill, three accounts and two treatments — which is what a real one
    /// looks like.
    #[test]
    fn a_mixed_bill_splits_by_line_and_pools_only_the_reclaimable_tax() {
        let lines = vec![
            line("5000", 100_000, VatCategory::Standard, 15_000),
            line("5100", 40_000, VatCategory::Zero, 0),
            line("5200", 10_000, VatCategory::Exempt, 500),
        ];
        let entry = entry_for_bill(&lines, money(165_500), &PostingAccounts::conventional())
            .expect("balances");

        let at = |account: &str| {
            entry
                .as_slice()
                .iter()
                .find(|l| l.account.as_str() == account)
                .map(|l| l.amount)
        };
        assert_eq!(at("5000"), Some(money(100_000)));
        assert_eq!(at("5100"), Some(money(40_000)));
        assert_eq!(at("5200"), Some(money(10_500)), "exempt tax rides along");
        assert_eq!(
            at("1200"),
            Some(money(15_000)),
            "and only the standard tax is claimed"
        );
        assert_eq!(at("2000"), Some(money(-165_500)));
    }

    #[test]
    fn a_payment_moves_what_is_owed_into_the_bank() {
        let entry = entry_for_payment(
            money(11_500),
            &code("1000"),
            &PostingAccounts::conventional(),
        )
        .expect("balances");
        let at = |account: &str| {
            entry
                .as_slice()
                .iter()
                .find(|l| l.account.as_str() == account)
                .map(|l| l.amount)
        };
        assert_eq!(at("2000"), Some(money(11_500)), "the debt goes down");
        assert_eq!(at("1000"), Some(money(-11_500)), "and the cash goes out");
    }
}
