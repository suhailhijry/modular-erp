//! Starting a collection, and recording how it ended.
//!
//! # A callback never decides anything
//!
//! None of the three gateways signs its webhook bodies, so a callback proves
//! only that *somebody* posted to a URL. [`erp_payments::authenticate`] returns
//! a gateway id and deliberately cannot return an amount, and everything below
//! takes a [`Charged`] — which comes from [`erp_payments::Gateway::fetch`],
//! over an authenticated connection.
//!
//! [`settle`] then checks the amount against what was started before it posts
//! anything. That check is the whole defence: a gateway id is not a secret, and
//! without it anybody who watched a customer pay could settle an invoice for a
//! number of their choosing.
//!
//! # Settling composes with `sales`, in one transaction
//!
//! `sales::pay_in` clears the receivable and is idempotent on the payment
//! reference — which is the gateway's own id here, so a callback delivered
//! three times records one payment. The fee is the only entry `sales` knows
//! nothing about, and it posts in the same transaction: a fee recorded without
//! its payment, or the other way round, is a set of books that has to be fixed
//! by hand.

use erp_eventlog::{Committed, Decision, ExecuteError, Metadata, try_execute};
use erp_payments::{Charged, Status};
use erp_types::{AggregateId, Money, Timestamp};

use crate::card::{Card, CardEvent, token_key};
use crate::payment::{Payment, PaymentEvent, Stage};
use crate::payout::{Payout, PayoutEvent};
use crate::posting::{PostingAccounts, Settlement, entry_for_fee, entry_for_payout};

#[derive(Debug, thiserror::Error)]
pub enum PaymentsError {
    #[error("payment {0} has not been started")]
    NotStarted(String),
    #[error("payment {0} has already been started")]
    AlreadyStarted(String),
    /// **The check that stands between a gateway id and the books.**
    #[error("the gateway says {found} and this payment was started for {expected}")]
    WrongAmount { expected: Money, found: Money },
    #[error("payment {id} is {stage} and cannot be settled")]
    NotCollectable { id: String, stage: &'static str },
    #[error("{0} is more than is left to refund")]
    RefundTooLarge(Money),
    #[error("payout {0} has already been recorded")]
    PayoutRecorded(String),
    /// A payout naming payments this system has never settled. **Refused**: the
    /// arithmetic would silently be against a smaller set than the operator
    /// thinks, and the difference would look like a gateway shortfall.
    #[error("{0} is not a settled payment this payout can cover")]
    NotSettled(String),
    /// A provider with no card to save. **Refused rather than stored**: a row
    /// that can never be charged is a saved card as far as a person picking
    /// one is concerned, and they find out at the till.
    #[error("{0} does not hold cards that can be charged later")]
    NoSavedCards(String),
    #[error("there is no saved card {0}")]
    NoSuchCard(String),
    /// **Forgetting is final.** Re-saving means the customer entering their
    /// card again, which mints a new token under a new id.
    #[error("card {0} was forgotten and cannot be used again")]
    CardForgotten(String),
    #[error("that is not a card this system can keep: {0}")]
    NotACard(String),
    /// Sealing or unsealing failed. **Never treated as "no card"** (L6):
    /// a token this system cannot read is a broken deployment, not a customer
    /// without a card on file.
    #[error(transparent)]
    Secret(#[from] erp_eventlog::SecretError),
    #[error("a payout in {found} cannot cover payments in {expected}")]
    PayoutCurrency {
        expected: erp_types::CurrencyCode,
        found: erp_types::CurrencyCode,
    },
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    #[error("the sale could not be settled: {0}")]
    Sales(String),
}

type Outcome = Result<Committed<PaymentEvent>, ExecuteError<PaymentsError>>;

/// What a charge at a gateway was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// `moyasar`, `tabby`, `tamara`.
    pub provider: String,
    /// **The gateway's own id.** What every callback names.
    pub gateway_id: String,
    pub invoice: AggregateId,
    pub amount: Money,
}

/// Records that a charge was created at a gateway.
///
/// **Written before the customer is sent anywhere.** An attempt this system did
/// not write down is an attempt no callback can be matched to, and the customer
/// will still have been charged.
pub async fn start_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    attempt: &Attempt,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.started {
                // A retried request. The stored attempt wins.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PaymentEvent::Started {
                provider: attempt.provider.clone(),
                gateway_id: attempt.gateway_id.clone(),
                invoice: attempt.invoice.clone(),
                amount: attempt.amount,
                started_at: at,
            }))
        },
    )
    .await
}

