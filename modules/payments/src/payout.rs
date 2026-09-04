//! What the gateway actually sent, against what it owed.
//!
//! # The reconciliation this module exists to make possible
//!
//! A gateway does not pay per transaction. It batches: a few days of payments,
//! net of its cut, in one transfer. So between the customer paying and the
//! money arriving, the clearing account holds a balance that is **a claim on
//! the gateway**, and the question worth answering is whether the transfer that
//! finally lands is the one those payments add up to.
//!
//! `Dr bank / Cr clearing` is the transfer. What makes it a reconciliation is
//! the list of payments it covers, and the arithmetic on them.
//!
//! # A difference has to post
//!
//! It will not always agree. A chargeback, a fee the settlement report explains
//! and the payment did not, a rounding difference on a currency conversion —
//! all of them show up as a payout that is not the sum of its parts.
//!
//! **The difference is booked, not refused.** That is the same call `pos` makes
//! about a till that counts short: a payout that cannot be recorded leaves the
//! books saying the gateway is still holding money it has already sent, for
//! ever, and the next reconciliation inherits the lie. It goes to its own
//! account so it is visible rather than buried in fees — a chargeback and a
//! processing fee are different facts about a business.
//!
//! # A payout with no list is still worth recording
//!
//! Somebody typing from a bank statement has an amount and a date and no
//! transaction list. That posts, reconciles nothing, and says so: `covered` is
//! zero and the difference is not computed, because there is nothing to compare
//! against. Pretending otherwise would be a reconciliation that always agrees.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PayoutEvent {
    /// The gateway transferred money to the bank.
    ///
    /// Carries everything the posting needs, for the reason
    /// [`crate::PaymentEvent::Settled`] does: the decision happens inside
    /// command handling, where an aggregate may be loaded (L7), and the posting
    /// reads the event rather than the aggregate.
    Received {
        provider: String,
        /// The gateway's own id for the payout.
        reference: String,
        /// **What arrived.** The number on the bank statement.
        amount: Money,
        /// What the covered payments say should have arrived — their amounts
        /// less the fees already booked against them.
        ///
        /// Equal to `amount` when nothing was named to compare against, so the
        /// difference is zero and honest rather than invented.
        expected: Money,
        /// The gateway payment ids this covers. Empty is allowed and means no
        /// reconciliation was possible.
        covers: Vec<String>,
        /// The bank account it landed in.
        into: AggregateId,
        /// Where the money was held until now.
        out_of: AggregateId,
        received_on: Timestamp,
    },
}

impl PayoutEvent {
    pub const NAMES: [&'static str; 1] = ["payments.payout.received"];
}

impl DomainEvent for PayoutEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Received { .. } => Self::NAMES[0],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// One transfer from a gateway, as the log describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Payout {
    pub received: bool,
    pub provider: String,
    pub amount: Option<Money>,
    pub expected: Option<Money>,
    pub covered: usize,
}

impl Payout {
    /// What arrived less what was owed. **Negative is short.**
    ///
    /// `None` when nothing has been received, and zero when nothing was named
    /// to compare against — which is not the same as "it agreed".
    #[must_use]
    pub fn difference(&self) -> Option<Money> {
        let (amount, expected) = (self.amount?, self.expected?);
        Some(Money::from_minor(
            amount.minor() - expected.minor(),
            amount.currency(),
        ))
    }

    /// Whether this payout reconciled to anything at all.
    #[must_use]
    pub const fn reconciled(&self) -> bool {
        self.covered > 0
    }
}

impl Aggregate for Payout {
    type Event = PayoutEvent;

    fn domain() -> DomainName {
        crate::domain("payments_payout")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            PayoutEvent::Received {
                provider,
                amount,
                expected,
                covers,
                ..
            } => {
                self.received = true;
                self.provider.clone_from(provider);
                self.amount = Some(*amount);
                self.expected = Some(*expected);
                self.covered = covers.len();
            }
        }
    }
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

    fn id(value: &str) -> AggregateId {
        AggregateId::new(value).unwrap_or_else(|_| unreachable!())
    }

    fn received(amount: i64, expected: i64, covers: &[&str]) -> PayoutEvent {
        PayoutEvent::Received {
            provider: "moyasar".to_owned(),
            reference: "po_1".to_owned(),
            amount: sar(amount),
            expected: sar(expected),
            covers: covers.iter().map(|s| (*s).to_owned()).collect(),
            into: id("1010"),
            out_of: id("1150"),
            received_on: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn replay(event: &PayoutEvent) -> Payout {
        let mut payout = Payout::default();
        Aggregate::apply(&mut payout, event);
        payout
    }

    #[test]
    fn a_payout_that_agrees_has_no_difference() {
        let payout = replay(&received(11_184, 11_184, &["pay_1", "pay_2"]));
        assert!(payout.received);
        assert!(payout.reconciled());
        assert_eq!(payout.difference(), Some(sar(0)));
        assert_eq!(payout.covered, 2);
    }

    /// **Negative is short**, which is the direction that matters: the gateway
    /// sent less than the payments say it owed.
    #[test]
    fn a_short_payout_says_so_and_by_how_much() {
        let short = replay(&received(11_000, 11_184, &["pay_1"]));
        assert_eq!(short.difference(), Some(sar(-184)));

        let over = replay(&received(11_300, 11_184, &["pay_1"]));
        assert_eq!(over.difference(), Some(sar(116)));
    }

    /// Nothing to compare against is **not** "it agreed", and the read model
    /// has to be able to tell them apart.
    #[test]
    fn a_payout_with_no_list_reconciles_nothing() {
        let payout = replay(&received(11_184, 11_184, &[]));
        assert!(payout.received);
        assert!(!payout.reconciled());
        assert_eq!(payout.difference(), Some(sar(0)));
    }

    #[test]
    fn nothing_received_has_no_difference() {
        assert_eq!(Payout::default().difference(), None);
        assert!(!Payout::default().reconciled());
    }
}
