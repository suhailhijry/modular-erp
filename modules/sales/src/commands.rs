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
use crate::posting::{PostingAccounts, entry_for_issue, entry_for_payment, entry_for_refund};
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

    let lines: Vec<InvoiceLine> = draft
        .lines
        .iter()
        .map(|line| InvoiceLine {
            description: line.description.clone(),
            net: line.net,
            vat: crate::vat::Vat::at(rates, line.category),
        })
        .collect();

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
pub async fn refund_invoice(
    db: &TenantDb,
    invoice: &AggregateId,
    receipt: &Receipt,
    metadata: &Metadata,
) -> Outcome {
    if !receipt.amount.is_positive() {
        return Err(rejected(SalesError::NotAPayment));
    }
    let memo = format!("Refund {} · invoice {invoice}", receipt.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match refund_in(&mut tx, invoice, receipt, &memo, metadata).await {
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
