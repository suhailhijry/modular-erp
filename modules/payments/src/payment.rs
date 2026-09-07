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

/// Who a prepayment invoice is made out to.
///
/// Deliberately smaller than `sales::Customer` and **not** that type: this ends
/// up in this module's event log, and a log that carries another module's
/// serialization shape is a log that breaks when that module changes one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Buyer {
    pub name: String,
    /// Giving one makes the document a standard invoice, which ZATCA clears
    /// before the buyer may be handed it. Leaving it out makes it simplified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
}

/// Money taken before the supply, and everything needed to bill for it.
///
/// **The net is carried, not derived.** A deposit is a fraction of something
/// already priced — `booking::pricing::Charged` has a net and a gross — so the
/// tax runs forwards from a known net rather than backwards out of a total.
/// Dividing a gross by a rate does not always land: at 15% there is no net
/// whose tax brings it to exactly 10.00. Nothing here has to find out, because
/// nothing here throws the net away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advance {
    /// What the deposit secures. Opaque: this module does not know what a
    /// booking is, the same way `prepaid` does not know what its `against` is.
    pub against: AggregateId,
    /// **Before tax.** The prepayment invoice is raised for this, and the tax
    /// is worked out from it.
    pub net: Money,
    pub buyer: Buyer,
}

/// **What a payment is collecting against.**
///
/// # Why this is not always an invoice
///
/// It was, and the assumption ran all the way through: `Started` carried an
/// invoice, `settle_in` called `sales::pay_in` with it, and every entry cleared
/// a receivable. That is right for a customer paying a bill, and wrong for the
/// money that arrives *before* there is one.
///
/// # But it becomes one, and quickly
///
/// [`Self::Advance`] is a state that lasts from the charge being created to the
/// gateway confirming it, and no longer. **Receiving consideration is itself a
/// tax point** — the earliest of supply, invoice and payment is what makes VAT
/// due — so settling a deposit raises a *prepayment invoice* for it there and
/// then. Everything after that is an ordinary invoice payment: a refund is a
/// credit note and the money back, exactly as it is for any other sale.
///
/// The first version of this held the money in a liability with no document and
/// no tax, and declared the tax later, when the business decided to keep it.
/// That put the output tax in whatever quarter the customer failed to turn up
/// in rather than the one they paid in — the same defect §31 fixed in the other
/// direction, and the reason this shape replaced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collects {
    /// A bill. `sales` clears the receivable and dedupes on the gateway's id.
    Invoice(AggregateId),
    /// Money taken before anything was billed. Settling it raises the bill.
    Advance(Advance),
}

impl Collects {
    /// **The invoice this settles against, raising one if it has to.**
    ///
    /// A deposit's document is derived from the payment's own id rather than
    /// chosen, so the same answer comes back however often it is asked and
    /// whoever asks — which is what lets a refund months later find the
    /// document it has to credit.
    #[must_use]
    pub fn invoice(&self, payment: &AggregateId) -> AggregateId {
        match self {
            Self::Invoice(id) => id.clone(),
            Self::Advance(_) => deposit_invoice(payment),
        }
    }

    /// The deposit's own details, when it is one.
    #[must_use]
    pub const fn advance(&self) -> Option<&Advance> {
        match self {
            Self::Advance(advance) => Some(advance),
            Self::Invoice(_) => None,
        }
    }

    /// Rebuilds one from the pair an event carries.
    ///
    /// **Two fields on the wire and one enum in the domain**, which is the same
    /// split `sales` makes between `DraftLine` and `InvoiceLine`. The pair is
    /// what lets every event written before deposits existed decode as what it
    /// was — an invoice payment — with no upcaster and no version two.
    #[must_use]
    pub fn of(invoice: Option<&AggregateId>, advance: Option<&Advance>) -> Option<Self> {
        match (invoice, advance) {
            (Some(id), None) => Some(Self::Invoice(id.clone())),
            (None, Some(advance)) => Some(Self::Advance(advance.clone())),
            // Neither, or both. A payment that collects against nothing cannot
            // be settled, and one that names two things is a bug in whatever
            // wrote it — neither is a state to guess at (L6).
            _ => None,
        }
    }

