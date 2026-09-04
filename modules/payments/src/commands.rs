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
use crate::payment::{Collects, Payment, PaymentEvent, Stage};
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
    /// Keeping an invoice payment. **Refused**: there is nothing to keep,
    /// because the supply it paid for already happened and was already
    /// invoiced. Retention is a deposit's question.
    #[error("payment {0} is against an invoice; there is no deposit to keep")]
    NotADeposit(String),
    #[error("there is nothing left of {0} to keep")]
    NothingToRetain(String),
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
    /// An invoice, or a deposit taken before there was one. See [`Collects`].
    pub collects: Collects,
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
            let (invoice, advance) = attempt.collects.split();
            Ok(Decision::one(PaymentEvent::Started {
                provider: attempt.provider.clone(),
                gateway_id: attempt.gateway_id.clone(),
                invoice,
                advance,
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
            }],
            discounts: Vec::new(),
            // **386, not 388.** The document says it bills for money taken
            // before the supply, which is a different tax point and a different
            // thing to report.
            prepayment: true,
            note: String::new(),
        },
        &format!("Deposit · {}", advance.against),
        metadata,
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
    )
    .await
    .map_err(|e| ExecuteError::Rejected(PaymentsError::Sales(e.to_string())))?;

    // **And the document the money implies.** In the same transaction, because
    // a refund recorded without its credit note is a tax invoice overstating
    // what was sold, and nobody would find it.
    credit_the_invoice(&mut *conn, invoice, reference, reason, at, metadata).await?;

    Ok(committed)
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
/// # What it does not do
///
/// **A partial refund gets no document**, and that is the deferral rather than
/// a decision taken here: a credit note for part of an invoice carries bands of
/// its own, and how a refund of an arbitrary amount divides across a
/// standard-rated line and a zero-rated one is not something this system may
/// guess. `sales` has recorded that as an open item since Phase 3d. Until it
/// lands, a partly-refunded invoice is a tax invoice this system knows is
/// overstated — named in the plan rather than silently looking like success.
async fn credit_the_invoice(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    reference: &str,
    reason: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PaymentsError>> {
    sales::credit_what_is_clear(&mut *conn, invoice, reference, reason, at, metadata)
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
            let Some(left) = state.refundable().filter(|m| m.is_positive()) else {
                return Err(PaymentsError::NothingToRetain(id.as_str().to_owned()));
            };

            Ok(Decision::one(PaymentEvent::Retained {
                amount: left,
                supply: retention.supply,
                advance_for: advance.against.clone(),
                retained_at: at,
            }))
        },
    )
    .await?;

    let Some(PaymentEvent::Retained { amount, supply, .. }) = committed.events.first() else {
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
    // authority's guidance says not to do.
    let net = amount
        .checked_sub(tax_on_kept(&mut *conn, *amount).await?)
        .map_err(|e| {
            ExecuteError::Rejected(PaymentsError::Unbalanced(ledger::Unbalanced::Money(e)))
        })?;
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

/// The tax inside a kept deposit, at the rate its prepayment invoice carried.
///
/// Resolved rather than remembered because the reclassification is a fact about
/// the money now, and the only thing it needs is how much of it was never
/// revenue in the first place.
async fn tax_on_kept(
    conn: &mut sqlx::PgConnection,
    gross: Money,
) -> Result<Money, ExecuteError<PaymentsError>> {
    let rates = ledger::Rates::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PaymentsError::Config(e)))?;
    let bp = rates.of(sales::VatCategory::Standard);
    let net = gross
        .apportioned(10_000, i64::from(10_000 + bp))
        .map_err(|e| {
            ExecuteError::Rejected(PaymentsError::Unbalanced(ledger::Unbalanced::Money(e)))
        })?;
    gross.checked_sub(net).map_err(|e| {
        ExecuteError::Rejected(PaymentsError::Unbalanced(ledger::Unbalanced::Money(e)))
    })
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
    pub card: AggregateId,
    pub provider: String,
    /// An invoice, or a deposit taken before there was one. See [`Collects`] —
    /// a saved card is exactly how a booking deposit gets charged.
    pub collects: Collects,
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
            let (invoice, advance) = collection.collects.split();
            Ok(Decision::one(PaymentEvent::Requested {
                card: collection.card.clone(),
                provider: collection.provider.clone(),
                invoice,
                advance,
                amount: collection.amount,
                callback_url: collection.callback_url.clone(),
                requested_at: at,
            }))
        },
    )
    .await
}
