//! What a caller can ask sales to do.
//!
//! # Both books, one transaction
//!
//! Issuing an invoice writes a `sales.invoice.issued` event *and* a journal
//! entry, and they commit together. That is the whole reason these commands do
//! not use [`TenantDb::execute`], which runs exactly one aggregate: an invoice
//! that exists without its accounting entry is a state nobody could explain and
//! nothing would clean up, so it is made unreachable instead of monitored for.
//!
//! The alternative — promise the posting through the outbox and deliver it
//! afterwards — is what the outbox is for, and it is the wrong tool here.
//! At-least-once delivery to an *external* system is unavoidable; between two
//! aggregates in the same database it is a choice, and choosing it would trade a
//! guarantee for a dead-letter queue. The outbox earns its place on the first
//! effect that leaves this process: emailing the invoice, or clearing it with
//! ZATCA.

use erp_eventlog::{
    Committed, Decision, ExecuteError, MAX_ATTEMPTS, Metadata, try_create, try_execute,
};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, CurrencyCode, Money, StreamId, Timestamp};
use ledger::LedgerError;

use crate::invoice::{
    Customer, Discount as InvoiceDiscount, DraftDiscount, DraftLine, Invoice, InvoiceEvent,
    InvoiceLine,
};
use crate::posting::{
    PostingAccounts, entry_for_credit, entry_for_issue, entry_for_payment, entry_for_refund,
};
use crate::vat::TaxError;

