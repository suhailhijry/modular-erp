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

use crate::awaiting::{Awaiting, AwaitingEvent};
use crate::card::{Card, CardEvent, token_key};
use crate::payment::{Collects, Payment, PaymentEvent, RefundRequest, Stage};
use crate::payout::{Payout, PayoutEvent};
use crate::posting::{
    PostingAccounts, Settlement, entry_for_fee, entry_for_forfeit, entry_for_payout,
};

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
    /// Keeping a deposit while a refund of it is still being carried out. The
    /// two would race for the same money; the refund finishes first.
    #[error("payment {0} has a refund in flight; it cannot be kept until that is settled")]
    RefundAwaited(String),
    /// A refund the gateway already refused, asked for again under the same
    /// reference. The answer does not change by asking.
    #[error("the gateway refused refund {reference} of {payment}: {why}")]
    RefundRefused {
        payment: String,
        reference: String,
        why: String,
    },
    /// Keeping an invoice payment. **Refused**: there is nothing to keep,
    /// because the supply it paid for already happened and was already
    /// invoiced. Retention is a deposit's question.
    #[error("payment {0} is against an invoice; there is no deposit to keep")]
    NotADeposit(String),
    #[error("there is nothing left of {0} to keep")]
    NothingToRetain(String),
    /// **A second deposit against something that already has one in flight.**
    /// Refused rather than started: the customer who opened the payment page
    /// twice would otherwise have two charges, and paying both is paying twice.
    /// The payment named is the one that already exists, so a caller can offer
    /// it instead.
    #[error("{against} already has deposit {payment} in flight")]
    AlreadyAwaited { against: String, payment: String },
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
    /// **A refund or a deposit `sales` will not let this member ask for** —
    /// over the tenant's document limit. Carried as `sales` said it, rather than as
    /// [`Self::Sales`]' flattened sentence, because the sentence and the 403
    /// are both `sales`' to give (`SalesError::refuses_the_caller`).
    #[error(transparent)]
    Refused(sales::SalesError),
}

type Outcome = Result<Committed<PaymentEvent>, ExecuteError<PaymentsError>>;

/// What a charge at a gateway was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Where the customer goes to pay, when the gateway named somewhere.
    pub pay_at: Option<String>,
    /// `moyasar`, `tabby`, `tamara`.
    pub provider: String,
    /// **The gateway's own id.** What every callback names.
    pub gateway_id: String,
    /// An invoice, or a deposit taken before there was one. See [`Collects`].
    pub collects: Collects,
    pub amount: Money,
}