/// Records what the gateway said, and posts it.
///
/// `charged` must have come from [`erp_payments::Gateway::fetch`]. See the
/// module docs for why nothing here will take a callback body.
///
/// Idempotent under retry (L8): the gateway's id is the payment reference
/// `sales` dedupes on, so a callback delivered three times settles once.
///
/// # Why the decision and the posting are separated
///
/// **An aggregate may only be loaded while handling a command** (L7). So every
/// read of the payment's state happens inside the closure below, and everything
/// the posting needs afterwards — the invoice, the account, the reference —
/// travels on the event it emits rather than being fetched a second time.
pub async fn settle_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    charged: &Charged,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    // Configuration, not an aggregate. Resolved once, before the decision, so
    // the closure has the account to write onto the event.
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;

    let committed = try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.started {
                return Err(PaymentsError::NotStarted(id.as_str().to_owned()));
            }
            // Already settled, failed or voided. Nothing to do, and posting
            // again would be a second payment against the same invoice.
            if state.stage != Stage::Pending {
                return Ok(Decision::nothing());
            }

            // A refusal, a cancellation, or still waiting — none of which
            // posts anything.
            if let Some(nothing_to_post) = unpaid(charged, at) {
                return Ok(nothing_to_post);
            }

            let expected = state
                .amount
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?;
            // **The check.** A gateway id is not a secret, and no callback in
            // this system is signed.
            if charged.amount != expected {
                return Err(PaymentsError::WrongAmount {
                    expected,
                    found: charged.amount,
                });
            }
            let invoice = state
                .invoice
                .clone()
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?;

            Ok(Decision::one(PaymentEvent::Settled {
                amount: charged.amount,
                fee: charged.fee,
                invoice,
                into: accounts.holding(Settlement::of(&state.provider)),
                reference: state.gateway_id.clone(),
                settled_at: at,
            }))
        },
    )
    .await?;

    // **Everything the posting needs is on the event.** No second load.
    let Some(PaymentEvent::Settled {
        amount,
        fee,
        invoice,
        into,
        reference,
        ..
    }) = committed.events.first()
    else {
        return Ok(committed);
    };

    // `sales` owns what a payment does to an invoice: it clears the receivable,
    // refuses an overpayment, and dedupes on the reference.
    sales::pay_in(
        &mut *conn,
        invoice,
        &sales::Receipt {
            reference: reference.clone(),
            amount: *amount,
            received_on: at,
            into: into.clone(),
        },
        &format!("Gateway · {reference}"),
        metadata,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    // And the one entry `sales` knows nothing about.
    if let Some(fee) = fee.filter(|f| f.minor() > 0) {
        let lines = entry_for_fee(fee, into, &accounts)
            .map_err(|e| ExecuteError::Rejected(PaymentsError::Unbalanced(e)))?;
        ledger::post_entry_in(
            &mut *conn,
            &fee_entry(id),
            at,
            &format!("Gateway fee · {reference}"),
            &lines,
            metadata,
        )
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;
    }

    Ok(committed)
}

/// What to write when the gateway's answer moves no money.
///
/// `None` when it did — which is the only case the caller has to think about,
/// and the reason this is split out rather than inlined.
fn unpaid(charged: &Charged, at: Timestamp) -> Option<Decision<PaymentEvent>> {
    match charged.status {
        Status::Failed => Some(Decision::one(PaymentEvent::Failed {
            why: charged
                .message
                .as_deref()
                .unwrap_or("refused")
                .chars()
                .take(500)
                .collect(),
            failed_at: at,
        })),
        Status::Voided => Some(Decision::one(PaymentEvent::Voided { voided_at: at })),
        // Still waiting on the customer or on a capture. Not an error — a
        // callback can legitimately arrive at either.
        Status::Initiated | Status::Authorized => Some(Decision::nothing()),
        Status::Paid | Status::Refunded => None,
    }
}

