//! What a caller can ask the counter to do.
//!
//! # A sale is three writes and one transaction
//!
//! `sales::issue_in` for the invoice and its journal entry, `sales::pay_in` for
//! each tender, and this module's own `Sold` event for the drawer — all inside
//! one `db.begin()`. The same reason `sales` posts to `ledger` inline rather
//! than a moment later: a till transaction that exists as an invoice but not as
//! takings, or as takings but not as an invoice, is a state nobody could explain
//! and nothing would clean up.
//!
//! # The tenders must come to exactly the sale
//!
//! Not less, because a till sale that leaves a balance is an invoice on credit
//! and not a till sale. Not more, because `sales` refuses an overpayment — and
//! it is right to: change handed back is a counter concern, not a record. A
//! customer who hands over fifty riyals for a forty-three riyal basket is
//! recorded as forty-three, which is what the drawer actually gains.

use erp_eventlog::{
    Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, try_create, try_execute,
};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, CurrencyCode, Money, StreamId, Timestamp};

use crate::posting::{PostingAccounts, entry_for_pay_out, entry_for_variance};
use crate::shift::{Shift, ShiftEvent, Tender};

#[derive(Debug, thiserror::Error)]
pub enum PosError {
    #[error("there is no shift {0}")]
    NoSuchShift(String),
    #[error("shift {0} has been closed")]
    Closed(String),
    #[error("a float cannot be negative")]
    NotAFloat,
    #[error("a sale needs something on it")]
    NothingSold,
    #[error("the tenders come to {tendered}, and the sale is {total}")]
    TendersDoNotMatch { tendered: String, total: String },
    #[error("an amount here must be more than nothing")]
    NotAnAmount,
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error(transparent)]
    Money(#[from] erp_types::MoneyError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    /// The sale itself was refused — an unknown customer, a rate that is not
    /// one, a closed period. Passed through rather than reworded: `sales` says
    /// what is wrong with an invoice better than this module could.
    #[error(transparent)]
    Sale(#[from] sales::SalesError),
    #[error(transparent)]
    Ledger(#[from] ledger::LedgerError),
}

impl erp_i18n::Localize for PosError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchShift(id) => {
                Message::new(messages::NO_SUCH_SHIFT).with("id", MessageArg::text(id))
            }
            Self::Closed(id) => Message::new(messages::CLOSED).with("id", MessageArg::text(id)),
            Self::NotAFloat => Message::new(messages::NOT_A_FLOAT),
            Self::NothingSold => Message::new(messages::NOTHING_SOLD),
            Self::TendersDoNotMatch { tendered, total } => {
                Message::new(messages::TENDERS_DO_NOT_MATCH)
                    .with("tendered", MessageArg::text(tendered))
                    .with("total", MessageArg::text(total))
            }
            Self::NotAnAmount => Message::new(messages::NOT_AN_AMOUNT),
            Self::InvalidReference(r) => {
                Message::new(messages::NO_SUCH_SHIFT).with("id", MessageArg::text(r))
            }
            Self::Money(_) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
            // Each already says the right thing in both languages.
            Self::Unbalanced(e) => e.message(),
            Self::Config(e) => e.message(),
            Self::Sale(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Refusal = CommandError<PosError>;
type Outcome = Result<Committed<ShiftEvent>, Refusal>;

/// What a sale reports back: the shift's event, and the document `sales` made.
#[derive(Debug)]
pub struct Rung {
    pub committed: Committed<ShiftEvent>,
    /// The statutory invoice number the receipt prints.
    pub number: String,
    /// What the customer paid.
    pub total: Money,
}

/// Commits, rolls back and retries — the one place that decides which.
///
/// Written out at each command for the reason `booking` and `prepaid` write it
/// out: a generic `AsyncFn` helper reads better and does not compile, because
/// axum needs a handler's future to be `Send`.
async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<PosError>>,
) -> Result<Option<T>, Refusal> {
    match outcome {
        Ok(done) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            Ok(Some(done))
        }
        Err(e) if e.is_conflict() => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Ok(None)
        }
        Err(e) => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Err(e.into())
        }
    }
}