/// Records that a charge was created at a gateway.
///
/// **Written before the customer is sent anywhere.** An attempt this system did
/// not write down is an attempt no callback can be matched to, and the customer
/// will still have been charged.
///
/// `authority` is whoever started it. A deposit it starts is judged against the
/// document limit here, on what its prepayment invoice will come to, because
/// that invoice is raised when the gateway settles and by then the customer
/// has paid.
pub async fn start_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    attempt: &Attempt,
    at: Timestamp,
    metadata: &Metadata,
    authority: sales::Authority,
) -> Outcome {
    let committed = try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.started {
                // A retried request. The stored attempt wins.
                return Ok(Decision::nothing());
            }
            // **What a payment collects was settled when it was asked for.**
            // A pass that comes along later — charging a saved card, or finding
            // that the customer has paid — supplies the gateway's id and
            // nothing else. Taking the target from the caller here would let
            // the second half of a two-step payment quietly point somewhere
            // the first half never did.
            let (invoice, advance) = loaded
                .aggregate
                .collects
                .clone()
                .unwrap_or_else(|| attempt.collects.clone())
                .split();
            // **And the amount, for the same reason.** A pass that found the
            // customer's own charge at the gateway hands over what the gateway
            // says was created; if that replaced what was *asked for*, the
            // settlement check below would be comparing the gateway to itself
            // and a deposit paid short would settle as if it were whole. What
            // was asked for is what this payment is for.
            let amount = loaded.aggregate.amount.unwrap_or(attempt.amount);
            Ok(Decision::one(PaymentEvent::Started {
                provider: attempt.provider.clone(),
                gateway_id: attempt.gateway_id.clone(),
                invoice,
                advance,
                amount,
                pay_at: attempt.pay_at.clone(),
                started_at: at,
            }))
        },
    )
    .await?;

    // Only when something was written, so a retry answers as the first did.
    if let Some(PaymentEvent::Started {
        advance: Some(advance),
        amount,
        ..
    }) = committed.events.first()
    {
        may_bill(&mut *conn, advance, *amount, authority, metadata).await?;
    }
    Ok(committed)
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
            let collects = state
                .collects
                .as_ref()
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?;

            Ok(Decision::one(PaymentEvent::Settled {
                amount: charged.amount,
                fee: charged.fee,
                // **A deposit's invoice is derived, not stored**, so it is the
                // same id here, at the refund months later, and on a replay.
                invoice: collects.invoice(id),
                advance: collects.advance().cloned(),
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
        advance,
        into,
        reference,
        ..
    }) = committed.events.first()
    else {
        return Ok(committed);
    };

    // **A deposit becomes a document the moment the money is real.** Receiving
    // consideration is itself a tax point — the earliest of supply, invoice and
    // payment is when VAT falls due — so the prepayment invoice is raised here,
    // in the same transaction, and everything after this line treats a deposit
    // as the ordinary invoice payment it now is.
    if let Some(advance) = advance {
        bill_the_deposit(&mut *conn, invoice, advance, *amount, at, metadata).await?;
    }

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

/// **Raises the prepayment invoice a deposit is billed under.**
///
/// # Why a document rather than a liability
///
/// Because the tax is due now. Receiving consideration is a tax point in its
/// own right, so a deposit that sat in a liability with no document would leave
/// the output tax undeclared in the period it fell due and declared in whatever
/// period the booking was finally served — or never, if the customer did not
/// come back. The authority wants the document within fifteen days of that
/// month's end, and the only way it reaches a VAT return is by being a `sales`
/// invoice, because that is what the return is built from.
///
/// # Why the net is carried rather than divided out
///
/// A deposit is a fraction of something already priced, and the price had a net
/// and a gross. Working the net back out of the gross does not always land —
/// at 15% there is no net whose tax comes to exactly 10.00 — so the net travels
/// with the deposit from the moment it is worked out, and the tax runs forwards
/// from it the way it does on every other invoice.
///
/// **What it does not do is guess.** If the invoice does not come to what the
/// customer was actually charged, the deposit was computed against a rate this
/// tenant no longer has, and that stops rather than posting a document for a
/// different number (L6).
async fn bill_the_deposit(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    advance: &crate::Advance,
    charged: Money,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PaymentsError>> {
    let numbered = sales::issue_in(
        &mut *conn,
        invoice,
        &sales::Draft {
            customer: {
                let mut customer = sales::Customer::new(&advance.buyer.name);
                customer.vat_number.clone_from(&advance.buyer.vat_number);
                customer
            },
            issued_on: at,
            due_on: None,
            currency: advance.net.currency(),
            lines: vec![sales::DraftLine {
                allowances: Vec::new(),
                description: format!("Deposit · {}", advance.against),
                net: advance.net,
                category: sales::VatCategory::Standard,
                // **A deposit is money taken before the supply**, and nothing
                // leaves a shelf when it is taken. The goods are depleted by
                // the final invoice, which is an ordinary one and carries the
                // product lines.
                product: None,
                quantity: None,
                serials: Vec::new(),
                lot: None,
            }],
            discounts: Vec::new(),
            // **386, not 388.** The document says it bills for money taken
            // before the supply, which is a different tax point and a different
            // thing to report.
            prepayment: true,
            prepaid: None,
            note: String::new(),
        },
        &format!("Deposit · {}", advance.against),
        metadata,
        // **Nobody issues this by hand.** The gateway has settled money the
        // customer already paid; the document follows from that, and refusing
        // it would leave the money undeclared rather than unpaid. A member who
        // started the charge was judged on this invoice when they asked, by
        // `request_in` or `start_in`.
        sales::Authority::System,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    // A retry: the invoice was raised on the attempt that came before this one.
    if numbered.committed.events.is_empty() {
        return Ok(());
    }

    let Some(sales::InvoiceEvent::Issued { totals, .. }) = numbered.committed.events.first() else {
        return Ok(());
    };
    if totals.gross != charged {
        return Err(ExecuteError::Rejected(PaymentsError::WrongAmount {
            expected: totals.gross,
            found: charged,
        }));
    }
    Ok(())
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
    let committed = try_execute::<Payment, _, PaymentsError>(
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
    .await?;
    release_awaiting(&mut *conn, &committed, id, at, metadata).await?;
    Ok(committed)
}

/// Records that it was cancelled before settling. Posts nothing.
pub async fn void_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let committed = try_execute::<Payment, _, PaymentsError>(
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
    .await?;
    release_awaiting(&mut *conn, &committed, id, at, metadata).await?;
    Ok(committed)
}

/// **A deposit that will never arrive frees its booking for another try.**
///
/// Only when this call is the one that ended the payment, and only for a
/// deposit: an invoice payment claims nothing. Same transaction as the ending,
/// so a booking is never left with a dead claim on it.
async fn release_awaiting(
    conn: &mut sqlx::PgConnection,
    committed: &Committed<PaymentEvent>,
    payment: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PaymentsError>> {
    if committed.events.is_empty() {
        return Ok(());
    }
    // The aggregate was just loaded by the caller's `try_execute`, and what it
    // collects is not on the ending event, so read it back (L7 permits it: this
    // is still the command).
    let loaded = erp_eventlog::load::<Payment>(&mut *conn, payment, crate::upcasters()).await?;
    let Some(against) = loaded
        .aggregate
        .collects
        .as_ref()
        .and_then(Collects::advance)
        .map(|advance| advance.against.clone())
    else {
        return Ok(());
    };
    try_execute::<Awaiting, _, PaymentsError>(
        &mut *conn,
        &against,
        crate::upcasters(),
        metadata,
        |held| {
            if held.aggregate.live.as_ref() != Some(payment) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(AwaitingEvent::Released {
                payment: payment.clone(),
                at,
            }))
        },
    )
    .await?;
    Ok(())
}

/// Records money the gateway has given back, and posts it.
///
/// **The second half of a refund.** The first is [`request_refund_in`], which
/// records that somebody asked; `crate::refund_requested` carries the request
/// to the gateway and calls this with what the gateway confirmed. Nothing else
/// should: a refund recorded here that the gateway never made is a set of books
/// saying money went back when it did not — which is exactly what the first
/// version of the refund route produced.
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
    reason: &str,
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
            // **Before anything else.** A fully refunded payment is no longer
            // collectable, so a retry of the refund that finished it would be
            // refused rather than answered — and the client that timed out on
            // the first one has no way to tell that from a real failure. It
            // would also be the one path that could issue a **second credit
            // note**, which is a statutory document that must not exist twice.
            if state.has_refund(reference) {
                return Ok(Decision::nothing());
            }
            // What is left, plus what this reference reserved for itself when
            // it was requested — the worker completing an awaited refund must
            // not be refused for the amount it is completing.
            let Some(refundable) = state.refundable_for(reference) else {
                return Err(PaymentsError::NotCollectable {
                    id: id.as_str().to_owned(),
                    stage: state.stage.as_str(),
                });
            };
            if amount.minor() > refundable.minor() {
                return Err(PaymentsError::RefundTooLarge(amount));
            }
            let invoice = state
                .collects
                .as_ref()
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?
                .invoice(id);

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

    // **One path, because a deposit has an invoice too.** Its prepayment
    // invoice is as much an invoice as any other, which is the whole point of
    // raising one: giving a deposit back is a credit note and the money, exactly
    // as it is for a sale.
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
        // **Judged when it was asked for**, by `request_refund_in`. This
        // records what the gateway already did; refusing it now would leave
        // the books saying the money is still here.
        sales::Authority::System,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    // **And the document the money implies.** In the same transaction, because
    // a refund recorded without its credit note is a tax invoice overstating
    // what was sold, and nobody would find it.
    credit_the_invoice(
        &mut *conn, invoice, reference, *amount, reason, at, metadata,
    )
    .await?;

    Ok(committed)
}

/// **Asks for money to go back.** Records the intent and nothing else.
///
/// The worker carries it to the gateway — see `crate::refund_requested` — and
/// [`refund_in`] is written from what the gateway confirms. So this posts
/// nothing and issues no document; what it does is reserve the amount, so a
/// second request cannot ask for the same riyals and nothing can keep them
/// while the gateway is being asked.
///
/// Idempotent on the reference: the same one twice is a retry, whether the
/// first is still awaited, already refunded, or already refused — the last
/// answers with the gateway's refusal rather than trying again, because the
/// gateway's answer does not change by asking.
///
/// **This is where a member's gateway refund is judged**, since by the time
/// [`refund_in`] records it the money has gone. `authority` is whoever asked;
/// `sales::may_refund` judges the money and the credit note it will leave
/// owing — against the tenant's document limit, and for
/// `sales::APPROVE_CREDIT_NOTE` when a credit note follows — the way a refund
/// recorded on the spot is judged.
#[expect(
    clippy::too_many_arguments,
    reason = "each is a fact about the request, and the last is who made it"
)]
pub async fn request_refund_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    reference: &str,
    amount: Money,
    reason: &str,
    at: Timestamp,
    metadata: &Metadata,
    authority: sales::Authority,
) -> Outcome {
    if !amount.is_positive() {
        return Err(ExecuteError::Rejected(PaymentsError::RefundTooLarge(
            amount,
        )));
    }
    let reason = reason.trim().to_owned();
    let committed = try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if state.has_refund(reference) || state.refund_awaited(reference).is_some() {
                return Ok(Decision::nothing());
            }
            if state.refund_refused(reference) {
                return Err(PaymentsError::RefundRefused {
                    payment: id.as_str().to_owned(),
                    reference: reference.to_owned(),
                    why: "already refused".to_owned(),
                });
            }
            let Some(refundable) = state.refundable() else {
                return Err(PaymentsError::NotCollectable {
                    id: id.as_str().to_owned(),
                    stage: state.stage.as_str(),
                });
            };
            if amount.minor() > refundable.minor() {
                return Err(PaymentsError::RefundTooLarge(amount));
            }
            Ok(Decision::one(PaymentEvent::RefundRequested {
                reference: reference.to_owned(),
                amount,
                reason: reason.clone(),
                requested_at: at,
            }))
        },
    )
    .await?;

    // **Judged only when the request is new**, so a retry answers the way the
    // first one did. A refusal here is an error from this function, and the
    // caller's transaction takes the request back out with it.
    if !committed.events.is_empty() {
        let payment = erp_eventlog::load::<Payment>(&mut *conn, id, crate::upcasters())
            .await?
            .aggregate;
        if let Some(collects) = &payment.collects {
            sales::may_refund(conn, &collects.invoice(id), amount, authority, metadata)
                .await
                .map_err(refused)?;
        }
    }
    Ok(committed)
}