/// Records that the gateway refused. **Posts nothing**: no money moved.
pub async fn fail_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let why = why.chars().take(500).collect::<String>();
    try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            // **A requested payment can fail too**, and it is the one case
            // where nothing was ever sent to a gateway: the card it named was
            // forgotten before the worker got to it. Leaving it `requested`
            // would be a charge nobody ever collects and nobody can see is
            // stuck.
            if !loaded.aggregate.started && !loaded.aggregate.requested {
                return Err(PaymentsError::NotStarted(id.as_str().to_owned()));
            }
            if !matches!(loaded.aggregate.stage, Stage::Pending | Stage::Requested) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PaymentEvent::Failed {
                why: why.clone(),
                failed_at: at,
            }))
        },
    )
    .await
}

/// Records that it was cancelled before settling. Posts nothing.
pub async fn void_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if !loaded.aggregate.started {
                return Err(PaymentsError::NotStarted(id.as_str().to_owned()));
            }
            if loaded.aggregate.stage != crate::payment::Stage::Pending {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PaymentEvent::Voided { voided_at: at }))
        },
    )
    .await
}

/// Records money given back, and posts it.
///
/// The money comes **out of the account it went into** and back onto the
/// receivable, which is `sales::refund_in`. The fee is not given back: a
/// gateway keeps its cut on a refunded payment, which is why a refund costs a
/// business more than the sale earned it.
///
/// Decided inside the closure and posted from the event, for the reason
/// [`settle_in`] is.
pub async fn refund_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    reference: &str,
    amount: Money,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;

    let committed = try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            let state = &loaded.aggregate;
            let Some(refundable) = state.refundable() else {
                return Err(PaymentsError::NotCollectable {
                    id: id.as_str().to_owned(),
                    stage: state.stage.as_str(),
                });
            };
            if amount.minor() > refundable.minor() {
                return Err(PaymentsError::RefundTooLarge(amount));
            }
            let invoice = state
                .invoice
                .clone()
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?;

            Ok(Decision::one(PaymentEvent::Refunded {
                amount,
                invoice,
                out_of: accounts.holding(Settlement::of(&state.provider)),
                reference: reference.to_owned(),
                refunded_at: at,
            }))
        },
    )
    .await?;

    let Some(PaymentEvent::Refunded {
        amount,
        invoice,
        out_of,
        reference,
        ..
    }) = committed.events.first()
    else {
        return Ok(committed);
    };

    sales::refund_in(
        &mut *conn,
        invoice,
        &sales::Receipt {
            reference: reference.clone(),
            amount: *amount,
            received_on: at,
            into: out_of.clone(),
        },
        &format!("Refund · {reference}"),
        metadata,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    Ok(committed)
}

/// What a gateway sent, and what it says it covers.
///
/// Named for the act rather than the record: [`crate::Payout`] is the aggregate
/// this produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// The gateway's own id for the transfer.
    pub reference: String,
    pub provider: String,
    /// What arrived. The number on the bank statement.
    pub amount: Money,
    /// The bank account it landed in.
    pub into: AggregateId,
    /// The gateway payment ids it covers. **Empty is allowed** and means no
    /// reconciliation is possible — see [`crate::payout`].
    pub covers: Vec<String>,
}

