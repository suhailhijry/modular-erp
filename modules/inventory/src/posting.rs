//! What stock does to the books.
//!
//! | when | entry |
//! |---|---|
//! | Received | `Dr` inventory, `Cr` goods received not invoiced, at what the delivery cost |
//! | Consumed on a document | `Dr` cost of goods sold, `Cr` inventory, at what the lots it took were carried at, plus what no lot could cover |
//! | Restored by a credit note | `Dr` inventory, `Cr` cost of goods sold, at what that consumption froze |
//! | Written off | `Dr` waste, `Cr` inventory, at what the lots it took were carried at |
//! | Counted short | `Dr` count variance, `Cr` inventory |
//! | Counted over | `Dr` inventory, `Cr` count variance |
//!
//! # A movement that is not booked is a lie with a date on it
//!
//! The argument is `pos::posting`'s, and it is the same argument: a till that
//! records a shortage and does not book it leaves the ledger saying the drawer
//! holds what it does not, for ever, and the next reconciliation inherits it.
//! A shelf is the same shelf. Every entry here is built by a pure function over
//! money and account codes, so what the ledger will be told is decided — and
//! tested — before any transaction is open.
//!
//! # Why a receipt posts, and what it posts against
//!
//! A delivery arrives and its invoice arrives, usually on different days. The
//! goods are an asset the moment they land — they are in the building, they can
//! be sold, and they can spoil — so the receipt debits the asset. What it owes
//! is not yet accounts payable, because nobody has billed anything: it is
//! **goods received, not invoiced**, which is what that liability account is
//! for. The supplier's bill then debits the same account on the line that names
//! the product, and the two cancel.
//!
//! Reversed — the bill debiting the asset, which is what this module did until
//! 2026-09-13 — the books say the shelves are empty for as long as the
//! paperwork takes, and the invariant below reports every delivery as a
//! discrepancy until its invoice is typed in.
//!
//! **An invoice that beats its delivery is the same entry the other way round.**
//! The bill posts to the holding account whether or not a receipt has arrived,
//! so the account sits as a debit — *invoiced, not yet received* — until it
//! does. Nothing refuses either order.
//!
//! **The check is still load-bearing**, and now it compares like with like: the
//! asset is written by this module at both ends, so a difference means stock
//! that is really gone rather than paperwork in flight. `crate::value_on_hand`
//! is this module's half; the comparison lives in the composition root, where
//! reading `proj_ledger` beside `proj_inventory` is not the cross-group read L3
//! forbids.
//!
//! # What a sale costs, and what a return gives back
//!
//! [`entry_for_consumption`] runs from `sales::issue_in`, once per line that
//! names a product, so a till sale, a booking bill and a `/v1/sales` invoice
//! all book their cost the same way. A plain product sold off an empty shelf
//! books the **shortfall** too, at the last unit cost the shelf saw: the goods
//! left the building whatever the count says, and an entry that ignored them
//! would leave the asset holding stock nobody has.
//!
//! [`entry_for_restoration`] is that entry reversed when a credit note puts the
//! goods back — at what the consumption froze, never at today's cost and never
//! at a share of what was credited.

use erp_types::{AggregateId, Money};
use ledger::{BalancedLines, Line, Unbalanced};

/// Where this module moves value.
///
/// # Why a count's variance and a write-off's loss are two fields
///
/// Because they are two numbers a manager acts on differently. A write-off is
/// somebody standing in front of the goods saying *this is spoiled* — a cost of
/// doing business the buyer controls, and a café that throws out milk every
/// week has a purchasing problem. A count variance is stock that left without
/// anybody saying so — theft, mis-picking, a delivery short-shipped and signed
/// for — and it is a control failure, which is a different meeting. Netting
/// them makes both unreadable, which is exactly the argument `pos` makes for
/// keeping `5910` out of `5900`.
///
/// **They default to the same code anyway**, and that is not a contradiction.
/// The shipped charts have one general loss account between them, and inventing
/// a second code for them would be a guess about a chart this system does not
/// own — a guess that fails on the first posting, which is what
/// `the_conventional_accounts_exist_in_every_shipped_chart` exists to catch.
/// So the *seam* is two and the *default* is one: a tenant who wants them apart
/// opens an account and splits them in one `PUT`, and a tenant who does not
/// gets one number that is still correct.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostingAccounts {
    /// What is on the shelf, as an asset. `1300` in every shipped chart.
    pub inventory: AggregateId,
    /// **What a delivery owes before anybody has billed for it.** `2010` in
    /// every shipped chart, and a liability: a receipt credits it and the
    /// supplier's bill line that names the product debits it back. A bill that
    /// beats its delivery leaves it a debit, which reads as *invoiced, not yet
    /// received* and is the same account either way round.
    pub goods_received: AggregateId,
    /// What went out the door cost. `5010` in every shipped chart.
    pub cogs: AggregateId,
    /// What a count could not find, or found more of. `5900 Other expenses` in
    /// every shipped chart — **not** `5910`, which is the drawer's: a missing
    /// case of beans is not a cash variance.
    pub variance: AggregateId,
    /// What a write-off threw away, whatever its reason. `5900` too, by
    /// default; see the type's own docs for why the field is separate.
    pub waste: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "inventory.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// A tenant who never opens the settings gets [`Self::conventional`]. One
    /// who *has* configured it and stored something unusable gets an error
    /// rather than the default, for the reason every other module does the
    /// same: a year of losses posted to the wrong account is found at an audit.
    ///
    /// # Errors
    /// A stored value this build cannot read, or the database.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            inventory: code("1300"),
            goods_received: code("2010"),
            cogs: code("5010"),
            variance: code("5900"),
            waste: code("5900"),
        }
    }

    /// Every account this value names, for the caller that has to check them.
    pub(crate) fn all(&self) -> [&AggregateId; 5] {
        [
            &self.inventory,
            &self.goods_received,
            &self.cogs,
            &self.variance,
            &self.waste,
        ]
    }
}