    /// The pair to write onto an event.
    #[must_use]
    pub fn split(&self) -> (Option<AggregateId>, Option<Advance>) {
        match self {
            Self::Invoice(id) => (Some(id.clone()), None),
            Self::Advance(advance) => (None, Some(advance.clone())),
        }
    }
}

/// The prepayment invoice a deposit is billed under.
///
/// Derived, so it is the same string every time it is worked out and nothing
/// has to be stored to find it again.
#[expect(
    clippy::expect_used,
    reason = "a prefix on an id that is already valid is valid"
)]
#[must_use]
pub fn deposit_invoice(payment: &AggregateId) -> AggregateId {
    AggregateId::new(format!("dep-{}", payment.as_str())).expect("a prefixed aggregate id is one")
}

/// What happened to one attempt to collect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PaymentEvent {
    /// Somebody asked for a saved card to be charged, and nothing has been
    /// sent to the gateway yet.
    ///
    /// **The only event this module writes that names a card.** A saved-card
    /// charge cannot happen in a request handler — it is an outbound call to a
    /// third party, and this system makes those from the worker, the way it
    /// submits to ZATCA. So the request records the intent, and
    /// `crate::charge_requested` is what turns it into a [`Self::Started`].
    ///
    /// The `card` is here rather than looked up later for the same reason
    /// [`Self::Settled`] carries its invoice: the job may not load an
    /// aggregate to find out what to do (L7).
    Requested {
        /// The saved card to charge, when there is one.
        ///
        /// **`None` is a deposit the customer pays themselves**, in their own
        /// browser, against the id named here. Nothing in this process charges
        /// it; the only question is whether they have. **Whose** it is is the card's to say;
        /// copying the customer here too would be a second place for the same
        /// fact to be wrong.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<AggregateId>,
        provider: String,
        /// See [`Collects`] for why this is a pair rather than an invoice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invoice: Option<AggregateId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        advance: Option<Advance>,
        amount: Money,
        /// Where the gateway sends the customer **if it decides it needs
        /// them**. A saved-card charge usually completes with nobody watching;
        /// one that raises a 3-D Secure challenge does not, and this is where
        /// that lands. See `crate::charge_requested`.
        callback_url: String,
        /// **What a hosted checkout is told**, when the provider hosts one.
        /// `None` is a card, or a charge the customer's browser creates
        /// against this system's id. See [`crate::Checkout`].
        ///
        /// Boxed for the reason `sales::InvoiceEvent::Issued` boxes its
        /// customer: it is the heavy variant, and `Box<T>` serialises as `T`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<Box<crate::Checkout>>,
        requested_at: Timestamp,
    },
    /// A charge was created at the gateway. **Nobody has paid anything yet.**
    Started {
        /// `moyasar`, `tabby`, `tamara`, or whatever a tenant configures.
        provider: String,
        /// The gateway's own id for it. What every later message names.
        gateway_id: String,
        /// What this is collecting against, when it is an invoice.
        ///
        /// **A pair, not an enum, and `Option` where it used to be required.**
        /// Every payment written before this module could collect an advance
        /// carries an invoice and decodes as one, which is what makes this a
        /// widening rather than a new event version. See [`Collects`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invoice: Option<AggregateId>,
        /// The deposit this collects, when there is no invoice yet. Settling it
        /// raises one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        advance: Option<Advance>,
        amount: Money,
        /// **Where the customer goes to pay**, when the provider hosts the
        /// page: a checkout the worker opened, or a 3-D Secure challenge a
        /// saved-card charge raised. `None` when there is nowhere to send
        /// them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pay_at: Option<String>,
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
        /// **What this cleared**, which for a deposit is the prepayment
        /// invoice raised in the same transaction.
        invoice: AggregateId,
        /// The deposit it was, when it was one — so the posting knows to raise
        /// the invoice before paying it. Everything the posting needs is on the
        /// event (L7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        advance: Option<Advance>,
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
    /// **Somebody asked for money to go back**, and the gateway has not been
    /// told yet.
    ///
    /// The first version of the refund route recorded [`Self::Refunded`]
    /// directly — the books, the credit note, everything — and never spoke to
    /// the gateway, on the instruction that the operator would refund there
    /// first. Nothing enforced the order, and a refund recorded before the
    /// gateway agreed is a set of books saying money went back when it did
    /// not. So a refund is now a request the worker carries out, the same shape
    /// as a saved-card charge: this is the intent, `crate::refund_requested` is
    /// the outbound call, and [`Self::Refunded`] is written only from what the
    /// gateway confirmed.
    RefundRequested {
        /// The caller's own reference. A retry with the same one is a retry,
        /// and it becomes the credit note's key.
        reference: String,
        amount: Money,
        /// Why, in the customer's language; printed on the credit note.
        reason: String,
        requested_at: Timestamp,
    },
    /// **The gateway would not give it back**, and will say the same again. A
    /// dead card, a payment too old to refund, an amount the provider will not
    /// split — the reason is theirs, kept for whoever has to explain it.
    RefundRefused {
        reference: String,
        why: String,
        refused_at: Timestamp,
    },
    /// Money given back, in full or in part — **as the gateway confirmed it**.
    ///
    /// Carries what the posting needs, for the reason [`Self::Settled`] does.
    Refunded {
        amount: Money,
        /// What is being credited. A deposit's prepayment invoice is as much an
        /// invoice as any other, which is the point of raising one.
        invoice: AggregateId,
        /// Where it comes back out of — the account it went into.
        out_of: AggregateId,
        /// The caller's own reference for this refund.
        reference: String,
        refunded_at: Timestamp,
    },
    /// **A deposit the business is keeping.** The customer did not come back,
    /// the money stays, and the liability it was held under goes away.
    ///
    /// Only ever written against an advance: an invoice payment has nothing to
    /// retain, because the supply it paid for already happened.
    Retained {
        /// What was kept — everything that had not been given back, including
        /// the tax that was declared on it when it arrived.
        amount: Money,
        /// **The part of `amount` that was never tax**, at the rate the
        /// prepayment invoice actually carried. Worked out here, from the
        /// deposit's own net and gross, rather than from whatever the standard
        /// rate is on the day somebody decides to keep the money — a rate
        /// change between the two would otherwise move the wrong figure out of
        /// revenue (L5). `None` on events written before this was carried.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        net: Option<Money>,
        /// **Whether keeping it counts as a sale.** The tenant's decision, from
        /// `crate::Retention`, recorded on the event rather than looked up
        /// later: a setting changed next year must not restate what the books
        /// said this year (L5).
        supply: bool,
        /// What it was being held against.
        advance_for: AggregateId,
        retained_at: Timestamp,
    },
    /// Cancelled before it settled. Cheaper than a refund, and possible for a
    /// much shorter time.
    Voided { voided_at: Timestamp },
}