/// **What `sales` refused a member**, as `sales` said it — see
/// [`PaymentsError::Refused`].
fn refused(e: ExecuteError<sales::SalesError>) -> ExecuteError<PaymentsError> {
    match e {
        ExecuteError::Rejected(refused) => ExecuteError::Rejected(PaymentsError::Refused(refused)),
        ExecuteError::Load(e) => ExecuteError::Load(e),
        ExecuteError::Append(e) => ExecuteError::Append(e),
        ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
        ExecuteError::Database(e) => ExecuteError::Database(e),
        ExecuteError::Contended { stream, attempts } => {
            ExecuteError::Contended { stream, attempts }
        }
        ExecuteError::AlreadyExists { stream } => ExecuteError::AlreadyExists { stream },
    }
}

/// **A member asking for a deposit is issuing its invoice**, so they are
/// judged against the document limit now — see `sales::may_issue`. The
/// prepayment invoice itself is raised by [`settle_in`] once the customer has
/// paid, with nobody acting, and must not be refused then.
async fn may_bill(
    conn: &mut sqlx::PgConnection,
    advance: &crate::Advance,
    amount: Money,
    authority: sales::Authority,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PaymentsError>> {
    sales::may_issue(conn, advance.net, amount, authority, metadata)
        .await
        .map_err(refused)
}

/// Records that the gateway would not give it back. Posts nothing; the money
/// never moved.
pub async fn refuse_refund_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    reference: &str,
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
            let state = &loaded.aggregate;
            if state.refund_awaited(reference).is_none() {
                // Already refunded, already refused, or never asked: nothing to
                // record either way.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(PaymentEvent::RefundRefused {
                reference: reference.to_owned(),
                why: why.clone(),
                refused_at: at,
            }))
        },
    )
    .await
}

