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

use ledger::LedgerError;
use spa_control::{CommandError, TenantDb};
use spa_eventlog::{Committed, Decision, ExecuteError, MAX_ATTEMPTS, Metadata, try_execute};
use spa_types::{AggregateId, CurrencyCode, Money, StreamId, Timestamp};

use crate::invoice::{Customer, Invoice, InvoiceEvent, InvoiceLine};
use crate::posting::{PostingAccounts, entry_for_issue, entry_for_payment};
use crate::vat::{TaxError, Totals};

#[derive(Debug, thiserror::Error)]
pub enum SalesError {
    #[error("an invoice needs at least one line that comes to something")]
    NothingToInvoice,
    #[error("invoice {0} has not been issued")]
    NotIssued(String),
    #[error("only {outstanding} is outstanding; the payment is {offered}")]
    Overpayment { outstanding: Money, offered: Money },
    #[error("the invoice is in {expected} and the payment is in {found}")]
    PaymentCurrency {
        expected: CurrencyCode,
        found: CurrencyCode,
    },
    #[error("a payment must be a positive amount")]
    NotAPayment,
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error(transparent)]
    Tax(#[from] TaxError),
    #[error(transparent)]
    Config(#[from] spa_eventlog::ConfigError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    /// The ledger refused the posting — a missing or closed account, almost
    /// always. Passed through rather than reworded: the ledger's message names
    /// the account, and that is what the person fixing it needs.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl spa_i18n::Localize for SalesError {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::{Message, MessageArg};
        match self {
            Self::NothingToInvoice => Message::new(messages::NOTHING_TO_INVOICE),
            Self::NotIssued(id) => {
                Message::new(messages::NOT_ISSUED).with("invoice", MessageArg::text(id.clone()))
            }
            Self::Overpayment {
                outstanding,
                offered,
            } => Message::new(messages::OVERPAYMENT)
                .with("outstanding", MessageArg::text(outstanding.to_string()))
                .with("offered", MessageArg::text(offered.to_string())),
            Self::PaymentCurrency { expected, found } => Message::new(messages::PAYMENT_CURRENCY)
                .with("expected", MessageArg::text(expected.to_string()))
                .with("found", MessageArg::text(found.to_string())),
            Self::NotAPayment => Message::new(messages::NOT_A_PAYMENT),
            Self::InvalidReference(reference) => Message::new(messages::INVALID_REFERENCE)
                .with("reference", MessageArg::text(reference.clone())),
            Self::Tax(TaxError::MixedCurrencies) => Message::new(messages::MIXED_CURRENCIES),
            Self::Tax(TaxError::OutOfRange) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
            // All three already say the right thing in both languages.
            Self::Config(e) => e.message(),
            Self::Unbalanced(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Outcome = Result<Committed<InvoiceEvent>, CommandError<SalesError>>;

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
    pub lines: Vec<InvoiceLine>,
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
) -> Outcome {
    if draft.lines.is_empty() {
        return Err(rejected(SalesError::NothingToInvoice));
    }

    // Everything that can be decided without the database is decided here, once
    // — not inside the retry loop, and not inside the transaction.
    let totals = crate::vat::total(draft.lines.iter().map(|l| (l.vat, l.net)), draft.currency)
        .map_err(|e| rejected(SalesError::Tax(e)))?;

    let entry_id = derived_id("si", &[id.as_str()])?;
    let memo = format!("Invoice {id} · {}", draft.customer.name);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match issue_in(&mut tx, id, &entry_id, draft, &totals, &memo, metadata).await {
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

    Err(contended(id))
}

/// One attempt at issuing: the invoice event and its journal entry, in the
/// caller's transaction.
async fn issue_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    entry_id: &AggregateId,
    draft: &Draft,
    totals: &Totals,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<InvoiceEvent>, ExecuteError<SalesError>> {
    // Resolved **in this transaction**, so what the invoice was posted to and
    // what the tenant had configured cannot disagree — and the generation goes
    // into the metadata, which is how "what was configured when this was
    // decided?" stays answerable without ever being read back (L5).
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let entry_lines = entry_for_issue(totals, &accounts).map_err(|e| {
        ExecuteError::Rejected(match e {
            // Every line cancelled out. A document that moves nothing is not an
            // invoice, and posting it would be an empty journal entry.
            ledger::Unbalanced::TooFewLines(_) => SalesError::NothingToInvoice,
            other => SalesError::Unbalanced(other),
        })
    })?;

    let committed = try_execute::<Invoice, _, SalesError>(
        &mut *conn,
        id,
        crate::upcasters(),
        &metadata,
        |loaded| {
            if loaded.aggregate.issued {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(InvoiceEvent::Issued {
                customer: draft.customer.clone(),
                issued_on: draft.issued_on,
                due_on: draft.due_on,
                currency: draft.currency,
                lines: draft.lines.clone(),
                totals: totals.clone(),
                note: draft.note.trim().to_owned(),
            }))
        },
    )
    .await?;

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

    Ok(committed)
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
    let entry_id = derived_id("sp", &[invoice.as_str(), &receipt.reference])?;
    let memo = format!("Payment {} · invoice {invoice}", receipt.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match pay_in(&mut tx, invoice, &entry_id, receipt, &memo, metadata).await {
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

async fn pay_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    entry_id: &AggregateId,
    receipt: &Receipt,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<InvoiceEvent>, ExecuteError<SalesError>> {
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

/// The accounts a sale moves, plus metadata stamped with the generation they
/// came from.
async fn resolve_accounts(
    conn: &mut sqlx::PgConnection,
    metadata: &Metadata,
) -> Result<(PostingAccounts, Metadata), ExecuteError<SalesError>> {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?;
    let version = spa_eventlog::configuration::version(&mut *conn)
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
        stream: StreamId::new(<Invoice as spa_eventlog::Aggregate>::domain(), id.clone()),
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
    }
}

/// The `Send` guard.
///
/// Both commands are called from axum handlers, whose futures must be `Send`.
/// When they are not, rustc reports the failure at the *route table* with types
/// from files that look unrelated (rust-lang/rust#102211) — so the assertion
/// lives here, in the crate that owns the code, where the error lands on the
/// line that caused it. See `spa-control/src/provision.rs` for the four triggers
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
    }
    let _ = commands_are_send;
};
