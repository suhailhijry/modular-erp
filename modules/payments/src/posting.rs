//! Where a gateway's money and a gateway's cut go.
//!
//! # Most of the posting is not here, on purpose
//!
//! A settled payment is a payment **against an invoice**, and `sales::pay_in`
//! already knows what that means: it clears the receivable, refuses an
//! overpayment, and is idempotent on the reference. Writing that again here
//! would be a second opinion about the same money, and the two would disagree
//! the first time one of them changed.
//!
//! So this module tells `sales` which account the money landed in, and `sales`
//! posts `Dr <that account> / Cr receivable`. What is left over — the piece
//! `sales` has no concept of — is the fee.
//!
//! # A fee is an expense, never a smaller sale
//!
//! A tenant that nets the gateway's cut against revenue cannot answer what it
//! actually sold, and the VAT return it files is wrong: the sale was a hundred
//! riyals and ZATCA is owed tax on a hundred riyals, whatever the gateway kept.
//!
//! So the fee is its own entry — `Dr fees / Cr the clearing account` — which
//! leaves the clearing account holding **net**, which is what the gateway will
//! actually pay over. A reconciliation against the payout then has something to
//! reconcile to.
//!
//! # Two clearing accounts, because two different people owe the money
//!
//! A card settles from the customer's bank in a day or two. Buy-now-pay-later
//! does not: the *provider* pays the merchant and collects from the buyer, so
//! after a capture the money is owed by Tabby or Tamara. That is a different
//! counterparty and a different credit risk, and netting the two into one
//! account hides which of them owes what.

use erp_types::AggregateId;
use erp_types::Money;
use ledger::{BalancedLines, Line, Unbalanced};
use serde::{Deserialize, Serialize};

/// Which accounts this tenant settles gateway money through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostingAccounts {
    /// Money a card gateway has taken and not yet paid over.
    pub clearing: AggregateId,
    /// What an instalment provider owes after a capture.
    pub instalments: AggregateId,
    /// The gateway's cut.
    pub fees: AggregateId,
    /// **What a payout disagreed with the books by.** A chargeback, a fee the
    /// settlement report explains and the payment did not, a rounding
    /// difference on a conversion. Its own account rather than lumped into
    /// [`Self::fees`], because a chargeback and a processing fee are different
    /// facts about a business.
    pub differences: AggregateId,
}

impl PostingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "payments.posting_accounts";

    /// What this tenant has configured, or what ships.
    ///
    /// The same call `sales::PostingAccounts` makes: a tenant who never opens
    /// the settings gets the conventional codes, and one who *has* configured
    /// something unusable gets an error rather than a silent fallback that
    /// hides the misconfiguration until a month-end.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::conventional, |configured| configured.value))
    }

    /// The codes every chart in `ledger::CHARTS` ships.
    #[must_use]
    pub fn conventional() -> Self {
        Self {
            clearing: code("1150"),
            instalments: code("1160"),
            fees: code("5400"),
            differences: code("5420"),
        }
    }

    /// Where money from this provider lands until it is paid over.
    ///
    /// **The distinction is who owes it**, not who processed it. A card is
    /// collected from the customer's own bank; an instalment provider has paid
    /// the merchant and is collecting from the customer itself.
    #[must_use]
    pub fn holding(&self, kind: Settlement) -> AggregateId {
        match kind {
            Settlement::Card => self.clearing.clone(),
            Settlement::Instalments => self.instalments.clone(),
        }
    }
}

/// Who ends up owing the merchant the money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settlement {
    /// The customer's own bank, through a card scheme.
    Card,
    /// A lender, which has paid the merchant and collects from the customer.
    Instalments,
}

impl Settlement {
    /// What a named provider is.
    ///
    /// **Unknown is a card**, which is the conservative answer: it settles into
    /// the ordinary clearing account, where a reconciliation will find it,
    /// rather than into the instalment receivable where it would misstate what
    /// a lender owes.
    #[must_use]
    pub fn of(provider: &str) -> Self {
        match provider {
            "tabby" | "tamara" => Self::Instalments,
            _ => Self::Card,
        }
    }
}

/// The gateway kept some of it: `Dr fees / Cr the clearing account`.
///
/// Leaves the clearing account holding **net**, which is what will actually be
/// paid over — so a payout has something to reconcile against.
pub fn entry_for_fee(
    fee: Money,
    holding: &AggregateId,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    BalancedLines::new(vec![
        Line::new(accounts.fees.clone(), fee),
        Line::new(
            holding.clone(),
            fee.checked_neg().map_err(Unbalanced::Money)?,
        ),
    ])
}