/// What the gateway was asked for and has not yet answered, for
/// `crate::refund_requested`.
#[must_use]
pub fn awaited_refunds(payment: &Payment) -> &[RefundRequest] {
    &payment.awaited_refunds
}

/// Issues the credit note a refund owes, **if the invoice is now clear**.
///
/// # Why ZATCA cares
///
/// A tax invoice is a statement about a supply, and giving the money back
/// changes the supply. The Kingdom's answer is not to amend the invoice — it
/// was issued, the customer holds a copy, and it was cleared — but to issue a
/// **credit note**, which is a document in its own right with its own number
/// and its own tax point. Everything downstream of that already exists:
/// `tax_sa::documents` builds one from `sales.invoice.cancelled`, the VAT
/// return nets it, and the signing and submission jobs carry it. What was
/// missing was anybody asking.
///
/// # Why it asks `sales` rather than deciding
///
/// Whether the invoice is clear is `sales`' own question and it can only be
/// answered from the invoice's history — a gateway payment does not know
/// whether it was the only one. So `sales::credit_what_is_clear` is what
/// decides, and this only carries the failure into this module's error type.
/// The alternative was for `payments` to load `sales`' aggregate (L7) or to
/// read another projection group that has not seen the refund committed a line
/// ago (L3), and both are worse than one call on a path that runs when
/// somebody hands money back.
///
/// # What it credits
///
/// A partial refund of a **single-band** invoice issues a partial credit note
/// for exactly what went back: `sales::credit_what_is_clear` finds the net
/// that, taxed the way the invoice was, comes to the refund (§44), and a
/// fully-refunded invoice ends credited by the sum of those parts. A refund
/// that clears the invoice in one shot issues a whole-invoice credit note
/// instead. A **multi-band** invoice is the one case still deferred: how an
/// arbitrary refund divides across a standard-rated line and a zero-rated one
/// is not something this system may guess, so it gets no document and the log
/// says which invoice is, until then, overstated by the refund.
async fn credit_the_invoice(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    reference: &str,
    refunded: Money,
    reason: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PaymentsError>> {
    sales::credit_what_is_clear(
        &mut *conn,
        invoice,
        reference,
        refunded,
        reason,
        at,
        metadata,
        // The refund's own, and for the same reason: the gateway has
        // already handed the money back, and this is the document it implies.
        sales::Authority::System,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))
}

