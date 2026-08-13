//! Journal lines that cannot be unbalanced.

use serde::{Deserialize, Serialize};
use spa_types::{AggregateId, CurrencyCode, Money, MoneyError};

/// One side of an entry.
///
/// **Signed, not a debit/credit pair.** A debit is positive, a credit negative,
/// and "debits equal credits" becomes "the amounts sum to zero" — one check on
/// one number instead of two sums and a comparison, and no way to write a line
/// that is somehow both. Statements render the two columns from the sign, which
/// is a presentation concern and belongs there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub account: AggregateId,
    /// Positive debits the account, negative credits it.
    pub amount: Money,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
}

impl Line {
    #[must_use]
    pub const fn new(account: AggregateId, amount: Money) -> Self {
        Self {
            account,
            amount,
            memo: None,
        }
    }

    #[must_use]
    pub fn with_memo(mut self, memo: impl Into<String>) -> Self {
        self.memo = Some(memo.into());
        self
    }

    #[must_use]
    pub const fn is_debit(&self) -> bool {
        self.amount.is_positive()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Unbalanced {
    #[error("an entry needs at least two lines; got {0}")]
    TooFewLines(usize),
    #[error("line {index} is in {found} but the entry is in {expected}")]
    MixedCurrencies {
        index: usize,
        expected: CurrencyCode,
        found: CurrencyCode,
    },
    #[error("debits and credits differ by {difference}")]
    DoesNotBalance { difference: Money },
    #[error("a line may not be zero (line {index})")]
    ZeroLine { index: usize },
    #[error(transparent)]
    Money(#[from] MoneyError),
}

impl spa_i18n::Localize for Unbalanced {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::{Message, MessageArg};
        match self {
            Self::TooFewLines(n) => Message::new(messages::TOO_FEW_LINES).with(
                "n",
                MessageArg::Count(i64::try_from(*n).unwrap_or(i64::MAX)),
            ),
            Self::MixedCurrencies {
                expected, found, ..
            } => Message::new(messages::MIXED_CURRENCIES)
                .with("expected", MessageArg::text(expected.to_string()))
                .with("found", MessageArg::text(found.to_string())),
            Self::DoesNotBalance { difference } => Message::new(messages::DOES_NOT_BALANCE)
                .with("difference", MessageArg::text(difference.to_string())),
            Self::ZeroLine { .. } => Message::new(messages::ZERO_LINE),
            Self::Money(_) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
        }
    }
}

/// Lines that have been checked, and can only have been checked.
///
/// # Why this type exists
///
/// The private field is the whole point. Every path that produces one goes
/// through [`BalancedLines::new`], and the event payload holds this type — so an
/// unbalanced entry is not something the posting code has to remember to
/// refuse, and not something a replay can resurrect either, because
/// [`Deserialize`] revalidates.
///
/// That last part is the one that matters most. Data written by an older version
/// of this system is exactly where "it was valid when we wrote it" stops being a
/// guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancedLines {
    lines: Vec<Line>,
    /// Cached rather than read from `lines[0]`, so [`Self::currency`] needs no
    /// index and no "cannot happen" fallback.
    currency: CurrencyCode,
}

impl BalancedLines {
    /// Checks that the lines are a legal entry.
    ///
    /// Rejects: fewer than two lines, mixed currencies, a zero line, and any set
    /// whose amounts do not sum to zero.
    pub fn new(lines: Vec<Line>) -> Result<Self, Unbalanced> {
        if lines.len() < 2 {
            return Err(Unbalanced::TooFewLines(lines.len()));
        }

        let currency = lines[0].amount.currency();
        for (index, line) in lines.iter().enumerate() {
            if line.amount.currency() != currency {
                return Err(Unbalanced::MixedCurrencies {
                    index,
                    expected: currency,
                    found: line.amount.currency(),
                });
            }
            if line.amount.is_zero() {
                return Err(Unbalanced::ZeroLine { index });
            }
        }

        let total = Money::checked_sum(lines.iter().map(|l| l.amount), currency)?;
        if !total.is_zero() {
            return Err(Unbalanced::DoesNotBalance { difference: total });
        }

        Ok(Self { lines, currency })
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Line] {
        &self.lines
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        // Never true — two lines minimum — but clippy asks for it next to `len`.
        self.lines.is_empty()
    }

    /// The one currency every line is in.
    #[must_use]
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// Total of the debit side. Equal to the credit side by construction.
    pub fn total_debits(&self) -> Result<Money, MoneyError> {
        Money::checked_sum(
            self.lines.iter().filter(|l| l.is_debit()).map(|l| l.amount),
            self.currency,
        )
    }
}

impl Serialize for BalancedLines {
    /// Stored as the bare list. The currency is derivable, and a redundant field
    /// in an event payload is a field that can disagree with itself.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.lines.serialize(s)
    }
}