#[derive(Debug, thiserror::Error)]
pub enum SalesError {
    #[error("an invoice needs at least one line that comes to something")]
    NothingToInvoice,
    #[error("invoice {0} has not been issued")]
    NotIssued(String),
    #[error("only {outstanding} is outstanding; the payment is {offered}")]
    Overpayment { outstanding: Money, offered: Money },
    #[error("the business holds only {held}; the refund is {offered}")]
    Overrefund { held: Money, offered: Money },
    #[error("the invoice is in {expected} and the payment is in {found}")]
    PaymentCurrency {
        expected: CurrencyCode,
        found: CurrencyCode,
    },
    #[error("a payment must be a positive amount")]
    NotAPayment,
    #[error("invoice {invoice} was already cancelled by {by}")]
    AlreadyCancelled { invoice: String, by: String },
    #[error("invoice {0} has been paid; refund it before crediting it")]
    HasPayments(String),
    /// A credit note naming a treatment the invoice never carried.
    ///
    /// **Unreachable by construction now that a credit line names an invoice
    /// line**: the treatment comes off that line, so it is one the invoice had.
    /// Kept as a stop rather than an unwrap — if it ever fires, the aggregate's
    /// bands and its lines disagree, and that is a corrupt log rather than
    /// something to guess past (L6).
    #[error("invoice {invoice} has nothing treated as {category}")]
    CreditWithoutABand { invoice: String, category: String },
    /// More credited than is left — of a **line**, or of a **band**.
    ///
    /// Both caps, because they catch different things. The line one stops a
    /// credit note taking back more of an item than was sold. The band one
    /// stops the total exceeding what was charged at that rate — and it is
    /// still needed, because a document discount comes off the band, so an
    /// invoice's lines sum to more than its bands whenever it carried one.
    #[error("{amount} is more than is left to credit")]
    CreditTooLarge { amount: Money },
    /// A credit note naming a line the invoice does not have.
    #[error("invoice {invoice} has no line {line}")]
    NoSuchLine { invoice: String, line: u16 },
    /// A credit note against an invoice that has already been cancelled
    /// outright, or a cancellation of one that has been partly credited. Both
    /// would credit the same supply twice.
    #[error("invoice {0} has already been credited")]
    AlreadyCredited(String),
    #[error("a credit note must credit something")]
    NothingToCredit,
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error("there is no customer {0} to issue this to")]
    NoSuchCustomer(String),
    #[error(transparent)]
    Tax(#[from] TaxError),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    #[error(transparent)]
    Numbering(#[from] erp_eventlog::NumberingError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    /// The ledger refused the posting — a missing or closed account, almost
    /// always. Passed through rather than reworded: the ledger's message names
    /// the account, and that is what the person fixing it needs.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl erp_i18n::Localize for SalesError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NothingToInvoice => Message::new(messages::NOTHING_TO_INVOICE),
            Self::NoSuchCustomer(id) => Message::new(messages::NO_SUCH_CUSTOMER)
                .with("customer", MessageArg::text(id.clone())),
            Self::NotIssued(id) => {
                Message::new(messages::NOT_ISSUED).with("invoice", MessageArg::text(id.clone()))
            }
            Self::Overpayment {
                outstanding,
                offered,
            } => Message::new(messages::OVERPAYMENT)
                .with("outstanding", MessageArg::text(outstanding.to_string()))
                .with("offered", MessageArg::text(offered.to_string())),
            Self::Overrefund { held, offered } => Message::new(messages::OVERREFUND)
                .with("held", MessageArg::text(held.to_string()))
                .with("offered", MessageArg::text(offered.to_string())),
            Self::PaymentCurrency { expected, found } => Message::new(messages::PAYMENT_CURRENCY)
                .with("expected", MessageArg::text(expected.to_string()))
                .with("found", MessageArg::text(found.to_string())),
            Self::NotAPayment => Message::new(messages::NOT_A_PAYMENT),
            Self::AlreadyCancelled { by, .. } => {
                Message::new(messages::ALREADY_CANCELLED).with("by", MessageArg::text(by.clone()))
            }
            Self::HasPayments(invoice) => Message::new(messages::HAS_PAYMENTS)
                .with("invoice", MessageArg::text(invoice.clone())),
            Self::CreditWithoutABand { invoice, category } => {
                Message::new(messages::CREDIT_WITHOUT_A_BAND)
                    .with("invoice", MessageArg::text(invoice.clone()))
                    .with("category", MessageArg::text(category.clone()))
            }
            Self::NoSuchLine { invoice, line } => Message::new(messages::NO_SUCH_LINE)
                .with("invoice", MessageArg::text(invoice.clone()))
                .with("line", MessageArg::Count(i64::from(*line))),
            Self::CreditTooLarge { amount } => Message::new(messages::CREDIT_TOO_LARGE)
                .with("amount", MessageArg::text(amount.to_string())),
            Self::AlreadyCredited(invoice) => Message::new(messages::ALREADY_CREDITED)
                .with("invoice", MessageArg::text(invoice.clone())),
            Self::NothingToCredit => Message::new(messages::NOTHING_TO_CREDIT),
            Self::InvalidReference(reference) => Message::new(messages::INVALID_REFERENCE)
                .with("reference", MessageArg::text(reference.clone())),
            Self::Tax(TaxError::MixedCurrencies) => Message::new(messages::MIXED_CURRENCIES),
            Self::Tax(TaxError::NotADiscount) => Message::new(messages::NOT_A_DISCOUNT),
            Self::Tax(TaxError::DiscountWithoutABand) => {
                Message::new(messages::DISCOUNT_WITHOUT_A_BAND)
            }
            Self::Tax(TaxError::DiscountTooLarge) => Message::new(messages::DISCOUNT_TOO_LARGE),
            Self::Tax(TaxError::OutOfRange) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
            // All four already say the right thing in both languages.
            Self::Config(e) => e.message(),
            Self::Numbering(e) => e.message(),
            Self::Unbalanced(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Outcome = Result<Committed<InvoiceEvent>, CommandError<SalesError>>;

/// A document that now exists, and the number on it.
///
/// The number comes back even when the command did nothing. A client whose
/// request timed out and retried has to be told the number the invoice already
/// carries — telling it "done" and nothing else would leave it to guess, and the
/// guess would be a number that does not exist.
#[derive(Debug)]
pub struct Numbered {
    pub committed: Committed<InvoiceEvent>,
    pub number: String,
}

type NumberedOutcome = Result<Numbered, CommandError<SalesError>>;

/// Everything an invoice needs to be issued.
///
/// A struct rather than eight parameters, because half of them are strings and
/// transposing two strings is a bug no type can catch.
#[derive(Debug, Clone)]
pub struct Draft {
    pub customer: Customer,
    /// The tax point. Not the wall clock — a March supply invoiced in April is
    /// still March.
    pub issued_on: Timestamp,
    pub due_on: Option<Timestamp>,
    pub currency: CurrencyCode,
    /// What is being charged for and how each line is treated. The **rate**
    /// comes from the tenant's configuration, resolved in the transaction that
    /// writes the invoice.
    pub lines: Vec<DraftLine>,
    /// What comes off the whole invoice. Each becomes a `cac:AllowanceCharge`
    /// on the document, so a customer sees the discount rather than a smaller
    /// number with no explanation.
    #[allow(clippy::struct_field_names, reason = "it is what it is called")]
    pub discounts: Vec<DraftDiscount>,
    /// **Bills for money taken before the supply.** A deposit.
    ///
    /// The document is a prepayment invoice to the authority rather than an
    /// ordinary one, because receiving consideration is its own tax point.
    /// Everything else about issuing it is the same.
    pub prepayment: bool,
    pub note: String,
}

/// Money arriving against an invoice.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// The client's or the bank's own reference. Recording the same one twice is
    /// a no-op.
    pub reference: String,
    pub amount: Money,
    pub received_on: Timestamp,
    /// The cash or bank account that took it.
    pub into: AggregateId,
}

/// Issues an invoice and posts it to the ledger, in one transaction.
///
/// Re-issuing the same `id` is a no-op — the stored invoice wins, and the second
/// caller's lines are ignored rather than applied. That is what makes a retried
/// request safe; a client that meant a different invoice should send a different
/// id.
pub async fn issue_invoice(
    db: &TenantDb,
    id: &AggregateId,
    draft: &Draft,
    metadata: &Metadata,
) -> NumberedOutcome {
    if draft.lines.is_empty() {
        return Err(rejected(SalesError::NothingToInvoice));
    }

    let memo = format!("Invoice {id} · {}", draft.customer.name);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match issue_in(&mut tx, id, draft, &memo, metadata).await {
            Ok(numbered) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(numbered);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(id))
}

/// One attempt at issuing: the invoice event and its journal entry, in the
/// caller's transaction.
///
/// **Public because a till composes it.** `pos` writes a shift's own event, this
/// invoice and its payment in one transaction, for the same reason this module
/// calls `ledger::post_entry_in` rather than posting a moment later: a sale that
/// exists in one place and not the other is a state nobody could explain.
pub async fn issue_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    draft: &Draft,
    memo: &str,
    metadata: &Metadata,
) -> Result<Numbered, ExecuteError<SalesError>> {
    // **Derived here and not taken as an argument.** It used to be a parameter,
    // and `cancel_in` reverses it by rebuilding the same name — so a caller that
    // chose a different one issued an invoice that could never be credited.
    // `pos` did exactly that, and the test that caught it is
    // `a_return_hands_the_money_back_and_credits_the_sale`. A name only this
    // module can get wrong is a name only this module should write.
    let entry_id = &issue_entry(id)?;
    // **The customer reference, checked in this transaction.**
    //
    // Against the *log* and not `proj_crm.customer`, because `crm` is a
    // different projection group running on its own checkpoint: a customer
    // created a moment ago is not in that table yet, and validating against it
    // would refuse an invoice to somebody the caller has just created. Same
    // question and same answer as `ledger::accepts_postings` one module over.
    //
    // Reading `crm`'s **write** side is not the cross-group read L3 forbids.
    // That law is about projection groups, and this touches none: it is the
    // event log, which every module shares by design.
    if let Some(customer) = &draft.customer.id
        && !crm::accepts_documents(&mut *conn, customer)
            .await
            .map_err(ExecuteError::Load)?
    {
        return Err(ExecuteError::Rejected(SalesError::NoSuchCustomer(
            customer.to_string(),
        )));
    }

    // **The rate, in this transaction too.** It used to be a constant the API
    // handler stamped onto each line before the command ran; it is now the
    // tenant's, and reading it here is what stops an invoice carrying a rate
    // that was never current — the same argument as the accounts below.
    let rates = ledger::Rates::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?;

    let lines = priced_lines(&draft.lines, rates)?;

    // The rate comes from the same configuration the lines' does, so a discount
    // on a standard-rated invoice reduces the tax at the rate that invoice was
    // stamped with.
    let discounts: Vec<InvoiceDiscount> = draft
        .discounts
        .iter()
        .map(|discount| InvoiceDiscount {
            reason: discount.reason.clone(),
            amount: discount.amount,
            vat: crate::vat::Vat::at(rates, discount.category),
        })
        .collect();

    let totals = crate::vat::total(
        lines.iter().map(|l| (l.vat, l.net)),
        discounts.iter().map(|d| (d.vat, d.amount)),
        draft.currency,
    )
    .map_err(|e| ExecuteError::Rejected(SalesError::Tax(e)))?;
    let totals = &totals;

    // Resolved **in this transaction**, so what the invoice was posted to and
    // what the tenant had configured cannot disagree — and the generation goes
    // into the metadata, which is how "what was configured when this was
    // decided?" stays answerable without ever being read back (L5).
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    // **Before the aggregate is loaded, and before anything else takes a lock.**
    //
    // Reserving first serializes every issue in this series from here to the end
    // of the transaction, which is what makes the `consume` below correct:
    // nobody else can take this number between the decision and it. It also
    // fixes the lock order — counter, then stream — so two concurrent issues
    // cannot deadlock by taking them the other way round.
    let reserved = erp_eventlog::numbering::reserve(&mut *conn, crate::INVOICE_SERIES)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;
    let number = crate::format_number(crate::INVOICE_PREFIX, reserved);

    let entry_lines = entry_for_issue(totals, &accounts).map_err(|e| {
        ExecuteError::Rejected(match e {
            // Every line cancelled out. A document that moves nothing is not an
            // invoice, and posting it would be an empty journal entry.
            ledger::Unbalanced::TooFewLines(_) => SalesError::NothingToInvoice,
            other => SalesError::Unbalanced(other),
        })
    })?;

    // **`try_create`, not `try_execute`.** A second issue under a taken id used
    // to return success carrying the *first* invoice's number, which lost a sale
    // and told the till it was saved. The kernel now tells a retry from a
    // different request by the fingerprint the caller put in the metadata.
    let committed = try_create::<Invoice, _, SalesError>(
        &mut *conn,
        id,
        crate::upcasters(),
        &metadata,
        |_loaded| {
            Ok(Decision::one(InvoiceEvent::Issued {
                number: Some(number.clone()),
                prepayment: draft.prepayment,
                customer: Box::new(draft.customer.clone()),
                issued_on: draft.issued_on,
                due_on: draft.due_on,
                currency: draft.currency,
                lines: lines.clone(),
                discounts: discounts.clone(),
                totals: totals.clone(),
                note: draft.note.trim().to_owned(),
            }))
        },
    )
    .await?;

    // Only when something was written. Re-issuing the same invoice appends
    // nothing, and burning a number there would put a gap in the sequence of a
    // business whose client merely retried a timed-out request.
    let number = if committed.at.is_some() {
        erp_eventlog::numbering::consume(&mut *conn, crate::INVOICE_SERIES)
            .await
            .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;
        number
    } else {
        // A retry. One extra load to tell the caller the number the invoice
        // already carries — on the one path where the client is repeating
        // itself anyway, and the alternative is a client left to guess.
        //
        // An invoice from before this system numbered anything has none stored:
        // its id *was* its number.
        erp_eventlog::load::<Invoice>(&mut *conn, id, crate::upcasters())
            .await?
            .aggregate
            .number
            .unwrap_or_else(|| id.as_str().to_owned())
    };

    // Runs even when the invoice was already issued, and is a no-op then too.
    // That is what heals a half-finished write from an older, less careful
    // version of this code — and it costs one load.
    ledger::post_entry_in(
        conn,
        entry_id,
        draft.issued_on,
        memo,
        &entry_lines,
        &metadata,
    )
    .await
    .map_err(lift)?;

    Ok(Numbered { committed, number })
}

/// Records money received against an invoice, and moves it in the ledger.
///
/// Recording the same `reference` twice is a no-op.
pub async fn record_payment(
    db: &TenantDb,
    invoice: &AggregateId,
    receipt: &Receipt,
    metadata: &Metadata,
) -> Outcome {
    if !receipt.amount.is_positive() {
        return Err(rejected(SalesError::NotAPayment));
    }

    // Scoped by invoice as well as reference: two customers can both call their
    // transfer "march".
    let memo = format!("Payment {} · invoice {invoice}", receipt.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match pay_in(&mut tx, invoice, receipt, &memo, metadata).await {
            Ok(committed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(invoice))
}

/// **Matches a `crm` record to an invoice that was issued without one.**
///
/// The reconciliation surface Phase 7a asked for, and the reason that phase
/// says *surface* rather than *foreign key*: invoices issued before `crm`
/// existed name a buyer no record matches, and a constraint would have refused
/// every one of them at once instead of letting somebody work through the list.
///
/// **It writes the reference and never the printed name.** What the document
/// says about its buyer was frozen at issue and stays frozen (L5); this is the
/// pointer that makes "everything for this customer" answerable.
///
/// Attaching the same record twice writes nothing. Attaching a *different* one
/// is a correction and does write, because a match made to the wrong customer
/// has to be fixable — and the log keeps both, so the correction is visible.
pub async fn attach_customer(
    db: &TenantDb,
    invoice: &AggregateId,
    customer: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;

            // Against the log, not `proj_crm` — the same question `issue_in`
            // asks and for the same reason: a projection lags, and a
            // reconciliation run right after creating the record would be
            // refused for a customer that plainly exists.
            if !crm::accepts_documents(&mut *conn, customer)
                .await
                .map_err(ExecuteError::Load)?
            {
                return Err(ExecuteError::Rejected(SalesError::NoSuchCustomer(
                    customer.to_string(),
                )));
            }

            try_execute::<Invoice, _, SalesError>(
                &mut *conn,
                invoice,
                crate::upcasters(),
                metadata,
                |loaded| {
                    let held = &loaded.aggregate;
                    if !held.issued {
                        return Err(SalesError::NotIssued(invoice.as_str().to_owned()));
                    }
                    if held.points_at(customer) {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(InvoiceEvent::CustomerAttached {
                        customer: customer.clone(),
                        at,
                    }))
                },
            )
            .await
        }
        .await;

        match outcome {
            Ok(committed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(invoice))
}

/// Money handed back to a customer.
///
/// **The mirror of a payment, and the thing this module had no concept of.**
/// `cancel_invoice` refuses an invoice the business is still holding money
/// against, which meant no *paid* invoice could ever be credited — and every
/// till sale is paid the instant it happens. A return was therefore unreachable
/// through any route, which is what this closes.
///
/// Refunding more than is held is refused for the reason overpaying is: a
/// business handing back money it never took has made a decision somebody needs
/// to see, and a negative balance is how that decision never gets made.
///
/// # It issues the credit note as well, when the invoice comes clear
///
/// A tax invoice is a statement about a supply, and handing the money back
/// changes the supply. The Kingdom's answer is a **credit note** — its own
/// number, its own tax point — and `tax_sa` already builds, signs and submits
/// one from `sales.invoice.cancelled`. What was missing was anybody asking, so
/// a refunded invoice stayed cleared at the full amount and the customer still
/// owed for it in the books.
///
/// Both in one transaction: a refund recorded without its credit note is a
/// document nobody would go looking for.
///
/// **A partial refund gets no credit note**, and that is the deferral rather
/// than a decision taken here — one for part of an invoice carries tax bands of
/// its own, and how an arbitrary amount divides across a standard-rated line
/// and a zero-rated one is not something this system may guess. [`cancel_in`]
/// answers `HasPayments` while the invoice is still holding money, which is
/// what that refusal means and why it is read rather than propagated.
pub async fn refund_invoice(
    db: &TenantDb,
    invoice: &AggregateId,
    receipt: &Receipt,
    reason: &str,
    metadata: &Metadata,
) -> Outcome {
    if !receipt.amount.is_positive() {
        return Err(rejected(SalesError::NotAPayment));
    }
    let memo = format!("Refund {} · invoice {invoice}", receipt.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let refunded = async {
            let committed = refund_in(&mut tx, invoice, receipt, &memo, metadata).await?;
            credit_what_is_clear(
                &mut tx,
                invoice,
                &receipt.reference,
                reason,
                receipt.received_on,
                metadata,
            )
            .await?;
            Ok::<_, ExecuteError<SalesError>>(committed)
        }
        .await;
        match refunded {
            Ok(committed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(invoice))
}

/// Credits an invoice a refund has left holding nothing.
///
/// **`HasPayments` is the ordinary answer, not a failure.** It is what
/// [`cancel_in`] says while an invoice is still holding money, which after a
/// partial refund is simply true. `AlreadyCancelled` is the same: the document
/// exists, which is the outcome wanted.
///
/// Not folded into [`refund_in`], deliberately. That one is a per-money-movement
/// primitive — a till calls it once per tender — and a credit note is per
/// document. Crediting there would issue one against a single tender's
/// reference and try again for every other.
pub async fn credit_what_is_clear(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    reference: &str,
    reason: &str,
    on: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    match credit_in(&mut *conn, invoice, reference, reason, on, metadata).await {
        Err(ExecuteError::Rejected(
            SalesError::HasPayments(_) | SalesError::AlreadyCancelled { .. },
        )) => Ok(()),
        other => other.map(|_| ()),
    }
}

/// One attempt at refunding, in the caller's transaction. Public for the reason
/// [`issue_in`] is: a till hands the money back in the same write that credits
/// the sale.
pub async fn refund_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    receipt: &Receipt,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<InvoiceEvent>, ExecuteError<SalesError>> {
    let entry_id = &money_entry("sr", invoice, &receipt.reference)?;
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let entry_lines = entry_for_refund(receipt.amount, &receipt.into, &accounts)
        .map_err(|e| ExecuteError::Rejected(SalesError::Unbalanced(e)))?;

    let committed = try_execute::<Invoice, _, SalesError>(
        &mut *conn,
        invoice,
        crate::upcasters(),
        &metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.issued {
                return Err(SalesError::NotIssued(invoice.as_str().to_owned()));
            }
            if state.has_refund(&receipt.reference) {
                return Ok(Decision::nothing());
            }

            let held = state
                .held()
                .ok_or_else(|| SalesError::NotIssued(invoice.as_str().to_owned()))?;

            if held.currency() != receipt.amount.currency() {
                return Err(SalesError::PaymentCurrency {
                    expected: held.currency(),
                    found: receipt.amount.currency(),
                });
            }
            if receipt.amount.minor() > held.minor() {
                return Err(SalesError::Overrefund {
                    held,
                    offered: receipt.amount,
                });
            }

            Ok(Decision::one(InvoiceEvent::Refunded {
                refund: receipt.reference.clone(),
                amount: receipt.amount,
                refunded_on: receipt.received_on,
                account: receipt.into.clone(),
            }))
        },
    )
    .await?;

    if !committed.events.is_empty() {
        ledger::post_entry_in(
            conn,
            entry_id,
            receipt.received_on,
            memo,
            &entry_lines,
            &metadata,
        )
        .await
        .map_err(lift)?;
    }

    Ok(committed)
}

/// One attempt at recording a payment, in the caller's transaction. Public for
/// the reason [`issue_in`] is.
pub async fn pay_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    receipt: &Receipt,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<InvoiceEvent>, ExecuteError<SalesError>> {
    // Derived here, for the reason `issue_in` derives its own.
    let entry_id = &money_entry("sp", invoice, &receipt.reference)?;
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let entry_lines = entry_for_payment(receipt.amount, &receipt.into, &accounts)
        .map_err(|e| ExecuteError::Rejected(SalesError::Unbalanced(e)))?;

    let committed = try_execute::<Invoice, _, SalesError>(
        &mut *conn,
        invoice,
        crate::upcasters(),
        &metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.issued {
                return Err(SalesError::NotIssued(invoice.as_str().to_owned()));
            }
            if state.has_payment(&receipt.reference) {
                return Ok(Decision::nothing());
            }

            let outstanding = state
                .outstanding()
                .ok_or_else(|| SalesError::NotIssued(invoice.as_str().to_owned()))?;

            if outstanding.currency() != receipt.amount.currency() {
                return Err(SalesError::PaymentCurrency {
                    expected: outstanding.currency(),
                    found: receipt.amount.currency(),
                });
            }
            // Refused rather than parked as a credit. A customer who overpays
            // has done something the business needs to decide about, and
            // silently swallowing it into a negative receivable is how that
            // decision never gets made.
            if receipt.amount.minor() > outstanding.minor() {
                return Err(SalesError::Overpayment {
                    outstanding,
                    offered: receipt.amount,
                });
            }

            Ok(Decision::one(InvoiceEvent::PaymentRecorded {
                payment: receipt.reference.clone(),
                amount: receipt.amount,
                received_on: receipt.received_on,
                account: receipt.into.clone(),
            }))
        },
    )
    .await?;

    // A payment already recorded also already posted — in this same transaction,
    // the first time. Posting again would be a no-op anyway; skipping it saves
    // the loads.
    if !committed.events.is_empty() {
        ledger::post_entry_in(
            conn,
            entry_id,
            receipt.received_on,
            memo,
            &entry_lines,
            &metadata,
        )
        .await
        .map_err(lift)?;
    }

    Ok(committed)
}

// ---------------------------------------------------------------------------

/// A journal entry id derived from a sales document.
///
/// Prefixed so a sales posting can never land on a journal entry someone posted
/// by hand — which would be absorbed silently, because posting an existing entry
/// id is a no-op.
fn derived_id(prefix: &str, parts: &[&str]) -> Result<AggregateId, CommandError<SalesError>> {
    let joined = format!("{prefix}.{}", parts.join("."));
    AggregateId::new(&joined).map_err(|_| rejected(SalesError::InvalidReference(parts.join("."))))
}

/// Cancels an invoice by crediting it: the journal entry it made is reversed,
/// and the invoice records which credit note did it.
///
/// # What this is not
///
/// Not a deletion. The invoice was issued, the customer may hold a copy, and
/// the books end up showing both it and the credit — which is the same reason
/// the ledger reverses rather than deletes.
///
/// Not a *partial* credit either. Crediting some lines and not others is a
/// document with lines of its own, and nobody has asked for one. ponytail: when
/// they do, it is a second command and this one stays as the whole-invoice case.
///
/// # Why an invoice with payments is refused
///
/// The money is somewhere. Cancelling the document without moving it back would
/// leave cash on the books against a sale that no longer exists, and this system
/// has no way to model the refund yet. Refusing says so; guessing would not.
pub async fn cancel_invoice(
    db: &TenantDb,
    invoice: &AggregateId,
    credit_note: &str,
    reason: &str,
    on: Timestamp,
    metadata: &Metadata,
) -> NumberedOutcome {
    let unusable = |_| ExecuteError::Rejected(SalesError::NotIssued(invoice.as_str().to_owned()));
    let entry_id = derived_id("si", &[invoice.as_str()]).map_err(unusable)?;
    let credit_id = derived_id("cn", &[invoice.as_str(), credit_note]).map_err(unusable)?;
    let memo = format!("Credit note {credit_note} · invoice {invoice}");

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match cancel_in(
            &mut tx,
            invoice,
            &entry_id,
            &credit_id,
            credit_note,
            reason,
            on,
            &memo,
            metadata,
        )
        .await
        {
            Ok(numbered) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(numbered);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(invoice))
}

/// The journal entry an invoice's issue posts under.
///
/// One function, because `cancel_in` reverses it by name and the two must agree.
fn issue_entry(invoice: &AggregateId) -> Result<AggregateId, ExecuteError<SalesError>> {
    derived_id("si", &[invoice.as_str()])
        .map_err(|_| ExecuteError::Rejected(SalesError::NotIssued(invoice.as_str().to_owned())))
}

/// **The name of the journal entry an invoice's issue posts under.**
///
/// Public because a report has to be able to say *which postings this document
/// made* — the §10b reconciliation is "the debits of the entry this invoice
/// posted equal what this invoice came to", and it cannot ask `proj_sales` or
/// `proj_ledger` for the answer without the cross-group read L3 forbids.
///
/// The alternative was for `reports` to reimplement `si.{invoice}`, which is
/// the shape of bug that stays quiet until somebody changes the prefix here.
/// The scheme belongs to this module; naming it out loud is cheaper than a
/// second copy of it.
#[must_use]
pub fn issue_entry_of(invoice: &str) -> String {
    format!("si.{invoice}")
}

/// The name of the entry a credit note posts under. See [`issue_entry_of`].
#[must_use]
pub fn credit_entry_of(invoice: &str, credit_note: &str) -> String {
    format!("cn.{invoice}.{credit_note}")
}

/// The journal entry a payment or a refund posts under. Scoped by invoice as
/// well as reference: two customers can both call their transfer "march".
fn money_entry(
    prefix: &str,
    invoice: &AggregateId,
    reference: &str,
) -> Result<AggregateId, ExecuteError<SalesError>> {
    derived_id(prefix, &[invoice.as_str(), reference])
        .map_err(|_| ExecuteError::Rejected(SalesError::NotIssued(invoice.as_str().to_owned())))
}

/// Credits an invoice inside the caller's transaction.
///
/// Public for the reason [`issue_in`] and [`refund_in`] are — a till credits the
/// sale in the same write that hands the money back. It derives both journal
/// entry ids itself, because they belong to **this** module's scheme: the one
/// being reversed is the entry `issue_in` posted, and a caller cannot be
/// expected to know how that was named.
pub async fn credit_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    credit_note: &str,
    reason: &str,
    on: Timestamp,
    metadata: &Metadata,
) -> Result<Numbered, ExecuteError<SalesError>> {
    let entry_id = issue_entry(invoice)?;
    let credit_id = money_entry("cn", invoice, credit_note)?;
    let memo = format!("Credit note {credit_note} · invoice {invoice}");
    cancel_in(
        conn,
        invoice,
        &entry_id,
        &credit_id,
        credit_note,
        reason,
        on,
        &memo,
        metadata,
    )
    .await
}

/// One attempt at crediting: the ledger reversal and the invoice's own event,
/// in the caller's transaction.
#[expect(
    clippy::too_many_arguments,
    reason = "every one is a value computed before the transaction opened"
)]
async fn cancel_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    entry_id: &AggregateId,
    credit_id: &AggregateId,
    credit_note: &str,
    reason: &str,
    on: Timestamp,
    memo: &str,
    metadata: &Metadata,
) -> Result<Numbered, ExecuteError<SalesError>> {
    let reference = credit_note.to_owned();
    let reason = reason.trim().to_owned();
    let mut already = false;

    // Same order as issuing: the counter first. A credit note is a statutory
    // document in its own right and gets its own gapless series.
    let reserved = erp_eventlog::numbering::reserve(&mut *conn, crate::CREDIT_NOTE_SERIES)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;
    let credit_note = crate::format_number(crate::CREDIT_NOTE_PREFIX, reserved);

    let committed = try_execute::<Invoice, _, SalesError>(
        &mut *conn,
        invoice,
        crate::upcasters(),
        metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.issued {
                return Err(SalesError::NotIssued(invoice.as_str().to_owned()));
            }
            // A retry, not a second credit. Compared on the client's key, not
            // on the number — the number is ours, and a retry is handed a
            // different one.
            if state.cancelled_by.as_deref() == Some(reference.as_str()) {
                return Ok(Decision::nothing());
            }
            if let Some(by) = &state.cancelled_by {
                return Err(SalesError::AlreadyCancelled {
                    invoice: invoice.as_str().to_owned(),
                    by: by.clone(),
                });
            }
            // **A cancellation reverses the whole issue entry.** On an invoice
            // that has already been partly credited, that would take back what
            // has already been taken back — the credited part twice, in the
            // books and in the VAT return. Credit the rest instead.
            if state.is_partly_credited() {
                return Err(SalesError::AlreadyCredited(invoice.as_str().to_owned()));
            }
            // **What matters is the money, not whether a payment exists.**
            // This used to refuse any invoice that had ever been paid, which
            // made a till sale — paid the instant it happens — impossible to
            // credit through any route. What a credit note may not do is undo a
            // supply while the business keeps the cash: refund it first, and
            // then the sale can be undone.
            let held = state
                .held()
                .ok_or_else(|| SalesError::NotIssued(invoice.as_str().to_owned()))?;
            if held.is_zero() {
                Ok(Decision::one(InvoiceEvent::Cancelled {
                    credit_note: credit_note.clone(),
                    reference: Some(reference.clone()),
                    reason: reason.clone(),
                    on,
                }))
            } else {
                Err(SalesError::HasPayments(invoice.as_str().to_owned()))
            }
        },
    )
    .await?;

    already |= committed.events.is_empty();

    let number = if already {
        // A repeat of a cancellation that already happened. Tell the caller the
        // credit note that exists, not the one they would have got.
        erp_eventlog::load::<Invoice>(&mut *conn, invoice, crate::upcasters())
            .await?
            .aggregate
            .credit_note
            .unwrap_or(credit_note)
    } else {
        erp_eventlog::numbering::consume(&mut *conn, crate::CREDIT_NOTE_SERIES)
            .await
            .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;
        ledger::reverse_in(conn, entry_id, credit_id, on, memo, metadata)
            .await
            .map_err(lift)?;
        credit_note
    };

    Ok(Numbered { committed, number })
}