/// **Keeps a deposit the customer did not come back for.**
///
/// It keeps everything that has not already been given back, and there is no
/// amount to pass: a policy that returns half is a refund of half followed by
/// this, which is two facts recorded as two facts rather than one number that
/// means both.
///
/// # The tax was settled when the money arrived
///
/// This raises no document and declares nothing. A deposit is billed by a
/// prepayment invoice at settlement, so the VAT on it was declared in the period
/// the customer paid — which is when it fell due — and keeping the money changes
/// none of that. **It is deliberately not reversed either**: the authority's own
/// guidance is to reverse a prepayment only when it actually goes back to the
/// buyer, and this is the case where it does not.
///
/// # What the setting decides
///
/// Whether the money the business kept is a **sale** or something else. A
/// tenant whose adviser says a forfeited deposit is compensation rather than
/// consideration books it separately, so their profit and loss can tell service
/// income from deposits nobody came back for. See [`crate::Retention`] for why
/// the default is a sale, and for the limit of what this can express.
pub async fn retain_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;
    let retention = crate::Retention::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;

    let committed = try_execute::<Payment, _, PaymentsError>(
        &mut *conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if state.stage == Stage::Retained {
                // A retry. The money is already the business's.
                return Ok(Decision::nothing());
            }
            let Some(advance) = state.collects.as_ref().and_then(Collects::advance) else {
                return Err(if state.collects.is_some() {
                    PaymentsError::NotADeposit(id.as_str().to_owned())
                } else {
                    PaymentsError::NotStarted(id.as_str().to_owned())
                });
            };
            if !state.awaited_refunds.is_empty() {
                return Err(PaymentsError::RefundAwaited(id.as_str().to_owned()));
            }
            let Some(left) = state.refundable().filter(|m| m.is_positive()) else {
                return Err(PaymentsError::NothingToRetain(id.as_str().to_owned()));
            };
            // **The net of what is kept, at the rate it was billed at.** The
            // deposit carried its own net, and the prepayment invoice was
            // raised for exactly that, so the share of the kept amount that was
            // never tax is `kept × net / gross` — the deposit's own figures,
            // not the standard rate on the day. Between settling and keeping,
            // a rate can change; the return the tax was declared on cannot.
            let gross = state
                .amount
                .ok_or_else(|| PaymentsError::NotStarted(id.as_str().to_owned()))?;
            let net = left
                .apportioned(advance.net.minor(), gross.minor())
                .map_err(|e| PaymentsError::Unbalanced(ledger::Unbalanced::Money(e)))?;

            Ok(Decision::one(PaymentEvent::Retained {
                amount: left,
                net: Some(net),
                supply: retention.supply,
                advance_for: advance.against.clone(),
                retained_at: at,
            }))
        },
    )
    .await?;

    let Some(PaymentEvent::Retained {
        amount,
        net,
        supply,
        ..
    }) = committed.events.first()
    else {
        return Ok(committed);
    };

    // **A sale needs nothing doing.** The prepayment invoice already recognised
    // it and already declared the tax; keeping the money is the supply
    // happening, not a new fact about it.
    if *supply {
        return Ok(committed);
    }

    // **Not a sale, so it moves out of revenue** — and only the revenue does.
    // The tax stays where it was declared: reclaiming it would be reversing a
    // prepayment the buyer never got back, which is the one thing the
    // authority's guidance says not to do. The net is the event's, worked out
    // above from the deposit's own figures.
    let net = net.unwrap_or(*amount);
    // Out of the account the prepayment invoice credited, which is `sales`' to
    // name — the same reason `settle_in` asks it where the receivable is.
    let revenue = sales::PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?
        .revenue;
    let lines = entry_for_forfeit(net, &revenue, &accounts)
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Unbalanced(e)))?;
    ledger::post_entry_in(
        &mut *conn,
        &retention_entry(id),
        at,
        &format!("Deposit forfeited · {id}"),
        &lines,
        metadata,
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    Ok(committed)
}