impl PaymentEvent {
    pub const NAMES: [&'static str; 9] = [
        "payments.payment.requested",
        "payments.payment.started",
        "payments.payment.settled",
        "payments.payment.failed",
        "payments.payment.refunded",
        "payments.payment.retained",
        "payments.payment.voided",
        "payments.payment.refund_requested",
        "payments.payment.refund_refused",
    ];
}

impl DomainEvent for PaymentEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Requested { .. } => Self::NAMES[0],
            Self::Started { .. } => Self::NAMES[1],
            Self::Settled { .. } => Self::NAMES[2],
            Self::Failed { .. } => Self::NAMES[3],
            Self::Refunded { .. } => Self::NAMES[4],
            Self::Retained { .. } => Self::NAMES[5],
            Self::Voided { .. } => Self::NAMES[6],
            Self::RefundRequested { .. } => Self::NAMES[7],
            Self::RefundRefused { .. } => Self::NAMES[8],
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
    /// A saved-card charge somebody asked for, which the worker has not sent
    /// to the gateway yet. **No money has been asked for**, so there is
    /// nothing to chase at the provider and nothing to reconcile.
    Requested,
    /// Created, and waiting on the customer or the gateway.
    #[default]
    Pending,
    Settled,
    Failed,
    Refunded,
    /// A deposit the customer did not come back for, which the business kept.
    Retained,
    Voided,
}