/// Resolves each draft line: its allowances come off, and the rate goes on.
///
/// **The allowances come off before anything else sees the line.** What the
/// caller gets back is BT-131 — the line net amount, which the standard defines
/// as the price less the line's own allowances — so the bands, the tax and the
/// posting all follow from one number and nothing has to remember to subtract
/// twice.
fn priced_lines(
    draft: &[DraftLine],
    rates: ledger::Rates,
) -> Result<Vec<InvoiceLine>, ExecuteError<SalesError>> {
    draft
        .iter()
        .map(|line| {
            if line.allowances.iter().any(|a| !a.amount.is_positive()) {
                // A negative allowance is a surcharge, which is a different
                // element and a different conversation.
                return Err(ExecuteError::Rejected(SalesError::Tax(
                    crate::vat::TaxError::NotADiscount,
                )));
            }
            let net = line
                .allowances
                .iter()
                .try_fold(line.net, |running, a| running.checked_sub(a.amount))
                .map_err(|e| ExecuteError::Rejected(SalesError::Tax(e.into())))?;
            if !line.allowances.is_empty() && !net.is_positive() {
                return Err(ExecuteError::Rejected(SalesError::Tax(
                    crate::vat::TaxError::DiscountTooLarge,
                )));
            }
            Ok(InvoiceLine {
                description: line.description.clone(),
                net,
                vat: crate::vat::Vat::at(rates, line.category),
                allowances: line.allowances.clone(),
            })
        })
        .collect()
}

