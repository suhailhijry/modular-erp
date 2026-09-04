//! An attempt to collect money, and how it ended.
//!
//! # Why this is an aggregate and not a field on the invoice
//!
//! A payment is a **conversation with somebody else's system**. It is created,
//! and then minutes or days later a customer finishes a 3-D Secure challenge,
//! or does not; a lender approves them, or declines; a capture lands, or the
//! gateway is down. `sales` records that an invoice was paid, which is one
//! fact. This records the attempt, which is a sequence — including the attempts
//! that failed, which an invoice has nowhere to put and which are exactly what
//! somebody asks about when a customer says they were charged.
//!
//! # The gateway's id is the identity
//!
//! Every later message about a payment — a callback, a capture, a refund —
//! names the gateway's own id and nothing else. So that is what this aggregate
//! is keyed on, and it is why [`Started`] carries it: an attempt this system
//! knows about and cannot match to a callback is an attempt nobody can settle.
//!
//! [`Started`]: PaymentEvent::Started

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, Money, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What happened to one attempt to collect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PaymentEvent {
    /// A charge was created at the gateway. **Nobody has paid anything yet.**
    Started {
        /// `moyasar`, `tabby`, `tamara`, or whatever a tenant configures.
        provider: String,
        /// The gateway's own id for it. What every later message names.
        gateway_id: String,
        /// What this is collecting against.
        invoice: AggregateId,
        amount: Money,
        started_at: Timestamp,
    },
    /// The gateway confirmed the money moved.
    ///
    /// **Only ever written from a `fetch`**, never from a callback body — see
    /// the module docs on `crate::commands`.
    ///
    /// It carries the invoice and the account as well as the amount, and that
    /// is deliberate. Everything the posting needs is **on the event**, so the
    /// command can decide inside `try_execute` — where an aggregate is allowed
    /// to be loaded (L7) — and post afterwards without reading the aggregate
    /// again. The account is the resolved one rather than a reference to the
    /// configuration it came from, which is L5.
    Settled {
        amount: Money,
        /// The gateway's cut, when it says. `None` is ordinary: most report it
        /// on the payout rather than on the payment.
        fee: Option<Money>,
        /// What this cleared.
        invoice: AggregateId,
        /// Where the money landed — a card clearing account, or an instalment
        /// provider's receivable.
        into: AggregateId,
        /// The gateway's own id, which is the reference `sales` dedupes on.
        reference: String,
        settled_at: Timestamp,
    },
    /// Refused, and the same request will be refused again. A declined card, a
    /// customer a lender would not lend to.
    Failed {
        /// In the gateway's words, for a person to read.
        why: String,
        failed_at: Timestamp,
    },
    /// Money given back, in full or in part.
    ///
    /// Carries what the posting needs, for the reason [`Self::Settled`] does.
    Refunded {
        amount: Money,
        invoice: AggregateId,
        /// Where it comes back out of — the account it went into.
        out_of: AggregateId,
        /// The caller's own reference for this refund.
        reference: String,
        refunded_at: Timestamp,
    },
    /// Cancelled before it settled. Cheaper than a refund, and possible for a
    /// much shorter time.
    Voided { voided_at: Timestamp },
}

impl PaymentEvent {
    pub const NAMES: [&'static str; 5] = [
        "payments.payment.started",
        "payments.payment.settled",
        "payments.payment.failed",
        "payments.payment.refunded",
        "payments.payment.voided",
    ];
}

impl DomainEvent for PaymentEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Started { .. } => Self::NAMES[0],
            Self::Settled { .. } => Self::NAMES[1],
            Self::Failed { .. } => Self::NAMES[2],
            Self::Refunded { .. } => Self::NAMES[3],
            Self::Voided { .. } => Self::NAMES[4],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// Where an attempt has got to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Created, and waiting on the customer or the gateway.
    #[default]
    Pending,
    Settled,
    Failed,
    Refunded,
    Voided,
}

impl Stage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Settled => "settled",
            Self::Failed => "failed",
            Self::Refunded => "refunded",
            Self::Voided => "voided",
        }
    }

    /// Whether anything more can happen to it.
    ///
    /// A settled payment is **not** finished: it can still be refunded. A
    /// failed one is, which is what makes a retry a new attempt rather than a
    /// revival of this one.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Failed | Self::Voided)
    }
}

/// One attempt, as the log describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Payment {
    pub started: bool,
    pub provider: String,
    pub gateway_id: String,
    pub invoice: Option<AggregateId>,
    pub amount: Option<Money>,
    pub stage: Stage,
    /// What has been given back so far.
    pub refunded_minor: i64,
}

impl Payment {
    /// Whether the money has arrived and not all of it has gone back.
    #[must_use]
    pub fn is_collected(&self) -> bool {
        matches!(self.stage, Stage::Settled)
    }