impl Stage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Pending => "pending",
            Self::Settled => "settled",
            Self::Failed => "failed",
            Self::Refunded => "refunded",
            Self::Retained => "retained",
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
        // **A retained deposit is finished.** The money is the business's, the
        // liability is gone, and there is nothing left to give back — which is
        // exactly what makes it different from `Settled`.
        matches!(self, Self::Failed | Self::Voided | Self::Retained)
    }
}

/// One attempt, as the log describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Payment {
    /// Asked for against a saved card, and not yet sent to the gateway.
    pub requested: bool,
    /// The card a request named, for the job that will charge it. `None` when
    /// the customer pays it themselves.
    pub card: Option<AggregateId>,
    /// Where the gateway sends a customer it decides it needs.
    pub callback_url: String,
    pub started: bool,
    pub provider: String,
    pub gateway_id: String,
    /// **What this payment collects against.** `None` before it is started,
    /// and on a payment whose event named neither an invoice nor an advance —
    /// which is a corrupt log rather than a state a command can produce.
    pub collects: Option<Collects>,
    pub amount: Option<Money>,
    pub stage: Stage,
    /// What has been given back so far.
    pub refunded_minor: i64,
    /// **The references of the refunds already made**, so a retried request is
    /// a no-op rather than a second one.
    ///
    /// Its own list rather than a count, because a retry is identified by the
    /// caller's own key and nothing else: `refunded_minor` reaching the total
    /// says the money is all back, not that *this* request is the one that did
    /// it. `pos` learned the same lesson from a drawer that went down twice.
    pub refunds: Vec<String>,
    /// **Refunds asked for and not yet carried out** at the gateway. Their
    /// amounts are spoken for: a second request cannot take them, and nothing
    /// may be kept while one is open.
    pub awaited_refunds: Vec<RefundRequest>,
    /// References the gateway refused. Kept so a retry of one is the same
    /// answer and not a second attempt.
    pub refused_refunds: Vec<String>,
}

/// One refund asked for and not yet confirmed by the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundRequest {
    pub reference: String,
    pub amount: Money,
    pub reason: String,
}

impl Payment {
    /// Whether this refund has already been made.
    #[must_use]
    pub fn has_refund(&self, reference: &str) -> bool {
        self.refunds.iter().any(|seen| seen == reference)
    }

    /// The open request under this reference, if there is one.
    #[must_use]
    pub fn refund_awaited(&self, reference: &str) -> Option<&RefundRequest> {
        self.awaited_refunds
            .iter()
            .find(|r| r.reference == reference)
    }

    /// Whether the gateway already refused this reference.
    #[must_use]
    pub fn refund_refused(&self, reference: &str) -> bool {
        self.refused_refunds.iter().any(|seen| seen == reference)
    }

    /// What every open request adds up to.
    fn awaited_minor(&self) -> i64 {
        self.awaited_refunds.iter().map(|r| r.amount.minor()).sum()
    }

    /// What could still be given back **to a new request** — after what has
    /// gone back and after what is already spoken for. `None` before the money
    /// arrived, and once the business has kept it.
    #[must_use]
    pub fn refundable(&self) -> Option<Money> {
        let amount = self.amount?;
        if !self.is_collected() {
            return None;
        }
        Some(Money::from_minor(
            amount.minor() - self.refunded_minor - self.awaited_minor(),
            amount.currency(),
        ))
    }

    /// What may go back **under this reference**: what is left, plus what this
    /// very request already reserved for itself — so the worker completing an
    /// awaited refund is not refused for the amount it is completing.
    #[must_use]
    pub fn refundable_for(&self, reference: &str) -> Option<Money> {
        let left = self.refundable()?;
        let reserved = self
            .refund_awaited(reference)
            .map_or(0, |r| r.amount.minor());
        Some(Money::from_minor(left.minor() + reserved, left.currency()))
    }

    /// Whether the money has arrived and not all of it has gone back.
    #[must_use]
    pub fn is_collected(&self) -> bool {
        matches!(self.stage, Stage::Settled)
    }
}