impl Default for PostingAccounts {
    fn default() -> Self {
        Self::conventional()
    }
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

/// **What a delivery put on the shelf**, against what is now owed for it.
///
/// The other half is `purchases`, which debits the same holding account on the
/// bill line that names the product — so between the two the account is what
/// has arrived and not been invoiced, and after both it is zero.
pub(crate) fn entry_for_receipt(
    value: Money,
    accounts: &PostingAccounts,
) -> Result<Option<BalancedLines>, Unbalanced> {
    two_sided(&accounts.inventory, &accounts.goods_received, value)
}

/// **What a document's line cost**, against the asset it came off.
///
/// `cost` is what the portions taken were carried at on their own lots, added
/// up — never an average across the shelf, which is the whole of why this
/// module keeps lots. `crate::consume_in` is what adds them.
pub(crate) fn entry_for_consumption(
    cost: Money,
    accounts: &PostingAccounts,
) -> Result<Option<BalancedLines>, Unbalanced> {
    two_sided(&accounts.cogs, &accounts.inventory, cost)
}

/// **What a return put back**, against the cost it was sold at.
///
/// [`entry_for_consumption`] the other way round, and at what *that*
/// consumption froze rather than at what the shelf costs today: a margin that
/// has been reported is not restated by a customer changing their mind. See
/// `crate::restore_in`.
pub(crate) fn entry_for_restoration(
    cost: Money,
    accounts: &PostingAccounts,
) -> Result<Option<BalancedLines>, Unbalanced> {
    two_sided(&accounts.inventory, &accounts.cogs, cost)
}

/// **What somebody threw away.** The reason travels on the movement, not on the
/// entry: an account code cannot hold four words, and `GET
/// /v1/inventory/movements` is where a loss is explained.
pub(crate) fn entry_for_write_off(
    cost: Money,
    accounts: &PostingAccounts,
) -> Result<Option<BalancedLines>, Unbalanced> {
    two_sided(&accounts.waste, &accounts.inventory, cost)
}

/// **What the count disagreed with the books by**, at what the lots it landed on
/// are carried at and what the debt it settled was charged out at.
///
/// `value` is signed the way the variance is, so a **negative** one is short:
/// the stock is gone, the asset comes down and the difference is an expense. An
/// over-count is the same entry the other way round. Nothing comes back for a
/// variance of zero, because an entry that moves nothing is not an entry — and
/// the ledger refuses one.
pub(crate) fn entry_for_variance(
    value: Money,
    accounts: &PostingAccounts,
) -> Result<Option<BalancedLines>, Unbalanced> {
    if value.is_negative() {
        return two_sided(
            &accounts.variance,
            &accounts.inventory,
            value.checked_abs()?,
        );
    }
    two_sided(&accounts.inventory, &accounts.variance, value)
}

/// Two lines, or none at all.
///
/// **Zero is filtered before the lines are built**, not after: a movement worth
/// nothing — a portion of a lot whose remaining value rounds to zero, a count
/// that found exactly what the books said — has no entry to make, and
/// `BalancedLines` refuses an empty set rather than accepting a pair of zeroes.
/// The same filter `sales::entry_for_issue` puts on its tax line, for the same
/// reason.
fn two_sided(
    debit: &AggregateId,
    credit: &AggregateId,
    value: Money,
) -> Result<Option<BalancedLines>, Unbalanced> {
    if value.is_zero() {
        return Ok(None);
    }
    let opposite = value.checked_neg()?;
    BalancedLines::new(vec![
        Line::new(debit.clone(), value),
        Line::new(credit.clone(), opposite),
    ])
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("SAR is a real code")),
        )
    }

    fn amount_on(lines: &BalancedLines, account: &str) -> i64 {
        lines
            .as_slice()
            .iter()
            .filter(|l| l.account.as_str() == account)
            .map(|l| l.amount.minor())
            .sum()
    }

    /// **The conventional mapping is not a guess about a tenant's chart — it is
    /// checked against the charts this build ships.**
    ///
    /// `1300` and `5010` were in the retail chart alone when this module was
    /// written, and the demo installs `services`. They are in all three now,
    /// the way `2300 Zakat payable` is, and this is what keeps that true.
    #[test]
    fn the_conventional_accounts_exist_in_every_shipped_chart() {
        let accounts = PostingAccounts::conventional();
        for chart in ledger::CHARTS {
            for needed in accounts.all() {
                assert!(
                    chart.accounts.iter().any(|a| a.code == needed.as_str()),
                    "chart {:?} has no account {} — conventional() would fail on \
                     the first posting a tenant on that chart made",
                    chart.id,
                    needed,
                );
            }
        }
    }

    /// **The holding account is a liability in every chart**, not an asset.
    ///
    /// A receipt credits it, so a code that landed in the 1000s by mistake
    /// would put a negative asset on the balance sheet and still balance —
    /// invisible to the trial-balance invariant, which is exactly the class of
    /// mistake `money_that_is_held_rather_than_earned_is_a_liability` pins for
    /// the property chart.
    #[test]
    fn what_is_received_and_not_invoiced_is_owed() {
        let holding = PostingAccounts::conventional().goods_received;
        for chart in ledger::CHARTS {
            let account = chart
                .accounts
                .iter()
                .find(|a| a.code == holding.as_str())
                .unwrap_or_else(|| panic!("chart {:?} has no account {holding}", chart.id));
            assert_eq!(
                account.kind,
                ledger::AccountKind::Liability,
                "chart {:?} makes {holding} a {:?}, and goods arrived and not billed are owed for",
                chart.id,
                account.kind,
            );
        }
    }

    /// **A delivery is an asset the moment it lands**, and what it owes is not
    /// yet accounts payable because nobody has billed anything.
    #[test]
    fn receiving_debits_the_shelf_and_credits_what_is_not_yet_invoiced() {
        let entry = entry_for_receipt(sar(34_000), &PostingAccounts::conventional())
            .expect("balances")
            .expect("something moved");

        assert_eq!(amount_on(&entry, "1300"), 34_000, "onto the shelf");
        assert_eq!(
            amount_on(&entry, "2010"),
            -34_000,
            "owed for, and not to accounts payable until the bill arrives"
        );
        assert_eq!(amount_on(&entry, "2000"), 0, "not the payable");
    }

    /// **A consumption moves cost out of the asset and into the expense**, and
    /// the amount is whatever the lots it took were carried at.
    #[test]
    fn consuming_debits_the_cost_of_what_was_sold() {
        let entry = entry_for_consumption(sar(4_200), &PostingAccounts::conventional())
            .expect("balances")
            .expect("something moved");

        assert_eq!(amount_on(&entry, "5010"), 4_200, "the cost of the goods");
        assert_eq!(amount_on(&entry, "1300"), -4_200, "off the shelf");
    }

    /// A write-off books its loss where a tenant put write-offs, which is not
    /// where it put count variances if it has split them.
    #[test]
    fn a_write_off_and_a_count_can_land_in_different_accounts() {
        let split = PostingAccounts {
            waste: code("5920"),
            ..PostingAccounts::conventional()
        };

        let thrown = entry_for_write_off(sar(1_200), &split)
            .expect("balances")
            .expect("something moved");
        assert_eq!(amount_on(&thrown, "5920"), 1_200);
        assert_eq!(amount_on(&thrown, "5900"), 0, "not the count's account");

        let short = entry_for_variance(sar(-1_200), &split)
            .expect("balances")
            .expect("something moved");
        assert_eq!(amount_on(&short, "5900"), 1_200);
        assert_eq!(amount_on(&short, "5920"), 0, "not the waste account");
    }

    /// **Short books the loss, over books the reverse**, and the two are
    /// mirror images because that is what finding more than the books say is.
    #[test]
    fn a_count_books_its_variance_both_ways() {
        let accounts = PostingAccounts::conventional();

        let short = entry_for_variance(sar(-900), &accounts)
            .expect("balances")
            .expect("something moved");
        assert_eq!(amount_on(&short, "5900"), 900, "the stock is gone");
        assert_eq!(amount_on(&short, "1300"), -900);

        let over = entry_for_variance(sar(900), &accounts)
            .expect("balances")
            .expect("something moved");
        assert_eq!(
            amount_on(&over, "1300"),
            900,
            "there is more than we thought"
        );
        assert_eq!(amount_on(&over, "5900"), -900);
    }

    /// **A count that found exactly what the books said posts nothing.** Not a
    /// pair of zero lines, and not a refusal — there is simply no entry to make.
    #[test]
    fn nothing_that_moved_nothing_posts() {
        let accounts = PostingAccounts::conventional();
        for entry in [
            entry_for_variance(sar(0), &accounts),
            entry_for_consumption(sar(0), &accounts),
            entry_for_write_off(sar(0), &accounts),
            entry_for_receipt(sar(0), &accounts),
        ] {
            assert_eq!(entry.expect("balances"), None);
        }
    }
}