/// The accounts a sale moves, plus metadata stamped with the generation they
/// came from.
async fn resolve_accounts(
    conn: &mut sqlx::PgConnection,
    metadata: &Metadata,
) -> Result<(PostingAccounts, Metadata), ExecuteError<SalesError>> {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?;
    let version = erp_eventlog::configuration::version(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?;

    Ok((
        accounts,
        Metadata {
            config_version: Some(version),
            ..metadata.clone()
        },
    ))
}

fn rejected(error: SalesError) -> CommandError<SalesError> {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn contended(id: &AggregateId) -> CommandError<SalesError> {
    ExecuteError::Contended {
        stream: StreamId::new(<Invoice as erp_eventlog::Aggregate>::domain(), id.clone()),
        attempts: MAX_ATTEMPTS,
    }
    .into()
}

/// Carries a ledger failure into this module's error type without flattening
/// what kind of failure it was — a rejection stays a rejection, a conflict stays
/// a conflict, so the retry loop above still recognises it.
fn lift(error: ExecuteError<LedgerError>) -> ExecuteError<SalesError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(SalesError::Ledger(e)),
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

/// The `Send` guard.
///
/// Both commands are called from axum handlers, whose futures must be `Send`.
/// When they are not, rustc reports the failure at the *route table* with types
/// from files that look unrelated (rust-lang/rust#102211) — so the assertion
/// lives here, in the crate that owns the code, where the error lands on the
/// line that caused it. See `erp-control/src/provision.rs` for the four triggers
/// this catches.
const _: fn() = || {
    fn assert_send<T: Send>(_: T) {}
    fn commands_are_send(
        db: &TenantDb,
        id: &AggregateId,
        draft: &Draft,
        receipt: &Receipt,
        metadata: &Metadata,
    ) {
        assert_send(issue_invoice(db, id, draft, metadata));
        assert_send(record_payment(db, id, receipt, metadata));
        assert_send(cancel_invoice(
            db,
            id,
            "",
            "",
            erp_types::Timestamp::UNIX_EPOCH,
            metadata,
        ));
    }
    let _ = commands_are_send;
};

#[cfg(test)]
mod entry_name_tests {
    use super::*;

    /// **The public names and the private derivation must agree**, or a report
    /// reconciles against entries that do not exist and reports every document
    /// as a discrepancy.
    #[test]
    fn the_published_entry_names_are_the_ones_that_get_posted() {
        let invoice = AggregateId::new("inv-1").expect("a valid id");

        assert_eq!(
            issue_entry(&invoice).expect("derives").as_str(),
            issue_entry_of("inv-1")
        );
        assert_eq!(
            derived_id("cn", &[invoice.as_str(), "cn-9"])
                .expect("derives")
                .as_str(),
            credit_entry_of("inv-1", "cn-9")
        );
    }
}

// ---------------------------------------------------------------------------
// Partial credit notes
// ---------------------------------------------------------------------------

/// One line of a credit note.
///
/// **No rate on it.** What a line is treated as is the caller's to say; the rate
/// that treatment was charged at is the *invoice's*, and reading it from
/// anywhere else is how a 2019 invoice gets credited at today's 15%.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditLine {
    /// **Which line of the invoice this credits**, by position.
    ///
    /// The description and the treatment come from it, so a credit note cannot
    /// describe something the invoice never charged for, and cannot be given a
    /// rate the invoice never carried.
    pub against: u16,
    /// Excluding tax, and positive: what is being taken back off that line,
    /// stated the way the invoice stated it rather than as a negative. It may
    /// be less than the line — part of a line can come back.
    pub net: Money,
}

/// A credit note against part of an invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditNote {
    /// The client's key. Sending it again is a retry, not a second credit note.
    pub reference: String,
    pub lines: Vec<CreditLine>,
    /// Why, for the customer to read. **It reaches ZATCA** as the document's
    /// note, so it is worth a sentence.
    pub reason: String,
    /// The credit note's own tax point. Not the invoice's — a credit note is a
    /// document in its own right and falls in the period it was issued in.
    pub on: Timestamp,
}