impl<'de> Deserialize<'de> for BalancedLines {
    /// Revalidates on the way in.
    ///
    /// Without this, a stored entry from an older build — or a hand-edited
    /// payload — would rebuild into a `BalancedLines` that is not balanced, and
    /// the type's guarantee would hold only for values constructed in this
    /// process.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let lines = Vec::<Line>::deserialize(d)?;
        Self::new(lines).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").expect("valid")
    }
    fn usd() -> CurrencyCode {
        CurrencyCode::new("USD").expect("valid")
    }
    fn account(name: &str) -> AggregateId {
        AggregateId::new(name).expect("valid")
    }
    fn line(name: &str, minor: i64) -> Line {
        Line::new(account(name), Money::from_minor(minor, sar()))
    }

    #[test]
    fn a_balanced_pair_is_accepted() {
        let lines =
            BalancedLines::new(vec![line("1000", 5000), line("4000", -5000)]).expect("balances");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines.total_debits().unwrap(),
            Money::from_minor(5000, sar())
        );
    }

    #[test]
    fn an_unbalanced_entry_is_refused_with_the_difference() {
        let error = BalancedLines::new(vec![line("1000", 5000), line("4000", -4900)])
            .expect_err("must not balance");
        assert_eq!(
            error,
            Unbalanced::DoesNotBalance {
                difference: Money::from_minor(100, sar())
            }
        );
    }

    #[test]
    fn one_line_is_not_an_entry() {
        assert_eq!(
            BalancedLines::new(vec![line("1000", 0)]).expect_err("refused"),
            Unbalanced::TooFewLines(1)
        );
        assert_eq!(
            BalancedLines::new(vec![]).expect_err("refused"),
            Unbalanced::TooFewLines(0)
        );
    }

    #[test]
    fn mixing_currencies_is_refused_before_it_is_summed() {
        let lines = vec![
            line("1000", 5000),
            Line::new(account("4000"), Money::from_minor(-5000, usd())),
        ];
        assert!(matches!(
            BalancedLines::new(lines).expect_err("refused"),
            Unbalanced::MixedCurrencies { index: 1, .. }
        ));
    }

    #[test]
    fn a_zero_line_is_refused() {
        // Two zero lines sum to zero, so without this check they would balance —
        // and an entry that moves nothing is a mistake, not a posting.
        assert!(matches!(
            BalancedLines::new(vec![line("1000", 0), line("4000", 0)]).expect_err("refused"),
            Unbalanced::ZeroLine { index: 0 }
        ));
    }

    #[test]
    fn many_lines_balance_as_a_set_not_pairwise() {
        BalancedLines::new(vec![
            line("1000", 10_000),
            line("4000", -6000),
            line("4100", -3000),
            line("4200", -1000),
        ])
        .expect("a set summing to zero is balanced");
    }

    /// **The guarantee that would otherwise hold only in this process.**
    #[test]
    fn a_stored_unbalanced_entry_will_not_decode() {
        let forged = serde_json::json!([
            { "account": "1000", "amount": { "minor": 5000, "currency": "SAR" } },
            { "account": "4000", "amount": { "minor": -1, "currency": "SAR" } }
        ]);
        let decoded = serde_json::from_value::<BalancedLines>(forged);
        assert!(
            decoded.is_err(),
            "an unbalanced payload must not rebuild into a BalancedLines, or the \
             type proves nothing about data written by an older build"
        );
    }

    #[test]
    fn a_balanced_entry_round_trips() {
        let lines =
            BalancedLines::new(vec![line("1000", 5000), line("4000", -5000)]).expect("balances");
        let json = serde_json::to_string(&lines).expect("serializes");
        assert_eq!(
            serde_json::from_str::<BalancedLines>(&json).expect("round trips"),
            lines
        );
    }

    mod properties {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Any set of non-zero amounts, plus the line that offsets their
            /// total, balances — whatever the amounts and however many.
            #[test]
            fn a_balancing_line_always_completes_an_entry(amounts in prop::collection::vec(-1_000_000i32..1_000_000, 1..12)) {
                let amounts: Vec<i64> = amounts
                    .into_iter()
                    .map(i64::from)
                    .filter(|a| *a != 0)
                    .collect();
                prop_assume!(!amounts.is_empty());

                let total: i64 = amounts.iter().sum();
                prop_assume!(total != 0);

                let mut lines: Vec<Line> = amounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Line::new(account(&format!("a{i}")), Money::from_minor(*a, sar())))
                    .collect();
                lines.push(Line::new(account("balance"), Money::from_minor(-total, sar())));

                prop_assert!(BalancedLines::new(lines).is_ok());
            }

            /// And any set that does *not* sum to zero is refused, however it is
            /// arranged.
            #[test]
            fn an_entry_that_does_not_sum_to_zero_is_always_refused(amounts in prop::collection::vec(-1_000_000i32..1_000_000, 2..12)) {
                let amounts: Vec<i64> = amounts.into_iter().map(i64::from).filter(|a| *a != 0).collect();
                prop_assume!(amounts.len() >= 2);
                prop_assume!(amounts.iter().sum::<i64>() != 0);

                let lines: Vec<Line> = amounts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Line::new(account(&format!("a{i}")), Money::from_minor(*a, sar())))
                    .collect();

                // Bound first: `prop_assert!` stringifies its expression into a
                // format string, and the `{ .. }` of a struct pattern breaks it.
                let refused = matches!(
                    BalancedLines::new(lines),
                    Err(Unbalanced::DoesNotBalance { .. })
                );
                prop_assert!(refused);
            }
        }
    }
}
