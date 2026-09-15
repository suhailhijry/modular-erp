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
use crate::limit::Authority;
use crate::posting::{
    PostingAccounts, entry_for_credit, entry_for_issue, entry_for_payment, entry_for_refund,
};
use crate::vat::TaxError;

/// The claim that issuing a credit note requires. One of [`hr::SEGREGATED`]:
/// raising a document and cancelling the money for it must not land in one pair
/// of hands.
pub const APPROVE_CREDIT_NOTE: &str = "sales:approve_credit_note";

/// **Whether this caller may credit an invoice here.**
///
/// **In the roots, so every path is judged the same.** A full cancellation and
/// a partial credit are the same authority, and until §70 this lived in the two
/// `sales` wrappers the `/v1/sales` routes call — so `pos::take_back`, which
/// calls the roots directly, took a return at the till that the sales screen
/// refused the same clerk. It is asked in [`cancel_in`] and [`credit_part_in`],
/// which every credit note in this system goes through, and in [`may_refund`]
/// for the credit note a gateway's refund will leave owing — that one is issued
/// with [`Authority::System`], when there is nobody left to ask.
///
/// **An answer rather than a refusal**, which is the shape
/// [`crate::limit::binding`] has and for the same reason: the question is async
/// and a decision is not. Each caller applies it *inside* the decision, after
/// the retry check, so the retry of a credit note issued while the caller held
/// the claim answers with that document instead of refusing once the claim is
/// revoked.
///
/// **The branch is §68's**, exactly: a member who is not the owner.
/// [`Authority::System`] — a gateway's confirmed refund, a worker's sweep — is
/// not claim-judged, the same way it is not limit-judged, because there is
/// nobody to ask. The owner is exempt here rather than inside `hr::may`, which
/// reads the handle's role: a root has no handle, and `Authority::of` has
/// already read that same role off it.
///
/// `hr::may` answers the rest, and the first of its answers is what keeps this
/// opt-in: a tenant that has never granted **this** claim is permitted, so a
/// till that worked yesterday works today — and granting some other claim does
/// not change that.
async fn may_credit(
    conn: &mut sqlx::PgConnection,
    authority: Authority,
    metadata: &Metadata,
) -> Result<bool, ExecuteError<SalesError>> {
    let Authority::Member { owner: false } = authority else {
        return Ok(true);
    };
    hr::may(&mut *conn, APPROVE_CREDIT_NOTE, metadata, None)
        .await
        .map_err(ExecuteError::Database)
}