/// The gateway sent the money on: `Dr bank / Cr where it was held`, and the
/// difference wherever it went.
///
/// **The difference posts rather than blocking the payout.** The same call
/// `pos` makes about a till that counts short: a payout that cannot be recorded
/// leaves the books saying the gateway still holds money it has already sent,
/// for ever, and the next reconciliation inherits the lie.
///
/// `expected` is what the covered payments say should have arrived. When
/// nothing was named, it equals `amount` and the difference is zero — which is
/// honest, because there was nothing to disagree with.
pub fn entry_for_payout(
    amount: Money,
    expected: Money,
    into: &AggregateId,
    out_of: &AggregateId,
    accounts: &PostingAccounts,
) -> Result<BalancedLines, Unbalanced> {
    let difference = Money::from_minor(amount.minor() - expected.minor(), amount.currency());

    let mut lines = vec![
        // What arrived.
        Line::new(into.clone(), amount),
        // What the gateway is no longer holding — the whole claim, not the
        // part that turned up.
        Line::new(
            out_of.clone(),
            expected.checked_neg().map_err(Unbalanced::Money)?,
        ),
    ];
    if !difference.is_zero() {
        // Short means the gateway kept more than the payments accounted for, so
        // the difference is an expense: `Dr differences`. A negative difference
        // negated is a positive debit, which is what this is.
        lines.push(Line::new(
            accounts.differences.clone(),
            difference.checked_neg().map_err(Unbalanced::Money)?,
        ));
    }
    BalancedLines::new(lines)
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
fn code(literal: &'static str) -> AggregateId {
    AggregateId::new(literal).expect("account codes in this crate are valid literals")
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::CurrencyCode;

    fn sar(minor: i64) -> Money {
        Money::from_minor(
            minor,
            CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!()),
        )
    }

    fn code(literal: &str) -> AggregateId {
        AggregateId::new(literal).unwrap_or_else(|_| unreachable!())
    }

    fn amount_on(lines: &BalancedLines, account: &str) -> i64 {
        lines
            .as_slice()
            .iter()
            .filter(|line| line.account.as_str() == account)
            .map(|line| line.amount.minor())
            .sum()
    }

    /// **The entry that keeps revenue honest.** The sale was a hundred riyals
    /// whatever the gateway kept.
    #[test]
    fn a_fee_is_an_expense_and_comes_out_of_what_is_held() {
        let accounts = PostingAccounts::conventional();
        let lines = entry_for_fee(sar(275), &accounts.clearing, &accounts).expect("balances");

        assert_eq!(amount_on(&lines, "5400"), 275);
        assert_eq!(amount_on(&lines, "1150"), -275);
        // Nothing touches revenue, which is the whole point.
        assert_eq!(amount_on(&lines, "4000"), 0);
    }

    /// A lender's fee comes out of what the lender owes, not out of the card
    /// clearing account — otherwise one provider's costs are charged to
    /// another's balance.
    #[test]
    fn an_instalment_fee_comes_out_of_the_instalment_receivable() {
        let accounts = PostingAccounts::conventional();
        let lines = entry_for_fee(sar(1_200), &accounts.instalments, &accounts).expect("balances");

        assert_eq!(amount_on(&lines, "5400"), 1_200);
        assert_eq!(amount_on(&lines, "1160"), -1_200);
        assert_eq!(amount_on(&lines, "1150"), 0);
    }

    /// **The entry that lets a short payout be recorded.**
    #[test]
    fn a_payout_that_agrees_touches_only_the_bank_and_the_clearing_account() {
        let accounts = PostingAccounts::conventional();
        let lines = entry_for_payout(
            sar(11_184),
            sar(11_184),
            &code("1010"),
            &accounts.clearing,
            &accounts,
        )
        .expect("balances");

        assert_eq!(amount_on(&lines, "1010"), 11_184);
        assert_eq!(amount_on(&lines, "1150"), -11_184);
        assert_eq!(amount_on(&lines, "5420"), 0, "nothing to explain");
    }

    /// The gateway sent less than the payments said it owed. The clearing
    /// account still has to come down by the whole claim.
    #[test]
    fn a_short_payout_books_the_difference_as_an_expense() {
        let accounts = PostingAccounts::conventional();
        let lines = entry_for_payout(
            sar(11_000),
            sar(11_184),
            &code("1010"),
            &accounts.clearing,
            &accounts,
        )
        .expect("balances");

        assert_eq!(amount_on(&lines, "1010"), 11_000, "what arrived");
        assert_eq!(amount_on(&lines, "1150"), -11_184, "the whole claim");
        assert_eq!(amount_on(&lines, "5420"), 184, "and the shortfall");
    }

    /// More than expected — an over-recovery, which is a credit to the same
    /// account rather than a different one.
    #[test]
    fn a_payout_over_what_was_owed_credits_the_same_account() {
        let accounts = PostingAccounts::conventional();
        let lines = entry_for_payout(
            sar(11_300),
            sar(11_184),
            &code("1010"),
            &accounts.clearing,
            &accounts,
        )
        .expect("balances");

        assert_eq!(amount_on(&lines, "1010"), 11_300);
        assert_eq!(amount_on(&lines, "1150"), -11_184);
        assert_eq!(amount_on(&lines, "5420"), -116);
    }

    #[test]
    fn a_provider_settles_where_the_money_will_come_from() {
        assert_eq!(Settlement::of("moyasar"), Settlement::Card);
        assert_eq!(Settlement::of("tabby"), Settlement::Instalments);
        assert_eq!(Settlement::of("tamara"), Settlement::Instalments);
        // **Conservative.** A provider nobody has taught this system about
        // lands where a reconciliation will find it, rather than misstating
        // what a lender owes.
        assert_eq!(Settlement::of("something-new"), Settlement::Card);

        let accounts = PostingAccounts::conventional();
        assert_eq!(accounts.holding(Settlement::Card).as_str(), "1150");
        assert_eq!(accounts.holding(Settlement::Instalments).as_str(), "1160");
    }

    /// Every code this module defaults to is one every shipped chart has —
    /// otherwise a tenant's first card payment is rejected by the ledger.
    #[test]
    fn the_conventional_accounts_exist_in_every_chart() {
        let accounts = PostingAccounts::conventional();
        for chart in ledger::CHARTS {
            for code in [
                &accounts.clearing,
                &accounts.instalments,
                &accounts.fees,
                &accounts.differences,
            ] {
                assert!(
                    chart.accounts.iter().any(|a| a.code == code.as_str()),
                    "{} has no {code}",
                    chart.id
                );
            }
        }
    }
}