/// **Credits part of an invoice**, as a document with lines of its own.
///
/// # How this differs from cancelling
///
/// [`cancel_invoice`] says the supply is undone and *reverses the journal entry
/// the invoice posted*. This says some of it is undone and posts its own entry
/// for what it takes back — because there is no such thing as reversing part of
/// a journal entry, and because the authority computes a credit note's tax from
/// the credit note's own lines rather than from the invoice's.
///
/// The two are mutually exclusive on one invoice: cancelling something already
/// partly credited would take the credited part back twice, in the books and in
/// the return. Both draw on the same gapless credit-note series, because both
/// produce a credit note and ZATCA does not care which shape made one.
///
/// # Where the rates come from
///
/// The invoice's own bands, never the tenant's current configuration. An
/// invoice issued at 5% is credited at 5% for ever (L5), and a category the
/// invoice never carried is refused rather than given today's rate — which is
/// the same refusal [`crate::vat::TaxError::DiscountWithoutABand`] makes, for
/// the same reason: reclaiming tax that was never charged.
///
/// # What it does not do
///
/// **It does not touch what was paid.** A credit note says the supply is undone;
/// handing the money back is [`refund_invoice`], and a business can do either
/// without the other — an unpaid invoice is credited and no cash moves, a paid
/// one needs both. Unlike [`cancel_invoice`] this does **not** require the
/// invoice to be clear of payments, because a part-credited invoice the customer
/// has paid in full is an ordinary thing: they are owed the difference, and
/// `outstanding` going negative is what says so.
pub async fn credit_invoice_part(
    db: &TenantDb,
    invoice: &AggregateId,
    note: &CreditNote,
    metadata: &Metadata,
) -> NumberedOutcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match credit_part_in(&mut tx, invoice, note, metadata).await {
            Ok(numbered) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(numbered);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(contended(invoice))
}