/// The entry a retention posts under.
#[expect(
    clippy::expect_used,
    reason = "a prefix on an id that is already valid is valid"
)]
fn retention_entry(payment: &AggregateId) -> AggregateId {
    AggregateId::new(format!("pk-{}", payment.as_str())).expect("a prefixed aggregate id is one")
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
    /// The saved card to charge, or `None` when the customer pays it
    /// themselves in their own browser.
    pub card: Option<AggregateId>,
    pub provider: String,
    /// An invoice, or a deposit taken before there was one. See [`Collects`] —
    /// a saved card is exactly how a booking deposit gets charged.
    pub collects: Collects,
    pub amount: Money,
    /// Where the gateway sends the customer if it decides it needs them.
    pub callback_url: String,
    /// **Everything a provider that hosts its own checkout is told.** `Some`
    /// makes this a checkout the worker opens; `None` is a card, or a charge
    /// the customer's browser creates itself. See [`crate::Checkout`].
    pub checkout: Option<crate::Checkout>,
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
///
/// # Who asked
///
/// `authority` is a member charging a card, or `System` for a customer's own
/// deposit. A member's deposit is judged against the document limit here, for
/// the reason [`start_in`] gives.
pub async fn request_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    collection: &Collection,
    at: Timestamp,
    metadata: &Metadata,
    authority: sales::Authority,
) -> Outcome {
    let committed = try_execute::<Payment, _, PaymentsError>(
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
            let (invoice, advance) = collection.collects.split();
            Ok(Decision::one(PaymentEvent::Requested {
                card: collection.card.clone(),
                provider: collection.provider.clone(),
                invoice,
                advance,
                amount: collection.amount,
                callback_url: collection.callback_url.clone(),
                checkout: collection.checkout.clone().map(Box::new),
                requested_at: at,
            }))
        },
    )
    .await?;

    // **One deposit in flight per thing it is against.** Checked against the
    // log, in this transaction, so two tabs a millisecond apart cannot both
    // get a charge — see `crate::awaiting`. A retry (nothing written above)
    // already holds its claim and is not asked again.
    if let (false, Collects::Advance(advance)) = (committed.events.is_empty(), &collection.collects)
    {
        may_bill(&mut *conn, advance, collection.amount, authority, metadata).await?;
        try_execute::<Awaiting, _, PaymentsError>(
            &mut *conn,
            &advance.against,
            crate::upcasters(),
            metadata,
            |held| {
                if let Some(other) = held.aggregate.blocks(id) {
                    return Err(PaymentsError::AlreadyAwaited {
                        against: advance.against.as_str().to_owned(),
                        payment: other.as_str().to_owned(),
                    });
                }
                if held.aggregate.live.as_ref() == Some(id) {
                    return Ok(Decision::nothing());
                }
                Ok(Decision::one(AwaitingEvent::Claimed {
                    payment: id.clone(),
                    at,
                }))
            },
        )
        .await?;
    }

    Ok(committed)
}
