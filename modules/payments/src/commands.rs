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
            if !loaded.aggregate.started {
                return Err(PaymentsError::NotStarted(id.as_str().to_owned()));
            }
            if loaded.aggregate.stage != crate::payment::Stage::Pending {
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