/// Records a transfer from a gateway, and reconciles it.
///
/// The arithmetic is: what arrived, against what the covered payments say
/// should have. The difference is **booked rather than refused** — see
/// [`crate::posting::entry_for_payout`] for why.
///
/// Reads the covered payments from the **projection**, not by loading each
/// aggregate: that is a read, and reads are served by read models (L7).
pub async fn record_payout_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    payout: &Transfer,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<Committed<PayoutEvent>, ExecuteError<PaymentsError>> {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;
    let out_of = accounts.holding(Settlement::of(&payout.provider));

    // **What the covered payments say should have arrived.** Their amount less
    // the fee already booked against each — which is exactly what the clearing
    // account is holding for them.
    let mut expected = Money::from_minor(0, payout.amount.currency());
    for gateway_id in &payout.covers {
        let row = crate::payment(&mut *conn, gateway_id)
            .await
            .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?
            .filter(|row| row.stage == "settled" || row.stage == "refunded")
            .ok_or_else(|| ExecuteError::Rejected(PaymentsError::NotSettled(gateway_id.clone())))?;

        if row.amount.currency() != payout.amount.currency() {
            return Err(ExecuteError::Rejected(PaymentsError::PayoutCurrency {
                expected: row.amount.currency(),
                found: payout.amount.currency(),
            }));
        }
        let net = row.amount.minor() - row.fee.map_or(0, Money::minor);
        expected = Money::from_minor(expected.minor() + net, expected.currency());
    }
    // Nothing named is nothing to disagree with, so the difference is zero and
    // honest rather than invented.
    if payout.covers.is_empty() {
        expected = payout.amount;
    }

    let committed = try_execute::<Payout, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.received {
                // A retried request. The stored payout wins.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PayoutEvent::Received {
                provider: payout.provider.clone(),
                reference: payout.reference.clone(),
                amount: payout.amount,
                expected,
                covers: payout.covers.clone(),
                into: payout.into.clone(),
                out_of: out_of.clone(),
                received_on: at,
            }))
        },
    )
    .await?;

    let Some(PayoutEvent::Received {
        amount,
        expected,
        into,
        out_of,
        reference,
        ..
    }) = committed.events.first()
    else {
        return Ok(committed);
    };

    let lines = entry_for_payout(*amount, *expected, into, out_of, &accounts)
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Unbalanced(e)))?;
    ledger::post_entry_in(
        &mut *conn,
        &payout_entry(id),
        at,
        &format!("Payout · {reference}"),
        &lines,
        metadata,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    Ok(committed)
}

/// A journal entry id for a payout, derived from it.
#[expect(
    clippy::expect_used,
    reason = "a derived id from an id that already parsed cannot fail to parse"
)]
fn payout_entry(payout: &AggregateId) -> AggregateId {
    AggregateId::new(format!("po-{}", payout.as_str())).expect("a prefixed aggregate id is one")
}

/// A journal entry id for a fee, derived from the payment.
///
/// Prefixed so it can never land on an entry somebody posted by hand, which
/// would be absorbed silently — posting an existing entry id is a no-op.
#[expect(
    clippy::expect_used,
    reason = "a derived id from an id that already parsed cannot fail to parse"
)]
fn fee_entry(payment: &AggregateId) -> AggregateId {
    AggregateId::new(format!("pf-{}", payment.as_str())).expect("a prefixed aggregate id is one")
}

// ---------------------------------------------------------------------------
// Saved cards
// ---------------------------------------------------------------------------

type CardOutcome = Result<Committed<CardEvent>, ExecuteError<PaymentsError>>;

/// What a customer left behind, minus the part that charges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedCard {
    pub customer: AggregateId,
    pub provider: String,
    pub brand: String,
    /// Exactly four digits, and the only digits of the card this system keeps.
    pub last4: String,
    pub expiry_month: i16,
    pub expiry_year: i16,
}

impl SavedCard {
    /// **Checked here rather than only at the edge**, because a card that
    /// expires in month 13 is a row somebody has to explain later, and this is
    /// the boundary every caller crosses.
    fn check(&self, token: &str) -> Result<(), PaymentsError> {
        if !crate::card::SAVES_CARDS.contains(&self.provider.as_str()) {
            return Err(PaymentsError::NoSavedCards(self.provider.clone()));
        }
        if token.trim().is_empty() {
            return Err(PaymentsError::NotACard("it has no token".to_owned()));
        }
        if self.last4.len() != 4 || !self.last4.chars().all(|c| c.is_ascii_digit()) {
            return Err(PaymentsError::NotACard(format!(
                "{} is not four digits",
                self.last4
            )));
        }
        if !(1..=12).contains(&self.expiry_month) {
            return Err(PaymentsError::NotACard(format!(
                "{} is not a month",
                self.expiry_month
            )));
        }
        // Not "in the future": a card that expired last week is still the card
        // the customer has, and telling them so is the gateway's job, in the
        // gateway's words, at the moment it matters.
        if !(2000..=2100).contains(&self.expiry_year) {
            return Err(PaymentsError::NotACard(format!(
                "{} is not a year",
                self.expiry_year
            )));
        }
        Ok(())
    }
}