/// One attempt at crediting part of an invoice, in the caller's transaction.
///
/// Public for the reason [`issue_in`] and [`refund_in`] are: a cancellation
/// policy that keeps half a deposit credits and refunds in one write.
pub async fn credit_part_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    note: &CreditNote,
    metadata: &Metadata,
) -> Result<Numbered, ExecuteError<SalesError>> {
    if note.lines.is_empty() {
        return Err(ExecuteError::Rejected(SalesError::NothingToCredit));
    }
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;
    let credit_id = money_entry("cn", invoice, &note.reference)?;
    let memo = format!("Credit note · invoice {invoice}");

    // **The counter first**, for the reason `issue_in` gives: it fixes the lock
    // order — counter, then stream — so two concurrent credits cannot deadlock
    // by taking them the other way round.
    let reserved = erp_eventlog::numbering::reserve(&mut *conn, crate::CREDIT_NOTE_SERIES)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;
    let number = crate::format_number(crate::CREDIT_NOTE_PREFIX, reserved);

    let committed = try_execute::<Invoice, _, SalesError>(
        &mut *conn,
        invoice,
        crate::upcasters(),
        &metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.issued {
                return Err(SalesError::NotIssued(invoice.as_str().to_owned()));
            }
            // A retry. The stored credit note wins, and the number reserved
            // above is simply not consumed.
            if state.has_credit(&note.reference) {
                return Ok(Decision::nothing());
            }
            // Already cancelled outright, so there is nothing left to credit.
            if state.cancelled_by.is_some() {
                return Err(SalesError::AlreadyCredited(invoice.as_str().to_owned()));
            }
            let currency = state
                .currency
                .ok_or_else(|| SalesError::NotIssued(invoice.as_str().to_owned()))?;

            let lines = priced_for_credit(state, &note.lines, invoice)?;
            let totals = crate::vat::total(
                lines.iter().map(|l| (l.line.vat, l.line.net)),
                // **No allowances on a credit note.** A discount is something
                // taken off before tax was worked out; a credit takes back what
                // was actually charged, and the caller states that directly.
                std::iter::empty(),
                currency,
            )
            .map_err(SalesError::Tax)?;

            // **Per band, against what is left in that band.** Checked here,
            // inside the decision, because it is a fact about the invoice's
            // history and nowhere else knows it.
            for band in &totals.bands {
                let left = state
                    .creditable_in(band.category, band.basis_points)
                    .ok_or_else(|| SalesError::CreditWithoutABand {
                        invoice: invoice.as_str().to_owned(),
                        category: band.category.as_str().to_owned(),
                    })?;
                if band.net.minor() > left.minor() {
                    return Err(SalesError::CreditTooLarge { amount: band.net });
                }
            }

            Ok(Decision::one(InvoiceEvent::Credited {
                credit_note: number.clone(),
                reference: note.reference.clone(),
                lines,
                totals,
                reason: note.reason.trim().to_owned(),
                on: note.on,
            }))
        },
    )
    .await?;

    let Some(InvoiceEvent::Credited { totals, .. }) = committed.events.first() else {
        // A retry. Tell the caller the number **this reference** was given, not
        // the most recent credit note — an invoice may have several, which is
        // the whole point of partial ones. The number reserved above is simply
        // not consumed.
        let existing = erp_eventlog::load::<Invoice>(&mut *conn, invoice, crate::upcasters())
            .await?
            .aggregate
            .credit_note_for(&note.reference)
            .map_or(number, str::to_owned);
        return Ok(Numbered {
            committed,
            number: existing,
        });
    };

    erp_eventlog::numbering::consume(&mut *conn, crate::CREDIT_NOTE_SERIES)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Numbering(e)))?;

    let entry_lines = entry_for_credit(totals, &accounts).map_err(|e| {
        ExecuteError::Rejected(match e {
            ledger::Unbalanced::TooFewLines(_) => SalesError::NothingToCredit,
            other => SalesError::Unbalanced(other),
        })
    })?;
    ledger::post_entry_in(
        &mut *conn,
        &credit_id,
        note.on,
        &memo,
        &entry_lines,
        &metadata,
    )
    .await
    .map_err(lift)?;

    Ok(Numbered { committed, number })
}