    /// What could still be given back.
    #[must_use]
    pub fn refundable(&self) -> Option<Money> {
        let amount = self.amount?;
        if !self.is_collected() {
            return None;
        }
        Some(Money::from_minor(
            amount.minor() - self.refunded_minor,
            amount.currency(),
        ))
    }
}

impl Aggregate for Payment {
    type Event = PaymentEvent;

    fn domain() -> DomainName {
        crate::domain("payments_payment")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            PaymentEvent::Started {
                provider,
                gateway_id,
                invoice,
                amount,
                ..
            } => {
                self.started = true;
                self.provider.clone_from(provider);
                self.gateway_id.clone_from(gateway_id);
                self.invoice = Some(invoice.clone());
                self.amount = Some(*amount);
                self.stage = Stage::Pending;
            }
            PaymentEvent::Settled { amount, .. } => {
                self.stage = Stage::Settled;
                // The gateway is the authority on what was actually taken, and
                // a partial capture is a real thing.
                self.amount = Some(*amount);
            }
            PaymentEvent::Failed { .. } => self.stage = Stage::Failed,
            PaymentEvent::Refunded { amount, .. } => {
                self.refunded_minor += amount.minor();
                if let Some(total) = self.amount
                    && self.refunded_minor >= total.minor()
                {
                    self.stage = Stage::Refunded;
                }
            }
            PaymentEvent::Voided { .. } => self.stage = Stage::Voided,
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

    fn started() -> PaymentEvent {
        PaymentEvent::Started {
            provider: "moyasar".to_owned(),
            gateway_id: "pay_1".to_owned(),
            invoice: id("INV-1"),
            amount: sar(10_000),
            started_at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn settled(amount: Money, fee: Option<Money>) -> PaymentEvent {
        PaymentEvent::Settled {
            amount,
            fee,
            invoice: id("INV-1"),
            into: id("1150"),
            reference: "pay_1".to_owned(),
            settled_at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn refund(amount: Money) -> PaymentEvent {
        PaymentEvent::Refunded {
            amount,
            invoice: id("INV-1"),
            out_of: id("1150"),
            reference: "refund-1".to_owned(),
            refunded_at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn replay(events: &[PaymentEvent]) -> Payment {
        let mut payment = Payment::default();
        for event in events {
            Aggregate::apply(&mut payment, event);
        }
        payment
    }

    #[test]
    fn an_attempt_starts_pending_and_nobody_has_paid() {
        let payment = replay(&[started()]);
        assert_eq!(payment.stage, Stage::Pending);
        assert!(!payment.is_collected());
        assert_eq!(payment.refundable(), None);
        assert_eq!(payment.gateway_id, "pay_1");
    }

    /// **The gateway is the authority on the amount.** A partial capture takes
    /// less than was asked for, and the books have to say what was taken.
    #[test]
    fn a_settlement_for_less_is_what_the_payment_is_worth() {
        let payment = replay(&[started(), settled(sar(6_000), Some(sar(165)))]);
        assert!(payment.is_collected());
        assert_eq!(payment.amount, Some(sar(6_000)));
        assert_eq!(payment.refundable(), Some(sar(6_000)));
    }

    #[test]
    fn refunds_add_up_and_the_last_one_finishes_it() {
        let paid = settled(sar(10_000), None);

        let part = replay(&[started(), paid.clone(), refund(sar(3_000))]);
        assert_eq!(part.stage, Stage::Settled, "still a sale that happened");
        assert_eq!(part.refundable(), Some(sar(7_000)));

        let all = replay(&[started(), paid, refund(sar(3_000)), refund(sar(7_000))]);
        assert_eq!(all.stage, Stage::Refunded);
        assert_eq!(all.refundable(), None);
    }

    /// A failed attempt is over; a settled one is not, because it can still be
    /// refunded. That is what makes a retry a **new** attempt.
    #[test]
    fn only_a_dead_attempt_is_finished() {
        assert!(Stage::Failed.is_finished());
        assert!(Stage::Voided.is_finished());
        assert!(!Stage::Pending.is_finished());
        assert!(!Stage::Settled.is_finished());
        assert!(!Stage::Refunded.is_finished());
    }

    #[test]
    fn every_event_has_a_name_and_they_are_all_different() {
        let events = [
            started(),
            settled(sar(1), None),
            PaymentEvent::Failed {
                why: "declined".to_owned(),
                failed_at: Timestamp::from(chrono::Utc::now()),
            },
            refund(sar(1)),
            PaymentEvent::Voided {
                voided_at: Timestamp::from(chrono::Utc::now()),
            },
        ];
        let names: Vec<_> = events.iter().map(|e| e.event_name().to_string()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "{names:?}");
        assert_eq!(names.len(), PaymentEvent::NAMES.len());
    }
}