fn contended<T>(stream: &AggregateId) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: StreamId::new(<Shift as erp_eventlog::Aggregate>::domain(), stream.clone()),
        attempts: MAX_ATTEMPTS,
    }))
}

fn rejected(error: PosError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn derived(prefix: &str, parts: &[&str]) -> Result<AggregateId, PosError> {
    let joined = format!("{prefix}.{}", parts.join("."));
    AggregateId::new(&joined).map_err(|_| PosError::InvalidReference(parts.join(".")))
}

// -------------------------------------------------------------------- shifts

/// Opening a till.
#[derive(Debug, Clone)]
pub struct Opening {
    /// The business's own name for this counter.
    pub till: String,
    /// Who is on it. Staff, so an opaque identity and not a `crm` record.
    pub operator: String,
    /// What is in the drawer before anything is sold.
    pub float: Money,
    pub at: Timestamp,
}

/// Opens a till.
///
/// **Nothing posts.** A float is cash moved from a safe to a drawer, and both
/// are `1000 Cash on hand`: the business is no richer for having moved it, so
/// there is no entry to make. It follows that a shift's `expected` — what the
/// drawer should physically hold — is a larger number than what the shift added
/// to the ledger, and that the two are answers to different questions. The
/// variance is what reconciles them, and it is the only one of the three that
/// posts.
pub async fn open_shift(
    db: &TenantDb,
    id: &AggregateId,
    opening: &Opening,
    metadata: &Metadata,
) -> Outcome {
    if opening.float.is_negative() {
        return Err(rejected(PosError::NotAFloat));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let committed = try_create::<Shift, _, PosError>(
                &mut tx,
                id,
                crate::upcasters(),
                metadata,
                |_loaded: &Loaded<Shift>| {
                    Ok(Decision::one(ShiftEvent::Opened {
                        till: opening.till.clone(),
                        operator: opening.operator.clone(),
                        float: opening.float,
                        at: opening.at,
                    }))
                },
            )
            .await?;
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Everything a till sale is.
#[derive(Debug, Clone)]
pub struct Basket {
    /// Who it is for. A walk-in is a name and nothing else; **a VAT number
    /// makes it a standard invoice**, which ZATCA clears before the customer may
    /// be given it. `sales` decides that, and stays the only place it is decided.
    pub customer: sales::Customer,
    pub lines: Vec<sales::DraftLine>,
    pub discounts: Vec<sales::DraftDiscount>,
    pub currency: CurrencyCode,
    /// How it was paid for. Must come to exactly the sale; see the module docs.
    pub tenders: Vec<Tender>,
    pub note: String,
    pub at: Timestamp,
}

impl Basket {
    fn tendered(&self) -> Result<Money, PosError> {
        Money::checked_sum(self.tenders.iter().map(|t| t.amount), self.currency)
            .map_err(PosError::Money)
    }
}

/// Rings a sale: the invoice, its payment, and the drawer, in one transaction.
///
/// Idempotent under retry on `sale`. A repeat re-reports the number the document
/// was issued under and takes no capacity in the drawer twice, because
/// `sales::issue_in` recognises the retry and this shift remembers what it rang.
pub async fn sell(
    db: &TenantDb,
    shift: &AggregateId,
    sale: &AggregateId,
    basket: &Basket,
    metadata: &Metadata,
) -> Result<Rung, Refusal> {
    if basket.lines.is_empty() {
        return Err(rejected(PosError::NothingSold));
    }
    if basket.tenders.iter().any(|t| !t.amount.is_positive()) {
        return Err(rejected(PosError::NotAnAmount));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;

            // **Before the document exists.** A sale rung onto a shut till is
            // refused rather than issued and then unwound, and the check reads
            // where somebody looking for it would look.
            let held = erp_eventlog::load::<Shift>(&mut *conn, shift, crate::upcasters())
                .await
                .map_err(ExecuteError::Load)?;
            if !held.aggregate.exists() {
                return Err(ExecuteError::Rejected(PosError::NoSuchShift(
                    shift.to_string(),
                )));
            }
            if !held.aggregate.is_open() && !held.aggregate.has_sale(sale) {
                return Err(ExecuteError::Rejected(PosError::Closed(shift.to_string())));
            }

            let issued = sales::issue_in(
                &mut *conn,
                sale,
                &draft_from(basket),
                &format!("Till {} · sale {sale}", held.aggregate.till),
                metadata,
            )
            .await
            .map_err(lift)?;

            // **On a retry there is no fresh `Issued` to read.** `issue_in`
            // recognised the repeat and recorded nothing, so the total comes
            // from the document that already exists rather than from the
            // decision that did not happen.
            let total = match gross_of(&issued) {
                Some(total) => total,
                None => erp_eventlog::load::<sales::Invoice>(&mut *conn, sale, sales::upcasters())
                    .await
                    .map_err(ExecuteError::Load)?
                    .aggregate
                    .gross
                    .ok_or_else(|| {
                        ExecuteError::Rejected(PosError::Sale(sales::SalesError::NotIssued(
                            sale.to_string(),
                        )))
                    })?,
            };

            let tendered = basket.tendered().map_err(ExecuteError::Rejected)?;
            if tendered != total {
                return Err(ExecuteError::Rejected(PosError::TendersDoNotMatch {
                    tendered: tendered.minor().to_string(),
                    total: total.minor().to_string(),
                }));
            }

            take_the_money(&mut *conn, sale, basket, metadata).await?;

            let committed = try_execute::<Shift, _, PosError>(
                &mut *conn,
                shift,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Shift>| {
                    if loaded.aggregate.has_sale(sale) {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(ShiftEvent::Sold {
                        sale: sale.clone(),
                        total,
                        tenders: basket.tenders.clone(),
                        at: basket.at,
                    }))
                },
            )
            .await?;

            Ok(Rung {
                committed,
                number: issued.number.clone(),
                total,
            })
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(shift)
}

/// A sale handed back.
#[derive(Debug, Clone)]
pub struct Return {
    /// The caller's key. Returning the same one twice is a no-op (L8).
    pub reference: String,
    /// What the customer is given back, and how. Must come to the whole sale:
    /// this credits the document, and a partial credit note is not something
    /// `sales` can write.
    pub tenders: Vec<Tender>,
    pub why: String,
    pub at: Timestamp,
}

/// Takes a sale back: the credit note, the money, and the drawer, in one write.
///
/// # Why this needed a change to `sales` first
///
/// `cancel_invoice` refused any invoice that had ever been paid — and **every
/// till sale is paid the instant it happens**, so no till sale could be credited
/// through any route. The rule was not wrong so much as too blunt: what a credit
/// note may not do is undo a supply while the business keeps the cash. So
/// `sales` gained a refund, the rule became *"nothing is still held"*, and this
/// hands the money back and credits the document in the same transaction —
/// which is also the only order in which the books are never briefly wrong.
pub async fn take_back(
    db: &TenantDb,
    shift: &AggregateId,
    sale: &AggregateId,
    returning: &Return,
    metadata: &Metadata,
) -> Outcome {
    // A return that hands nothing back is not a return, and it is also the only
    // input for which the currency below is unanswerable.
    if returning.tenders.is_empty() || returning.tenders.iter().any(|t| !t.amount.is_positive()) {
        return Err(rejected(PosError::NotAnAmount));
    }
    let currency = returning.tenders[0].amount.currency();
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let held = erp_eventlog::load::<Shift>(&mut *conn, shift, crate::upcasters())
                .await
                .map_err(ExecuteError::Load)?;
            if !held.aggregate.exists() {
                return Err(ExecuteError::Rejected(PosError::NoSuchShift(
                    shift.to_string(),
                )));
            }

            let committed = try_execute::<Shift, _, PosError>(
                &mut *conn,
                shift,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Shift>| {
                    if loaded.aggregate.has_pay_out(&returning.reference) {
                        return Ok(Decision::nothing());
                    }
                    let total =
                        Money::checked_sum(returning.tenders.iter().map(|t| t.amount), currency)?;
                    Ok(Decision::one(ShiftEvent::Refunded {
                        sale: sale.clone(),
                        total,
                        tenders: returning.tenders.clone(),
                        why: returning.why.clone(),
                        at: returning.at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                give_the_money_back(&mut *conn, sale, returning, metadata).await?;
                sales::credit_in(
                    &mut *conn,
                    sale,
                    &returning.reference,
                    &returning.why,
                    returning.at,
                    metadata,
                )
                .await
                .map_err(lift)?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(shift)
}

/// One `sales` refund per tender, out of the account its method settles in.
async fn give_the_money_back(
    conn: &mut sqlx::PgConnection,
    sale: &AggregateId,
    returning: &Return,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PosError>> {
    let accounts = accounts(&mut *conn).await?;
    for (n, tender) in returning.tenders.iter().enumerate() {
        let reference = format!("refund-{}-{n}", tender.method);
        let receipt = sales::Receipt {
            reference: reference.clone(),
            amount: tender.amount,
            received_on: returning.at,
            into: accounts.for_method(tender.method).clone(),
        };
        sales::refund_in(
            &mut *conn,
            sale,
            &receipt,
            &format!("Refund {reference} · sale {sale}"),
            metadata,
        )
        .await
        .map_err(lift)?;
    }
    Ok(())
}

/// Cash out of the drawer for something that is not a refund.
#[derive(Debug, Clone)]
pub struct PayOut {
    /// The caller's key. Paying the same one out twice is a no-op (L8).
    pub reference: String,
    pub amount: Money,
    /// Where it went, as an account code — a banking run to `1010`, a supplier
    /// paid in cash to an expense.
    pub to: AggregateId,
    pub why: String,
    pub at: Timestamp,
}

/// Takes cash out of the drawer and posts where it went.
pub async fn pay_out(
    db: &TenantDb,
    shift: &AggregateId,
    payment: &PayOut,
    metadata: &Metadata,
) -> Outcome {
    if !payment.amount.is_positive() {
        return Err(rejected(PosError::NotAnAmount));
    }
    let entry = derived("pso", &[shift.as_str(), &payment.reference]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Shift, _, PosError>(
                &mut *conn,
                shift,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Shift>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PosError::NoSuchShift(shift.to_string()));
                    }
                    if held.has_pay_out(&payment.reference) {
                        return Ok(Decision::nothing());
                    }
                    if !held.is_open() {
                        return Err(PosError::Closed(shift.to_string()));
                    }
                    Ok(Decision::one(ShiftEvent::PaidOut {
                        reference: payment.reference.clone(),
                        why: payment.why.clone(),
                        amount: payment.amount,
                        at: payment.at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_pay_out(payment.amount, &payment.to, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PosError::Unbalanced(e)))?;
                post(
                    &mut *conn,
                    &entry,
                    payment.at,
                    &format!("Paid out · {} · {}", payment.reference, payment.why),
                    &lines,
                    metadata,
                )
                .await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(shift)
}

/// Counts the drawer, shuts the till, and books what the count disagreed by.
///
/// `declared` is what a person actually counted. The variance is
/// `declared - expected`, it is frozen into the event rather than recomputed on
/// read (L5), and it posts — see [`crate::posting`] for why that matters.
pub async fn close_shift(
    db: &TenantDb,
    shift: &AggregateId,
    declared: Money,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    if declared.is_negative() {
        return Err(rejected(PosError::NotAnAmount));
    }
    let entry = derived("psc", &[shift.as_str()]).map_err(rejected)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Shift, _, PosError>(
                &mut *conn,
                shift,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Shift>| {
                    let held = &loaded.aggregate;
                    if !held.exists() {
                        return Err(PosError::NoSuchShift(shift.to_string()));
                    }
                    // Closing a shut till is a no-op and not an error, so a
                    // manager whose request timed out can send it again.
                    if !held.is_open() {
                        return Ok(Decision::nothing());
                    }
                    let expected = held
                        .expected()
                        .ok_or_else(|| PosError::NoSuchShift(shift.to_string()))??;
                    let variance = declared.checked_sub(expected)?;
                    Ok(Decision::one(ShiftEvent::Closed {
                        expected,
                        declared,
                        variance,
                        at,
                    }))
                },
            )
            .await?;

            if let Some(variance) = closing(&committed.events) {
                let accounts = accounts(&mut *conn).await?;
                let lines = entry_for_variance(variance, &accounts)
                    .map_err(|e| ExecuteError::Rejected(PosError::Unbalanced(e)))?;
                if let Some(lines) = lines {
                    post(
                        &mut *conn,
                        &entry,
                        at,
                        &format!("Drawer variance · {shift}"),
                        &lines,
                        metadata,
                    )
                    .await?;
                }
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(shift)
}

/// One `sales` payment per tender, each into the account its method settles in.
///
/// Split out of [`sell`] because that function was one line over the limit, and
/// this half is a separate statement anyway: the sale is a document, and this is
/// the money arriving against it.
async fn take_the_money(
    conn: &mut sqlx::PgConnection,
    sale: &AggregateId,
    basket: &Basket,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PosError>> {
    let accounts = accounts(&mut *conn).await?;
    for (n, tender) in basket.tenders.iter().enumerate() {
        let reference = format!("{}-{n}", tender.method);
        let receipt = sales::Receipt {
            reference: reference.clone(),
            amount: tender.amount,
            received_on: basket.at,
            into: accounts.for_method(tender.method).clone(),
        };
        sales::pay_in(
            &mut *conn,
            sale,
            &receipt,
            &format!("Tender {reference} · sale {sale}"),
            metadata,
        )
        .await
        .map_err(lift)?;
    }
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn draft_from(basket: &Basket) -> sales::Draft {
    sales::Draft {
        customer: basket.customer.clone(),
        // **The tax point is the moment of the sale**, which at a counter is
        // also the moment of payment and the moment of handover.
        issued_on: basket.at,
        // Nothing is owed after a till sale, so nothing is due later.
        due_on: None,
        currency: basket.currency,
        lines: basket.lines.clone(),
        discounts: basket.discounts.clone(),
        note: basket.note.clone(),
    }
}

/// What the invoice came to, read back from what was actually recorded.
fn gross_of(issued: &sales::Numbered) -> Option<Money> {
    issued
        .committed
        .events
        .iter()
        .find_map(|event| match event {
            sales::InvoiceEvent::Issued { totals, .. } => Some(totals.gross),
            _ => None,
        })
}

/// What a close decided the variance was. `None` when it decided nothing.
fn closing(events: &[ShiftEvent]) -> Option<Money> {
    events.iter().find_map(|event| match event {
        ShiftEvent::Closed { variance, .. } => Some(*variance),
        _ => None,
    })
}

/// Carries a `sales` failure into this module's error without flattening what
/// kind of failure it was. The same shape `sales::lift` uses on `ledger`.
fn lift(error: ExecuteError<sales::SalesError>) -> ExecuteError<PosError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(PosError::Sale(e)),
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

async fn post(
    conn: &mut sqlx::PgConnection,
    entry: &AggregateId,
    on: Timestamp,
    memo: &str,
    lines: &ledger::BalancedLines,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PosError>> {
    ledger::post_entry_in(conn, entry, on, memo, lines, metadata)
        .await
        .map(|_| ())
        .map_err(|e| match e {
            ExecuteError::Rejected(refusal) => ExecuteError::Rejected(PosError::Ledger(refusal)),
            ExecuteError::Load(e) => ExecuteError::Load(e),
            ExecuteError::Append(e) => ExecuteError::Append(e),
            ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
            ExecuteError::Database(e) => ExecuteError::Database(e),
            ExecuteError::Contended { stream, attempts } => {
                ExecuteError::Contended { stream, attempts }
            }
            ExecuteError::AlreadyExists { stream } => ExecuteError::AlreadyExists { stream },
        })
}

async fn accounts(
    conn: &mut sqlx::PgConnection,
) -> Result<PostingAccounts, ExecuteError<PosError>> {
    PostingAccounts::resolve(conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PosError::Config(e)))
}