#[derive(Debug, thiserror::Error)]
pub enum SalesError {
    #[error("an invoice needs at least one line that comes to something")]
    NothingToInvoice,
    /// **Crediting an invoice is a claim, once this tenant grants it.**
    ///
    /// See §52: until 2026-09-09 this claim could be granted, displayed and
    /// relied on while nothing consulted it.
    #[error("issuing a credit note needs the {0} claim")]
    NotApproved(String),
    /// **More than this caller may issue in one document** — the limit the
    /// owner set at `/v1/sales/document-limit`. `amount` is the document's
    /// total on the limit's basis, and in another currency than the limit's
    /// when that is why. See `limit.rs`.
    #[error("{amount} is more than the {limit} one document may come to")]
    OverDocumentLimit { limit: Money, amount: Money },
    /// **A line that carries no tax must say why**, and only the tenant knows.
    ///
    /// The reason is a code from the tax authority's list, configured once at
    /// `PUT /v1/ledger/vat-rates`. Refused here rather than defaulted: this
    /// build used to state *financial services* for every exempt line, which
    /// was right for one kind of business and a false statement to a tax
    /// authority for every other.
    #[error(
        "a {} line needs a reason, and none is configured; set one at /v1/ledger/vat-rates",
        category.as_str()
    )]
    NoExemptionReason { category: ledger::VatCategory },
    #[error("invoice {0} has not been issued")]
    NotIssued(String),
    #[error("only {outstanding} is outstanding; the payment is {offered}")]
    Overpayment { outstanding: Money, offered: Money },
    #[error("the business holds only {held}; the refund is {offered}")]
    Overrefund { held: Money, offered: Money },
    /// **A tax document in a currency the jurisdiction does not take.** See
    /// [`DocumentCurrency`](crate::DocumentCurrency).
    #[error("tax documents here are issued in {expected}; this one is in {found}")]
    DocumentCurrency {
        expected: CurrencyCode,
        found: CurrencyCode,
    },
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
    /// A credit line saying units came back, against an invoice line that sold
    /// no product — an hour of consultancy, or a line issued before products
    /// existed. **Refused rather than ignored** (L6): there is no shelf for
    /// them to land on.
    #[error("line {line} of invoice {invoice} sold no product, so no units can come back on it")]
    NotAStockLine { invoice: String, line: u16 },
    /// A credit note against an invoice that has already been cancelled
    /// outright, or a cancellation of one that has been partly credited. Both
    /// would credit the same supply twice.
    #[error("invoice {0} has already been credited")]
    AlreadyCredited(String),
    #[error("a credit note must credit something")]
    NothingToCredit,
    /// A line priced per unit whose quantity is not one.
    #[error("a quantity is a whole number of units, and more than nothing")]
    NotAQuantity,
    /// A line that names units without saying what they are units of, or
    /// without a quantity of that many — on an invoice and on a credit note.
    ///
    /// **Refused rather than reconciled** (L6). A line naming three serials and
    /// charging for one would take three units off the shelf and print a
    /// document saying one went out, and neither number is safe to believe over
    /// the other.
    #[error("this line names {named} units; it must name the product and charge for {named}")]
    NamedUnits { named: i64 },
    /// A line naming a lot and no product. **Refused rather than dropped**
    /// (L6): nothing would take the units off that lot, and whoever named it
    /// was promised that batch.
    #[error("this line names lot {lot} and no product")]
    LotWithoutAProduct { lot: String },
    /// **What the shelf said**, carried whole. A product nobody declared, a
    /// serial that is not there, or a tracked product the shelf cannot cover
    /// (R1) — every one of them names the thing the person has to fix, and
    /// flattening them into "could not take the stock" would throw that away.
    #[error(transparent)]
    Stock(#[from] inventory::InventoryError),
    /// The prepayment invoice does not fit the supply it is deducted from.
    #[error(transparent)]
    Prepaid(#[from] crate::vat::PrepaidError),
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
            Self::NotApproved(claim) => {
                Message::new(messages::NOT_APPROVED).with("claim", MessageArg::text(claim.clone()))
            }
            Self::OverDocumentLimit { limit, amount } => {
                Message::new(if limit.currency() == amount.currency() {
                    messages::OVER_DOCUMENT_LIMIT
                } else {
                    messages::DOCUMENT_LIMIT_CURRENCY
                })
                .with("limit", MessageArg::text(limit.to_string()))
                .with("amount", MessageArg::text(amount.to_string()))
                .with(
                    "claim",
                    MessageArg::text(crate::limit::EXCEED_DOCUMENT_LIMIT.to_owned()),
                )
            }
            Self::NoExemptionReason { category } => Message::new(messages::NO_EXEMPTION_REASON)
                .with("category", MessageArg::text(category.as_str().to_owned())),
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
            Self::DocumentCurrency { expected, found } => Message::new(messages::DOCUMENT_CURRENCY)
                .with("expected", MessageArg::text(expected.to_string()))
                .with("found", MessageArg::text(found.to_string())),
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
            Self::NotAStockLine { invoice, line } => Message::new(messages::NOT_A_STOCK_LINE)
                .with("invoice", MessageArg::text(invoice.clone()))
                .with("line", MessageArg::Count(i64::from(*line))),
            Self::CreditTooLarge { amount } => Message::new(messages::CREDIT_TOO_LARGE)
                .with("amount", MessageArg::text(amount.to_string())),
            Self::AlreadyCredited(invoice) => Message::new(messages::ALREADY_CREDITED)
                .with("invoice", MessageArg::text(invoice.clone())),
            Self::NothingToCredit => Message::new(messages::NOTHING_TO_CREDIT),
            Self::NotAQuantity => Message::new(messages::NOT_A_QUANTITY),
            Self::NamedUnits { named } => {
                Message::new(messages::NAMED_UNITS).with("named", MessageArg::Count(*named))
            }
            Self::LotWithoutAProduct { lot } => Message::new(messages::LOT_WITHOUT_A_PRODUCT)
                .with("lot", MessageArg::text(lot.clone())),
            // Already says the right thing in both languages.
            Self::Stock(e) => e.message(),
            Self::Prepaid(why) => Message::new(messages::PREPAID_DOES_NOT_FIT)
                .with("why", MessageArg::text(why.to_string())),
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

impl SalesError {
    /// **A refusal of who is asking**, not of what was asked: a claim they do
    /// not hold, or a limit only the owner or a claim lifts. A 403 at every
    /// route a sales refusal surfaces from, so the sales routes, the till and
    /// the booking desk cannot answer the same refusal differently.
    #[must_use]
    pub const fn refuses_the_caller(&self) -> bool {
        matches!(self, Self::NotApproved(_) | Self::OverDocumentLimit { .. })
    }

    /// **A line of the wrong shape**, whichever document and whichever door
    /// carried it: a quantity that is not one, names that do not match their
    /// line, a lot named with no product, or a line the shelf cannot read
    /// ([`inventory::InventoryError::is_malformed`]). A 400 at the sales routes
    /// and at the till alike; deciding it once is what keeps the same refusal
    /// from being two statuses depending on which door raised it.
    #[must_use]
    pub const fn is_malformed(&self) -> bool {
        match self {
            Self::NotAQuantity | Self::NamedUnits { .. } | Self::LotWithoutAProduct { .. } => true,
            Self::Stock(stock) => stock.is_malformed(),
            _ => false,
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
    /// **The other end of that**: the final invoice for a supply a deposit was
    /// taken on. The lines are the whole supply; what the prepayment invoice
    /// already declared comes off, band by band, and this document charges
    /// and declares only the rest. See [`crate::Prepaid`].
    pub prepaid: Option<crate::vat::Prepaid>,
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
    authority: Authority,
) -> NumberedOutcome {
    if draft.lines.is_empty() {
        return Err(rejected(SalesError::NothingToInvoice));
    }

    let memo = format!("Invoice {id} · {}", draft.customer.name);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match issue_in(&mut tx, id, draft, &memo, metadata, authority).await {
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

/// **The currency the tax authority takes documents in**, when this tenant is
/// under one — see [`DocumentCurrency`](crate::DocumentCurrency). Read in the
/// issuing transaction with everything else the document is judged by, so a
/// rule set a moment ago refuses the next invoice and not the one after it.
async fn in_the_document_currency(
    conn: &mut sqlx::PgConnection,
    currency: CurrencyCode,
) -> Result<(), ExecuteError<SalesError>> {
    if let Some(required) = crate::DocumentCurrency::resolve(conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?
        && required.currency != currency
    {
        return Err(ExecuteError::Rejected(SalesError::DocumentCurrency {
            expected: required.currency,
            found: currency,
        }));
    }
    Ok(())
}

/// One attempt at issuing: the invoice event and its journal entry, in the
/// caller's transaction.
///
/// **Public because a till composes it.** `pos` writes a shift's own event, this
/// invoice and its payment in one transaction, for the same reason this module
/// calls `ledger::post_entry_in` rather than posting a moment later: a sale that
/// exists in one place and not the other is a state nobody could explain.
///
/// `authority` is who is issuing it, for the tenant's document limit — see
/// [`DocumentLimit`](crate::DocumentLimit). Every invoice this system issues
/// passes through here, so this is where that limit is judged.
pub async fn issue_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    draft: &Draft,
    memo: &str,
    metadata: &Metadata,
    authority: Authority,
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

    in_the_document_currency(&mut *conn, draft.currency).await?;

    // **The rate, in this transaction too.** It used to be a constant the API
    // handler stamped onto each line before the command ran; it is now the
    // tenant's, and reading it here is what stops an invoice carrying a rate
    // that was never current — the same argument as the accounts below.
    let rates = ledger::Rates::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(SalesError::Config(e)))?;

    let lines = priced_lines(&draft.lines, &rates)?;

    // The rate comes from the same configuration the lines' does, so a discount
    // on a standard-rated invoice reduces the tax at the rate that invoice was
    // stamped with.
    let discounts: Vec<InvoiceDiscount> = draft
        .discounts
        .iter()
        .map(|discount| InvoiceDiscount {
            reason: discount.reason.clone(),
            amount: discount.amount,
            vat: crate::vat::Vat::at(&rates, discount.category),
        })
        .collect();

    let totals = crate::vat::total(
        lines.iter().map(|l| (l.vat.clone(), l.net)),
        discounts.iter().map(|d| (d.vat.clone(), d.amount)),
        draft.currency,
    )
    .map_err(|e| ExecuteError::Rejected(SalesError::Tax(e)))?;
    // **What this document charges.** The whole supply, less what a
    // prepayment invoice already billed and declared for it.
    let totals = match &draft.prepaid {
        Some(prepaid) => totals
            .less(prepaid)
            .map_err(|e| ExecuteError::Rejected(SalesError::Prepaid(e)))?,
        None => totals,
    };
    let totals = &totals;
    // **Only now, with the total this document charges.** Compared inside the
    // decision below, so a retry of an invoice issued before a limit was
    // lowered answers with its number rather than a refusal.
    let limit = crate::limit::binding(&mut *conn, authority, metadata).await?;

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
            if let Some(limit) = limit {
                limit.judge(totals.net, totals.gross)?;
            }
            Ok(Decision::one(InvoiceEvent::Issued {
                number: Some(number.clone()),
                prepayment: draft.prepayment,
                prepaid: draft.prepaid.clone(),
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

    deplete(conn, id, &committed, draft.issued_on, &metadata).await?;

    Ok(Numbered { committed, number })
}

/// **Takes off the shelf what this invoice sold**, in the invoice's own
/// transaction.
///
/// Every invoice this system issues goes through [`issue_in`], so this is the
/// one place a document depletes stock: a till sale, a booking bill and a
/// `/v1/sales` invoice all reach it (decision 2). Cost of goods sold posts
/// there too, lot by lot, in `inventory`.
///
/// # Sorted by product
///
/// Decision 14, and the same argument the counter-before-stream comment above
/// makes: two tills selling the same two products in opposite orders would take
/// the two shelves' locks the other way round and deadlock. One order, fixed
/// here, and two lines naming one product still take that shelf once each — in
/// line order, which their references already keep apart.
///
/// # Never on a retry
///
/// **The lines come off the `Issued` event this transaction wrote**, the way
/// `came_back` reads the `Credited` one, so a retried invoice — which writes no
/// event — depletes nothing by construction. The invoice is idempotent for
/// ever; the shelf's own retry check is a bounded window, and a review found a
/// retry reaching the shelf after that window had rolled took the stock a
/// second time while the cost entry, whose id is derived, posted nothing.
/// Nothing needs healing: no invoice has been written without its depletion,
/// because both are one transaction. The movement reference is still derived
/// from the invoice and the line's position (L8), which is what lets a credit
/// note name the consumption it is undoing.
async fn deplete(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    committed: &Committed<InvoiceEvent>,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    let Some(InvoiceEvent::Issued { lines, .. }) = committed.events.first() else {
        return Ok(());
    };
    let mut sold: Vec<(usize, &InvoiceLine)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.product.is_some())
        .collect();
    if sold.is_empty() {
        return Ok(());
    }
    sold.sort_by(|(a, one), (b, two)| one.product.cmp(&two.product).then_with(|| a.cmp(b)));

    for (at_line, line) in sold {
        let Some(product) = &line.product else {
            continue;
        };
        inventory::consume_in(
            &mut *conn,
            product,
            &inventory::Consumption {
                // **Named units or a quantity, never both** — that is
                // `inventory`'s own shape, and a serial-tracked line always
                // names them. A line with a product and no quantity is one
                // unit, the same reading the document takes of
                // `cbc:InvoicedQuantity`.
                quantity: line.serials.is_empty().then(|| line.quantity.unwrap_or(1)),
                // The lot the line named, overriding the picking rule. A lot
                // that is not open on this shelf, or is short, refuses.
                lot: line.lot.clone(),
                serials: line.serials.clone(),
                reference: line_reference(invoice, at_line),
                at,
            },
            metadata,
        )
        .await
        .map_err(lift_stock)?;
    }
    Ok(())
}

/// **The movement reference one invoice line's stock moves under.**
///
/// Derived from the document and the line's position, never minted (L8), which
/// is what makes a retried invoice deplete once and a credit note able to name
/// the consumption it is undoing.
fn line_reference(invoice: &AggregateId, line: usize) -> String {
    format!("{invoice}.{line}")
}

/// The same for what a credit note puts back. **Its own key and not the
/// sale's**: keyed on the consumption's reference, the shelf would hear a retry
/// of the sale and put nothing back.
///
/// # The credit note's *number*, not the client's reference
///
/// A client reference is only unique per invoice — `has_credit` sits on the
/// invoice, and the credit entry's id carries the invoice beside it. Two credit
/// notes called `RET-1` on two invoices of the same product would share a
/// return reference on one shelf, and the second would be heard as a retry of
/// the first and put nothing back while its money posted in full. A review
/// found exactly that. The number is the tenant's own gapless series, so it
/// cannot repeat, and it is short enough that the entry id it names still fits
/// beside a shelf. Stock only moves in the transaction that issues the credit
/// note, so the number is final whenever this is called.
fn return_reference(credit_note_number: &str, line: usize) -> String {
    format!("r.{credit_note_number}.{line}")
}

/// What comes back, keyed by the invoice line's product and its position on the
/// invoice: how many units, or `None` for all of them, and which by name.
///
/// **A map, so one invoice line is one return.** Two lines of one credit note
/// may name the same invoice line, and as two entries they would share a return
/// reference and the second would be heard as a retry and put nothing back.
/// Keyed by product first, so iterating it is the sorted order [`deplete`]
/// takes shelves in.
type ComingBack = std::collections::BTreeMap<(AggregateId, usize), (Option<i64>, Vec<String>)>;

/// **Puts back what a credit note says came back**, one invoice line at a time.
///
/// **Never a share of what was credited** (decision 12): the amount on a credit
/// line and the units on it are two different statements, and dividing one into
/// the other is the guess L6 refuses.
///
/// # Onto the shelf the sale took it from
///
/// A shelf is a product at a branch, and the branch is the one the invoice was
/// **issued** at — read off the `Issued` event's metadata, because a credit
/// note raised at head office, or at another till, is still undoing that sale.
/// Asked of this request's branch, it would look for the consumption on a shelf
/// that never saw it and refuse the cancellation.
async fn restore(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    credit_note_number: &str,
    back: &ComingBack,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    if back.is_empty() {
        return Ok(());
    }
    let stream = erp_types::StreamId::new(
        <Invoice as erp_eventlog::Aggregate>::domain(),
        invoice.clone(),
    );
    let branch = erp_eventlog::read_stream(&mut *conn, &stream)
        .await
        .map_err(|e| ExecuteError::Load(e.into()))?
        .into_iter()
        .find(|event| event.event_name.as_str() == InvoiceEvent::NAMES[0])
        .and_then(|issued| issued.metadata.branch().map(str::to_owned));

    for ((product, line), (quantity, serials)) in back {
        inventory::restore_in(
            &mut *conn,
            product,
            &inventory::Restoration {
                taken_on: line_reference(invoice, *line),
                branch: branch.clone(),
                quantity: *quantity,
                serials: serials.clone(),
                reference: return_reference(credit_note_number, *line),
                at,
            },
            metadata,
        )
        .await
        .map_err(lift_stock)?;
    }
    Ok(())
}

/// Carries a stock failure into this module's error with its kind intact, so
/// the retry loops still recognise a conflict. The shape [`lift`] has.
fn lift_stock(error: ExecuteError<inventory::InventoryError>) -> ExecuteError<SalesError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(SalesError::Stock(e)),
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
/// **A partial refund gets a partial credit note only on a single-band
/// invoice**: how an arbitrary amount divides across a standard-rated line and
/// a zero-rated one is not something this system may guess. Which document a
/// refund leaves owing is [`credit_what_is_clear`]'s to decide.
pub async fn refund_invoice(
    db: &TenantDb,
    invoice: &AggregateId,
    receipt: &Receipt,
    reason: &str,
    metadata: &Metadata,
    authority: Authority,
) -> Outcome {
    if !receipt.amount.is_positive() {
        return Err(rejected(SalesError::NotAPayment));
    }
    let memo = format!("Refund {} · invoice {invoice}", receipt.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let refunded = async {
            let committed =
                refund_in(&mut tx, invoice, receipt, &memo, metadata, authority).await?;
            credit_what_is_clear(
                &mut tx,
                invoice,
                &receipt.reference,
                receipt.amount,
                reason,
                receipt.received_on,
                metadata,
                authority,
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

/// **Refuses a refund this caller may not hand back**, before anybody is
/// asked to hand it back.
///
/// For the refund a gateway carries out: the member asks, the worker tells the
/// gateway, and [`refund_in`] is written from what the gateway confirms — by
/// then the money has gone, and refusing to record it, or the credit note it
/// implies, would only make the books wrong. So the member is judged when they
/// ask, on what the refund at `/v1/sales` would be judged on: the money, as
/// [`refund_in`] judges it, and the **whole-invoice credit note** a refund that
/// clears the invoice issues, by the same `owed` that decides which credit
/// note [`credit_what_is_clear`] issues. A partial credit note credits exactly
/// what went back, so the money covers it.
///
/// Judged on the invoice as it stands when they ask. A refund or credit note
/// that lands between the asking and the gateway's answer can change which
/// credit note the answer issues, and that one is not judged again.
///
/// **The limit and the claim, both.** The credit note the gateway's answer
/// leaves owing is written by [`credit_what_is_clear`] with
/// [`Authority::System`], which no claim judges — so [`APPROVE_CREDIT_NOTE`] is
/// asked for *here*, while there is still somebody to ask, exactly as the limit
/// is. Whole or part: a partial credit note is still a credit note, and the
/// money covering it answers the limit's question rather than this one's.
///
/// **Here rather than beside the rest of the limit** (`limit.rs`) because it
/// loads the aggregate, and L7 keeps that to command handling — which this is:
/// it runs inside `payments::request_refund_in`'s transaction, and decides
/// from history what that write may do.
///
/// # Errors
/// [`SalesError::OverDocumentLimit`], [`SalesError::NotApproved`], or whatever
/// reading the invoice raised.
pub async fn may_refund(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    refunded: Money,
    authority: Authority,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    let limit = crate::limit::binding(&mut *conn, authority, metadata).await?;
    let approved = may_credit(&mut *conn, authority, metadata).await?;
    // Nothing to judge, so nothing to read: the invoice is loaded here only to
    // answer one of those two.
    if limit.is_none() && approved {
        return Ok(());
    }
    let state = erp_eventlog::load::<Invoice>(&mut *conn, invoice, crate::upcasters())
        .await?
        .aggregate;
    let judged = || {
        if let Some(limit) = limit {
            limit.judge_refund(&state, invoice.as_str(), refunded)?;
        }
        let held = state
            .held()
            .ok_or_else(|| SalesError::NotIssued(invoice.as_str().to_owned()))?
            .checked_sub(refunded)
            .map_err(|e| SalesError::Tax(e.into()))?;
        let owed = owed(&state, held, refunded);
        // **The credit note this refund will leave owing**, asked for now
        // because the gateway's answer issues it with `System` and there is
        // nobody to ask by then. Whole or part: both are credit notes.
        if !approved && matches!(owed, Owed::Whole | Owed::Part(_)) {
            return Err(SalesError::NotApproved(APPROVE_CREDIT_NOTE.to_owned()));
        }
        match owed {
            // The whole-invoice credit note that clearing it issues, which
            // the line above does not cover: that one judges the money.
            Owed::Whole => {
                limit.map_or(Ok(()), |limit| limit.judge_whole(&state, invoice.as_str()))
            }
            // A partial credit note credits exactly what went back, which is
            // what was just judged, and no credit note is nothing to judge.
            Owed::Nothing | Owed::Part(_) | Owed::Overstated(_) => Ok(()),
        }
    };
    judged().map_err(ExecuteError::Rejected)
}

/// Credits what a refund undid: the whole invoice when it now holds nothing,
/// and **the refunded part when it still holds some** and the invoice has one
/// tax band. Which of those is `owed`'s to say, from the invoice as the
/// refund left it; this carries it out.
///
/// `refunded` is what went back under `reference`, tax included; the credit
/// note is keyed on the same reference, so a retried refund credits once.
///
/// Not folded into [`refund_in`], deliberately. That one is a per-money-movement
/// primitive — a till calls it once per tender — and a credit note is per
/// document. Crediting there would issue one against a single tender's
/// reference and try again for every other.
///
/// `authority` is the refund's, and the credit note it issues is judged like
/// any other: against the document limit, so a refund that clears an invoice is
/// refused when the whole-invoice credit note would be over it, and — since
/// §70 — for [`APPROVE_CREDIT_NOTE`], so a member who may not issue a credit
/// note may not refund their way to one either. A refund a gateway makes passes
/// `System` here and is judged for neither; its member was judged on the same
/// `owed` when they asked, for the limit — see [`may_refund`].
#[expect(
    clippy::too_many_arguments,
    reason = "every one is a fact about the refund that already happened"
)]
pub async fn credit_what_is_clear(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    reference: &str,
    refunded: Money,
    reason: &str,
    on: Timestamp,
    metadata: &Metadata,
    authority: Authority,
) -> Result<(), ExecuteError<SalesError>> {
    let state = erp_eventlog::load::<Invoice>(&mut *conn, invoice, crate::upcasters())
        .await?
        .aggregate;
    let held = state.held().ok_or_else(|| {
        ExecuteError::Rejected(SalesError::NotIssued(invoice.as_str().to_owned()))
    })?;
    let net = match owed(&state, held, refunded) {
        Owed::Nothing => return Ok(()),
        Owed::Whole => {
            return credit_in(
                &mut *conn, invoice, reference, reason, on, metadata, authority,
            )
            .await
            .map(|_| ());
        }
        Owed::Overstated(why) => {
            tracing::warn!(%invoice, %reference, %refunded, bands = state.bands.len(), "{why}");
            return Ok(());
        }
        Owed::Part(net) => net,
    };
    let lines = spread_over_lines(&state, net);

    match credit_part_in(
        &mut *conn,
        invoice,
        &CreditNote {
            reference: reference.to_owned(),
            lines,
            reason: reason.to_owned(),
            on,
        },
        metadata,
        authority,
    )
    .await
    {
        Ok(_) => Ok(()),
        // More than is left to credit — refunds after a credit note raised by
        // hand — or nothing to spread the net over. The refund stands; the
        // document does not follow, and the log says which.
        Err(ExecuteError::Rejected(
            SalesError::CreditTooLarge { .. } | SalesError::NothingToCredit,
        )) => {
            tracing::warn!(
                %invoice,
                %reference,
                %refunded,
                "a partial refund could not be credited against what is left of the invoice"
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The credit note a refund leaves an invoice owing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owed {
    /// None: the invoice already has its whole credit note.
    Nothing,
    /// A whole cancellation: it holds nothing, and no part is credited yet.
    Whole,
    /// A partial credit note for this net, which taxed at the invoice's one
    /// rate comes to exactly the refund.
    Part(Money),
    /// A part with no honest way to write it. The invoice stays overstated by
    /// the refund, and this says why.
    Overstated(&'static str),
}

/// **Which credit note a refund of `refunded` leaves this invoice owing**,
/// when it holds `held` once the refund is recorded.
///
/// One decision in two places. [`credit_what_is_clear`] carries it out after a
/// refund; `limit::may_refund` judges it before a gateway is asked for one,
/// because the credit note a gateway refund implies is issued once the money
/// has gone, with nobody acting. Two copies of this question drifted once
/// already: the first `may_refund` judged the money and not the whole-invoice
/// credit note clearing an invoice issues.
///
/// **Only a single-band invoice gets a part.** A credit note for part of an
/// invoice carries bands of its own, and how an arbitrary refund divides
/// across a standard-rated line and a zero-rated one is not something this
/// system may guess. With one band there is one answer: the net that, taxed at
/// that band's rate, comes to what went back. Every deposit is such an
/// invoice.
pub(crate) fn owed(state: &Invoice, held: Money, refunded: Money) -> Owed {
    if state.cancelled_by.is_some() {
        return Owed::Nothing;
    }
    // What [`cancel_in`] requires, read the same way: a cancellation reverses
    // the whole issue entry, so not after a part of it has been credited, and
    // not while the business still holds money for it.
    if held.is_zero() && !state.is_partly_credited() {
        return Owed::Whole;
    }
    let [band] = state.bands.as_slice() else {
        return Owed::Overstated(
            "a partial refund of a multi-band invoice gets no credit note; the document is overstated by the refund",
        );
    };
    net_of_gross(refunded, band.basis_points).map_or(
        Owed::Overstated(
            "no net at this band's rate comes to exactly what was refunded; no credit note",
        ),
        Owed::Part,
    )
}

/// The net that, taxed at `basis_points` the way every invoice is, comes to
/// exactly `gross` — or none, when no net lands on it.
///
/// Dividing a gross by a rate does not always land: at 15% no net comes to
/// exactly 10.00. So the candidates around the division are checked forwards,
/// with the same rounding the invoice used, rather than trusted backwards.
fn net_of_gross(gross: Money, basis_points: i32) -> Option<Money> {
    let currency = gross.currency();
    let rate = i64::from(basis_points);
    let base = gross.minor().checked_mul(10_000)? / (10_000 + rate);
    [base - 1, base, base + 1]
        .into_iter()
        .filter(|candidate| *candidate > 0)
        .map(|candidate| Money::from_minor(candidate, currency))
        .find(|net| {
            net.scaled_by(basis_points)
                .and_then(|tax| net.checked_add(tax))
                .is_ok_and(|comes_to| comes_to == gross)
        })
}

/// Spreads a net across the invoice's lines, in order, each up to what is left
/// of it to credit. One band means every line credits at the same rate, so
/// which line takes it changes nothing but the description.
fn spread_over_lines(state: &Invoice, net: Money) -> Vec<CreditLine> {
    let mut left = net.minor();
    let mut lines = Vec::new();
    for (index, line) in state.lines.iter().enumerate() {
        if left <= 0 {
            break;
        }
        let credited = state
            .credited_lines
            .get(index)
            .map_or(0, |money| money.minor());
        let room = line.net.minor() - credited;
        if room <= 0 {
            continue;
        }
        let part = room.min(left);
        let Ok(against) = u16::try_from(index) else {
            break;
        };
        lines.push(CreditLine {
            against,
            net: Money::from_minor(part, net.currency()),
            // **Nothing comes back on the shelf.** This spreads a *refund* over
            // the lines, and money handed back says nothing about goods handed
            // back — deriving units from an amount is exactly decision 12's
            // "never a proportion". A customer returning the goods raises the
            // credit note that says so.
            quantity: None,
            serials: Vec::new(),
        });
        left -= part;
    }
    lines
}

/// One attempt at refunding, in the caller's transaction. Public for the reason
/// [`issue_in`] is: a till hands the money back in the same write that credits
/// the sale.
///
/// **Every refund is judged against the document limit here**, on what goes
/// back — a till's once per tender, and its credit note once for the return.
pub async fn refund_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    receipt: &Receipt,
    memo: &str,
    metadata: &Metadata,
    authority: Authority,
) -> Result<Committed<InvoiceEvent>, ExecuteError<SalesError>> {
    let entry_id = &money_entry("sr", invoice, &receipt.reference)?;
    let limit = crate::limit::binding(&mut *conn, authority, metadata).await?;
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
            if let Some(limit) = limit {
                limit.judge_refund(state, invoice.as_str(), receipt.amount)?;
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
    authority: Authority,
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
            authority,
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
    authority: Authority,
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
        authority,
    )
    .await
}

/// One attempt at crediting: the ledger reversal and the invoice's own event,
/// in the caller's transaction. The root of every whole-invoice credit note, so
/// where one is judged against the document limit — on the whole invoice, which
/// is what it credits — and where a member is asked for
/// [`APPROVE_CREDIT_NOTE`].
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
    authority: Authority,
) -> Result<Numbered, ExecuteError<SalesError>> {
    let reference = credit_note.to_owned();
    let reason = reason.trim().to_owned();
    let mut already = false;
    // **Who is asking, before the decision.** Both are async and a decision is
    // not, and both answers are the same on every optimistic-concurrency
    // attempt. Both are applied *inside* it, after the retry check.
    let approved = may_credit(&mut *conn, authority, metadata).await?;
    let limit = crate::limit::binding(&mut *conn, authority, metadata).await?;

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
            // **After the retry**, where the limit's comparison also sits: a
            // credit note issued while the caller held the claim is still their
            // document when the client resends its reference, claim or no
            // claim.
            if !approved {
                return Err(SalesError::NotApproved(APPROVE_CREDIT_NOTE.to_owned()));
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
                // Only once it would be issued: an invoice still holding
                // money is refused for that, not for its size.
                if let Some(limit) = limit {
                    limit.judge_whole(state, invoice.as_str())?;
                }
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
        ledger::reverse_in(&mut *conn, entry_id, credit_id, on, memo, metadata)
            .await
            .map_err(lift)?;
        // **The whole supply is undone, so the whole of it comes back.** Not a
        // proportion of anything: the quantity on each line is the one it was
        // sold at, which is what the line itself records. Read back off the
        // aggregate, because the decision above returns the event and not the
        // state it was taken from — one load, on the path that has already
        // reversed a journal entry.
        let sold = erp_eventlog::load::<Invoice>(&mut *conn, invoice, crate::upcasters())
            .await?
            .aggregate
            .lines;
        let back: ComingBack = sold
            .iter()
            .enumerate()
            .filter_map(|(at, line)| Some(((line.product.clone()?, at), (None, Vec::new()))))
            .collect();
        restore(&mut *conn, invoice, &credit_note, &back, on, metadata).await?;
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
    rates: &ledger::Rates,
) -> Result<Vec<InvoiceLine>, ExecuteError<SalesError>> {
    draft
        .iter()
        .map(|line| {
            // **L6, before anything is written.** A category that carries no
            // tax needs the authority's article, and a document that cannot
            // say why is refused at issue rather than rendered with a guess.
            if line.category != ledger::VatCategory::Standard
                && rates.reason(line.category).is_none()
            {
                return Err(ExecuteError::Rejected(SalesError::NoExemptionReason {
                    category: line.category,
                }));
            }
            if line.allowances.iter().any(|a| !a.amount.is_positive()) {
                // A negative allowance is a surcharge, which is a different
                // element and a different conversation.
                return Err(ExecuteError::Rejected(SalesError::Tax(
                    crate::vat::TaxError::NotADiscount,
                )));
            }
            // **Multiplied out here, and never divided back.** A line given as
            // a price and a quantity comes to their product; the total is what
            // the tax, the bands, the posting and the document are all built
            // from, and working the price back out of it is a division that
            // does not always land on a halala (decision 3).
            let quantity = match line.quantity {
                Some(quantity) if quantity <= 0 => {
                    return Err(ExecuteError::Rejected(SalesError::NotAQuantity));
                }
                other => other,
            };
            // **A line that names units says how many, and of what.** Without
            // the quantity the document would print one unit while three left
            // the shelf; without the product nothing would take them off it at
            // all, and a serial silently dropped is how a phone leaves the shop
            // with no record of which one.
            if !line.serials.is_empty() {
                let named = i64::try_from(line.serials.len()).unwrap_or(i64::MAX);
                if line.product.is_none() || quantity != Some(named) {
                    return Err(ExecuteError::Rejected(SalesError::NamedUnits { named }));
                }
            }
            if let (Some(lot), None) = (&line.lot, &line.product) {
                return Err(ExecuteError::Rejected(SalesError::LotWithoutAProduct {
                    lot: lot.clone(),
                }));
            }
            let charged = match quantity {
                Some(quantity) => line
                    .net
                    .checked_mul_int(quantity)
                    .map_err(|e| ExecuteError::Rejected(SalesError::Tax(e.into())))?,
                None => line.net,
            };
            let net = line
                .allowances
                .iter()
                .try_fold(charged, |running, a| running.checked_sub(a.amount))
                .map_err(|e| ExecuteError::Rejected(SalesError::Tax(e.into())))?;
            if !line.allowances.is_empty() && !net.is_positive() {
                return Err(ExecuteError::Rejected(SalesError::Tax(
                    crate::vat::TaxError::DiscountTooLarge,
                )));
            }
            Ok(InvoiceLine {
                description: line.description.clone(),
                product: line.product.clone(),
                quantity,
                unit: quantity.map(|_| line.net),
                serials: line.serials.clone(),
                lot: line.lot.clone(),
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
        assert_send(issue_invoice(db, id, draft, metadata, Authority::of(db)));
        assert_send(record_payment(db, id, receipt, metadata));
        assert_send(cancel_invoice(
            db,
            id,
            "",
            "",
            erp_types::Timestamp::UNIX_EPOCH,
            metadata,
            Authority::of(db),
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
    /// **How many units came back onto the shelf**, when the line sold stock.
    ///
    /// Sent by the client and never derived from [`Self::net`] (decision 12):
    /// the money and the goods are two statements, and a partial credit is as
    /// often a price adjustment as a returned carton. `None` puts nothing back,
    /// which is what a goodwill credit means and what a refund spread across
    /// lines by `credit_what_is_clear` means — it knows the money and nothing
    /// about the goods.
    pub quantity: Option<i64>,
    /// **Which units came back**, on a line that sold named units: one name
    /// per unit, as many as [`Self::quantity`]. Each has to be one this
    /// invoice's line sold and that has not come back already — `inventory`
    /// follows the sale through its shelf's stream to say so. Empty returns
    /// by quantity, which for named units only the whole line can.
    pub serials: Vec<String>,
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
    authority: Authority,
) -> NumberedOutcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match credit_part_in(&mut tx, invoice, note, metadata, authority).await {
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
/// policy that keeps half a deposit credits and refunds in one write. The root
/// of every partial credit note, and so where one is judged against the
/// document limit, on its own totals, and where a member is asked for
/// [`APPROVE_CREDIT_NOTE`].
pub async fn credit_part_in(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    note: &CreditNote,
    metadata: &Metadata,
    authority: Authority,
) -> Result<Numbered, ExecuteError<SalesError>> {
    if note.lines.is_empty() {
        return Err(ExecuteError::Rejected(SalesError::NothingToCredit));
    }
    // Both before the number is reserved, both the same on every attempt, and
    // both applied inside the decision below, after the retry check.
    let approved = may_credit(&mut *conn, authority, metadata).await?;
    let limit = crate::limit::binding(&mut *conn, authority, metadata).await?;
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
            // After the retry, for the reason [`cancel_in`] gives.
            if !approved {
                return Err(SalesError::NotApproved(APPROVE_CREDIT_NOTE.to_owned()));
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
                lines.iter().map(|l| (l.line.vat.clone(), l.line.net)),
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
            if let Some(limit) = limit {
                limit.judge(totals.net, totals.gross)?;
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

    came_back(&mut *conn, invoice, note, &number, &committed, &metadata).await?;

    Ok(Numbered { committed, number })
}

/// **What a partial credit note put back on the shelf.**
///
/// The product is the *invoice's*, carried onto the credited line by
/// [`priced_for_credit`], so a credit note cannot return something the invoice
/// never sold. The quantity is the caller's, and a line that gives none puts
/// nothing back — which is what a goodwill credit and a refund spread over
/// lines both mean (decision 12).
///
/// **Two lines against one invoice line add up**: the credit note as a whole
/// says that many came back, and which. `priced_for_credit` has already refused
/// a quantity that is not one, one against a line that sold no product, and
/// names that do not agree with it.
async fn came_back(
    conn: &mut sqlx::PgConnection,
    invoice: &AggregateId,
    note: &CreditNote,
    number: &str,
    committed: &Committed<InvoiceEvent>,
    metadata: &Metadata,
) -> Result<(), ExecuteError<SalesError>> {
    let Some(InvoiceEvent::Credited { lines, .. }) = committed.events.first() else {
        return Ok(());
    };
    let mut back = ComingBack::new();
    for (credited, asked) in lines.iter().zip(&note.lines) {
        let (Some(product), Some(quantity)) = (&credited.line.product, asked.quantity) else {
            continue;
        };
        let (units, named) = back
            .entry((product.clone(), usize::from(credited.against)))
            .or_insert((Some(0), Vec::new()));
        *units = units
            .and_then(|so_far| so_far.checked_add(quantity))
            .map(Some)
            .ok_or(ExecuteError::Rejected(SalesError::NotAQuantity))?;
        named.extend(asked.serials.iter().cloned());
    }
    restore(conn, invoice, number, &back, note.on, metadata).await
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
            // **Units that came back are refused, not dropped, when they cannot
            // go anywhere** (L6). A quantity of nothing is not one, and units
            // against a line that sold no product have no shelf to land on —
            // silently ignoring either tells the client stock came back when
            // none did.
            if let Some(quantity) = line.quantity {
                if quantity <= 0 {
                    return Err(SalesError::NotAQuantity);
                }
                if against.product.is_none() {
                    return Err(SalesError::NotAStockLine {
                        invoice: invoice.as_str().to_owned(),
                        line: line.against,
                    });
                }
            }
            // **Names come with their count**, the rule a draft line follows:
            // three names and a quantity of two are two statements that
            // disagree, and names with no quantity would put nothing back.
            if !line.serials.is_empty() {
                let named = i64::try_from(line.serials.len()).unwrap_or(i64::MAX);
                if line.quantity != Some(named) {
                    return Err(SalesError::NamedUnits { named });
                }
            }

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
                    // **The invoice's product**, so a credit note cannot put
                    // back something the invoice never sold. The client sends
                    // how many, not what.
                    product: against.product.clone(),
                    // **Not the invoice's quantity or unit price.** A credit
                    // note credits an *amount* off a line, and stating a
                    // quantity beside it would make the document claim
                    // `net = quantity × price` for a division that need not
                    // land. What came back is on the shelf's own movement.
                    quantity: None,
                    unit: None,
                    serials: Vec::new(),
                    lot: None,
                    net: line.net,
                    // And the invoice's rate: one issued at 5% is credited at
                    // 5% for ever (L5).
                    vat: against.vat.clone(),
                    // **Stated at what is coming back.** The invoice's own
                    // allowances are what made this line smaller in the first
                    // place; they are not taken off a second time.
                    allowances: Vec::new(),
                },
            })
        })
        .collect()
}