/// Resolves each credit line against the invoice line it names.
///
/// **The lookup and the caps are one pass.** Naming a line settles three things
/// at once that used to be separate: what the credit note says, what rate it
/// credits at, and how much of that line is left to credit. A line the invoice
/// does not have has no answer to any of them, which is a refusal rather than a
/// default.
fn priced_for_credit(
    state: &Invoice,
    lines: &[CreditLine],
    invoice: &AggregateId,
) -> Result<Vec<crate::invoice::CreditedLine>, SalesError> {
    // Two lines of one credit note may name the same invoice line; the cap is
    // on what they come to together, not on each in turn.
    let mut taken: Vec<Money> = state.credited_lines.clone();

    lines
        .iter()
        .map(|line| {
            if !line.net.is_positive() {
                return Err(SalesError::NothingToCredit);
            }
            let nowhere = || SalesError::NoSuchLine {
                invoice: invoice.as_str().to_owned(),
                line: line.against,
            };
            let against = state.lines.get(line.against as usize).ok_or_else(nowhere)?;
            let slot = taken.get_mut(line.against as usize).ok_or_else(nowhere)?;

            // What is left of this line, counting earlier credit notes and the
            // rest of this one.
            let running = slot
                .checked_add(line.net)
                .map_err(|e| SalesError::Tax(e.into()))?;
            if running.minor() > against.net.minor() {
                return Err(SalesError::CreditTooLarge { amount: line.net });
            }
            *slot = running;

            Ok(crate::invoice::CreditedLine {
                against: line.against,
                line: InvoiceLine {
                    // **The invoice's words, not the caller's.** A credit note
                    // that could describe anything described nothing.
                    description: against.description.clone(),
                    net: line.net,
                    // And the invoice's rate: one issued at 5% is credited at
                    // 5% for ever (L5).
                    vat: against.vat,
                    // **Stated at what is coming back.** The invoice's own
                    // allowances are what made this line smaller in the first
                    // place; they are not taken off a second time.
                    allowances: Vec::new(),
                },
            })
        })
        .collect()
}