impl Aggregate for Payment {
    type Event = PaymentEvent;

    fn domain() -> DomainName {
        crate::domain("payments_payment")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            PaymentEvent::Requested {
                card,
                provider,
                invoice,
                advance,
                amount,
                callback_url,
                ..
            } => {
                self.requested = true;
                self.card.clone_from(card);
                self.provider.clone_from(provider);
                self.collects = Collects::of(invoice.as_ref(), advance.as_ref());
                self.amount = Some(*amount);
                self.callback_url.clone_from(callback_url);
                self.stage = Stage::Requested;
            }
            PaymentEvent::Started {
                provider,
                gateway_id,
                invoice,
                advance,
                amount,
                ..
            } => {
                self.started = true;
                self.provider.clone_from(provider);
                self.gateway_id.clone_from(gateway_id);
                self.collects = Collects::of(invoice.as_ref(), advance.as_ref());
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
            PaymentEvent::RefundRequested {
                reference,
                amount,
                reason,
                ..
            } => {
                if self.refund_awaited(reference).is_none() {
                    self.awaited_refunds.push(RefundRequest {
                        reference: reference.clone(),
                        amount: *amount,
                        reason: reason.clone(),
                    });
                }
            }
            PaymentEvent::RefundRefused { reference, .. } => {
                self.awaited_refunds.retain(|r| r.reference != *reference);
                self.refused_refunds.push(reference.clone());
            }
            PaymentEvent::Refunded {
                amount, reference, ..
            } => {
                self.awaited_refunds.retain(|r| r.reference != *reference);
                self.refunded_minor += amount.minor();
                self.refunds.push(reference.clone());
                if let Some(total) = self.amount
                    && self.refunded_minor >= total.minor()
                {
                    self.stage = Stage::Refunded;
                }
            }
            PaymentEvent::Retained { .. } => self.stage = Stage::Retained,
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
            pay_at: None,
            provider: "moyasar".to_owned(),
            gateway_id: "pay_1".to_owned(),
            invoice: Some(id("INV-1")),
            advance: None,
            amount: sar(10_000),
            started_at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn settled(amount: Money, fee: Option<Money>) -> PaymentEvent {
        PaymentEvent::Settled {
            amount,
            fee,
            invoice: id("INV-1"),
            advance: None,
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

    /// **A retried refund is one refund.** Recognised by the caller's own key,
    /// because the amount alone cannot tell a retry from a second, identical
    /// refund somebody meant.
    #[test]
    fn a_payment_knows_which_refunds_it_has_already_made() {
        let payment = replay(&[started(), settled(sar(10_000), None), refund(sar(3_000))]);
        assert!(payment.has_refund("refund-1"));
        assert!(!payment.has_refund("refund-2"));
        assert!(!Payment::default().has_refund("refund-1"));
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
            PaymentEvent::Requested {
                checkout: None,
                card: Some(id("card-1")),
                provider: "moyasar".to_owned(),
                invoice: Some(id("INV-1")),
                advance: None,
                amount: sar(10_000),
                callback_url: "https://bassat.sa/paid".to_owned(),
                requested_at: Timestamp::from(chrono::Utc::now()),
            },
            started(),
            settled(sar(1), None),
            PaymentEvent::Failed {
                why: "declined".to_owned(),
                failed_at: Timestamp::from(chrono::Utc::now()),
            },
            refund(sar(1)),
            PaymentEvent::Retained {
                amount: sar(1),
                supply: true,
                advance_for: id("BOOK-1"),
                net: None,
                retained_at: Timestamp::from(chrono::Utc::now()),
            },
            PaymentEvent::Voided {
                voided_at: Timestamp::from(chrono::Utc::now()),
            },
            PaymentEvent::RefundRequested {
                reference: "refund-1".to_owned(),
                amount: sar(1),
                reason: "changed their mind".to_owned(),
                requested_at: Timestamp::from(chrono::Utc::now()),
            },
            PaymentEvent::RefundRefused {
                reference: "refund-1".to_owned(),
                why: "too old".to_owned(),
                refused_at: Timestamp::from(chrono::Utc::now()),
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