/// Keeps a customer's card for next time.
///
/// # Two stores, one transaction
///
/// The display facts go in the log; the **token goes in `module_secret`**,
/// sealed, and never into an event — see [`crate::card`] for why. Both happen
/// on the connection handed in, so a caller that hands in a transaction gets
/// one or neither. A card recorded whose token was not sealed is a card that
/// looks chargeable and is not.
///
/// # What this trusts the caller for
///
/// The brand and the last four digits are **the caller's word**, because the
/// token was minted in a browser this process never saw. They are display, and
/// nothing decides anything from them — a wrong `last4` mislabels a row for
/// whoever entered it. The token is what charges, and the gateway owns that.
pub async fn save_card_in(
    conn: &mut sqlx::PgConnection,
    sealing: &erp_eventlog::SealingKey,
    id: &AggregateId,
    card: &SavedCard,
    token: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> CardOutcome {
    card.check(token).map_err(ExecuteError::Rejected)?;

    let committed = try_execute::<Card, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.forgotten {
                return Err(PaymentsError::CardForgotten(id.as_str().to_owned()));
            }
            if loaded.aggregate.saved {
                // A retried request. The stored card wins, and the token
                // already beside it is the one that was sealed with it.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(CardEvent::Saved {
                customer: card.customer.clone(),
                provider: card.provider.clone(),
                brand: card.brand.clone(),
                last4: card.last4.clone(),
                expiry_month: card.expiry_month,
                expiry_year: card.expiry_year,
                saved_at: at,
            }))
        },
    )
    .await?;

    if committed.events.is_empty() {
        return Ok(committed);
    }
    erp_eventlog::secrets::put(&mut *conn, sealing, &token_key(id), token.as_bytes())
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Secret(e)))?;
    Ok(committed)
}

/// Removes a card, and **deletes the thing that could charge it**.
///
/// The event records that it happened, because a customer asking for their card
/// to be removed is history somebody may have to answer for. The token is a
/// delete, which is the half an append-only log cannot do and the reason it was
/// never in one.
pub async fn forget_card_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> CardOutcome {
    let committed = try_execute::<Card, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if !loaded.aggregate.saved {
                return Err(PaymentsError::NoSuchCard(id.as_str().to_owned()));
            }
            if loaded.aggregate.forgotten {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(CardEvent::Forgotten { forgotten_at: at }))
        },
    )
    .await?;

    // **Unconditionally**, and not only when this call is the one that wrote
    // the event. A retry whose first attempt committed and then failed here
    // would otherwise leave the token behind for ever, which is precisely the
    // outcome the customer asked against.
    erp_eventlog::secrets::forget(&mut *conn, &token_key(id))
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Secret(e)))?;
    Ok(committed)
}

/// What somebody wants taken off a saved card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub card: AggregateId,
    pub provider: String,
    pub invoice: AggregateId,
    pub amount: Money,
    /// Where the gateway sends the customer if it decides it needs them.
    pub callback_url: String,
}

/// Records that a saved card should be charged. **Charges nothing.**
///
/// # Why this does not talk to the gateway
///
/// Because a request handler is the wrong place for an outbound call to a third
/// party, and this system already decided that: ZATCA submissions and the
/// settlement sweep are worker jobs for the same reason. A handler that waits
/// on a gateway holds a database connection for as long as somebody else's
/// server feels like taking, and a gateway having a slow morning becomes this
/// tenant running out of connections.
///
/// So this writes the intent and answers, and [`crate::charge_requested`] is
/// what sends it. What the caller gets back is the payment's id — which is the
/// id the gateway will use too, because it is passed as Moyasar's `given_id`.
///
/// # The card is not checked here
///
/// Deliberately. The token is the only thing that actually decides whether this
/// card can be charged, it lives in another store, and it can be deleted
/// between this call and the charge whatever is checked now. So the authority
/// is [`crate::charge_requested`], which fails the payment with a reason when
/// the token has gone. The route ahead of this reads `proj_payments.card` for a
/// friendly refusal on the common mistake, which is a typo rather than a race.
pub async fn request_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    collection: &Collection,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.requested || loaded.aggregate.started {
                // A retried request. The stored one wins — and it matters more
                // here than anywhere else in this module, because the
                // alternative is charging somebody twice.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PaymentEvent::Requested {
                card: collection.card.clone(),
                provider: collection.provider.clone(),
                invoice: collection.invoice.clone(),
                amount: collection.amount,
                callback_url: collection.callback_url.clone(),
                requested_at: at,
            }))
        },
    )
    .await
}
