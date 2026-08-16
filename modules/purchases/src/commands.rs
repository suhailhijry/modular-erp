//! What a caller can ask purchases to do.
//!
//! The same shape as `sales::commands`, and for the same reason: a bill and its
//! journal entry commit together, so the two cannot disagree. See that module's
//! header for why this is a transaction rather than an outbox effect.
//!
//! # What is different, and it is the interesting part
//!
//! Sales *computes* tax; purchases *records* it. Input tax is reclaimed against
//! the supplier's document, so the figure in the books has to be the figure on
//! the document. What this module validates is that the stated tax is
//! **plausible** — never negative, zero on anything not standard-rated, and
//! never claimed without a supplier VAT number to evidence it.

use ledger::{LedgerError, VatCategory};
use spa_control::{CommandError, TenantDb};
use spa_eventlog::{Committed, Decision, ExecuteError, MAX_ATTEMPTS, Metadata, try_execute};
use spa_types::{AggregateId, CurrencyCode, Money, StreamId, Timestamp};

use crate::bill::{Bill, BillEvent, BillLine, Supplier};
use crate::posting::{PostingAccounts, entry_for_bill, entry_for_payment};

#[derive(Debug, thiserror::Error)]
pub enum PurchaseError {
    #[error("a bill needs at least one line that comes to something")]
    NothingOnIt,
    #[error("bill {0} has not been recorded")]
    NotRecorded(String),
    #[error("only {outstanding} is outstanding; the payment is {offered}")]
    Overpayment { outstanding: Money, offered: Money },
    #[error("the bill is in {expected} and the payment is in {found}")]
    PaymentCurrency {
        expected: CurrencyCode,
        found: CurrencyCode,
    },
    #[error("a payment must be a positive amount")]
    NotAPayment,
    #[error("every line of a bill must be in the same currency")]
    MixedCurrencies,
    /// The supplier charged tax on something that does not carry it.
    #[error("a {category} line cannot carry tax, and this one carries {tax}")]
    TaxOnAnUntaxedLine { category: String, tax: Money },
    #[error("tax cannot be negative")]
    NegativeTax,
    /// Input tax is reclaimed against a registered supplier's tax invoice. A
    /// bill with tax on it and no registration number is not evidence of one.
    #[error("input tax cannot be reclaimed without the supplier's VAT number")]
    NoSupplierVatNumber,
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error(transparent)]
    Config(#[from] spa_eventlog::ConfigError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    /// The ledger refused the posting — a missing account, a closed one, or a
    /// closed period. Passed through rather than reworded: its message names
    /// what is wrong, and that is what the person fixing it needs.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl spa_i18n::Localize for PurchaseError {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::{Message, MessageArg};
        match self {
            Self::NothingOnIt => Message::new(messages::NOTHING_ON_IT),
            Self::NotRecorded(id) => {
                Message::new(messages::NOT_RECORDED).with("bill", MessageArg::text(id.clone()))
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
            Self::MixedCurrencies => Message::new(messages::MIXED_CURRENCIES),
            Self::TaxOnAnUntaxedLine { category, tax } => {
                Message::new(messages::TAX_ON_AN_UNTAXED_LINE)
                    .with("category", MessageArg::text(category.clone()))
                    .with("tax", MessageArg::text(tax.to_string()))
            }
            Self::NegativeTax => Message::new(messages::NEGATIVE_TAX),
            Self::NoSupplierVatNumber => Message::new(messages::NO_SUPPLIER_VAT_NUMBER),
            Self::InvalidReference(reference) => Message::new(messages::INVALID_REFERENCE)
                .with("reference", MessageArg::text(reference.clone())),
            // All three already say the right thing in both languages.
            Self::Config(e) => e.message(),
            Self::Unbalanced(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Outcome = Result<Committed<BillEvent>, CommandError<PurchaseError>>;

/// A supplier's invoice, as it arrived.
#[derive(Debug, Clone)]
pub struct Draft {
    pub supplier: Supplier,
    /// Their invoice number. What a reclaim is evidenced by.
    pub supplier_reference: String,
    /// The tax point, from their document. Not when it was typed in.
    pub billed_on: Timestamp,
    pub due_on: Option<Timestamp>,
    pub currency: CurrencyCode,
    pub lines: Vec<BillLine>,
    pub note: String,
}

/// Money going out against a bill.
#[derive(Debug, Clone)]
pub struct Payment {
    /// Our own reference. Recording the same one twice is a no-op.
    pub reference: String,
    pub amount: Money,
    pub paid_on: Timestamp,
    /// The cash or bank account it left.
    pub from: AggregateId,
}

/// What a bill comes to, summed from lines the supplier stated.
///
/// Summation only. Nothing here rounds, because nothing here decides an amount —
/// which is the whole difference between this and `sales::vat::total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Totals {
    net: Money,
    tax: Money,
    gross: Money,
}

/// Records a supplier's bill and posts it to the ledger, in one transaction.
///
/// Recording the same `id` twice is a no-op — the stored bill wins. `id` is
/// **our** key for it; the supplier's own number is `supplier_reference`, and it
/// goes on the document rather than being the identity, because two suppliers
/// can both call something `INV-001`.
pub async fn record_bill(
    db: &TenantDb,
    id: &AggregateId,
    draft: &Draft,
    metadata: &Metadata,
) -> Outcome {
    let totals = check(draft).map_err(rejected)?;

    let entry_id = derived_id("pb", &[id.as_str()])?;
    let memo = format!(
        "Bill {} · {}",
        draft.supplier_reference, draft.supplier.name
    );

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match record_in(&mut tx, id, &entry_id, draft, &totals, &memo, metadata).await {
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

/// **Everything decidable without the database, decided once.**
///
/// The tax is the supplier's, so this checks it is *possible* rather than
/// recomputing it. A figure that fails any of these is not a rounding
/// disagreement — it is a bill somebody typed in wrong, or a reclaim that would
/// be disallowed.
fn check(draft: &Draft) -> Result<Totals, PurchaseError> {
    if draft.lines.is_empty() {
        return Err(PurchaseError::NothingOnIt);
    }

    let mut net = Money::zero(draft.currency);
    let mut tax = Money::zero(draft.currency);
    let mut carries_tax = false;

    for line in &draft.lines {
        if line.net.currency() != draft.currency || line.tax.currency() != draft.currency {
            return Err(PurchaseError::MixedCurrencies);
        }
        if line.tax.is_negative() {
            return Err(PurchaseError::NegativeTax);
        }
        // Zero-rated and exempt supplies carry no tax. A supplier who charged
        // some has made a mistake, and recording it would claim it.
        if line.category != VatCategory::Standard && !line.tax.is_zero() {
            return Err(PurchaseError::TaxOnAnUntaxedLine {
                category: line.category.as_str().to_owned(),
                tax: line.tax,
            });
        }
        carries_tax |= !line.tax.is_zero();

        net = net
            .checked_add(line.net)
            .map_err(|e| PurchaseError::Unbalanced(e.into()))?;
        tax = tax
            .checked_add(line.tax)
            .map_err(|e| PurchaseError::Unbalanced(e.into()))?;
    }

    // The evidence rule. Reclaiming input tax needs the supplier's tax invoice,
    // and a tax invoice carries their registration number.
    if carries_tax && draft.supplier.vat_number.is_none() {
        return Err(PurchaseError::NoSupplierVatNumber);
    }

    let gross = net
        .checked_add(tax)
        .map_err(|e| PurchaseError::Unbalanced(e.into()))?;
    if gross.is_zero() && net.is_zero() {
        return Err(PurchaseError::NothingOnIt);
    }

    Ok(Totals { net, tax, gross })
}

/// One attempt at recording: the bill's event and its journal entry, in the
/// caller's transaction.
async fn record_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    entry_id: &AggregateId,
    draft: &Draft,
    totals: &Totals,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<BillEvent>, ExecuteError<PurchaseError>> {
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let entry_lines = entry_for_bill(&draft.lines, totals.gross, &accounts).map_err(|e| {
        ExecuteError::Rejected(match e {
            ledger::Unbalanced::TooFewLines(_) => PurchaseError::NothingOnIt,
            other => PurchaseError::Unbalanced(other),
        })
    })?;

    let committed = try_execute::<Bill, _, PurchaseError>(
        &mut *conn,
        id,
        crate::upcasters(),
        &metadata,
        |loaded| {
            if loaded.aggregate.received {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(BillEvent::Received {
                supplier: draft.supplier.clone(),
                supplier_reference: draft.supplier_reference.trim().to_owned(),
                billed_on: draft.billed_on,
                due_on: draft.due_on,
                currency: draft.currency,
                lines: draft.lines.clone(),
                net: totals.net,
                tax: totals.tax,
                gross: totals.gross,
                note: draft.note.trim().to_owned(),
            }))
        },
    )
    .await?;

    ledger::post_entry_in(
        conn,
        entry_id,
        draft.billed_on,
        memo,
        &entry_lines,
        &metadata,
    )
    .await
    .map_err(lift)?;

    Ok(committed)
}

/// Pays a supplier, and moves the money in the ledger.
///
/// Recording the same `reference` twice is a no-op.
pub async fn pay_bill(
    db: &TenantDb,
    bill: &AggregateId,
    payment: &Payment,
    metadata: &Metadata,
) -> Outcome {
    if !payment.amount.is_positive() {
        return Err(rejected(PurchaseError::NotAPayment));
    }

    let entry_id = derived_id("pp", &[bill.as_str(), &payment.reference])?;
    let memo = format!("Payment {} · bill {bill}", payment.reference);

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match pay_in(&mut tx, bill, &entry_id, payment, &memo, metadata).await {
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

    Err(contended(bill))
}

async fn pay_in(
    conn: &mut sqlx::PgConnection,
    bill: &AggregateId,
    entry_id: &AggregateId,
    payment: &Payment,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<BillEvent>, ExecuteError<PurchaseError>> {
    let (accounts, metadata) = resolve_accounts(&mut *conn, metadata).await?;

    let entry_lines = entry_for_payment(payment.amount, &payment.from, &accounts)
        .map_err(|e| ExecuteError::Rejected(PurchaseError::Unbalanced(e)))?;

    let committed = try_execute::<Bill, _, PurchaseError>(
        &mut *conn,
        bill,
        crate::upcasters(),
        &metadata,
        |loaded| {
            let state = &loaded.aggregate;
            if !state.received {
                return Err(PurchaseError::NotRecorded(bill.as_str().to_owned()));
            }
            // A retry, not a second payment.
            if state.has_payment(&payment.reference) {
                return Ok(Decision::nothing());
            }
            let expected = state.currency.unwrap_or_else(|| payment.amount.currency());
            if expected != payment.amount.currency() {
                return Err(PurchaseError::PaymentCurrency {
                    expected,
                    found: payment.amount.currency(),
                });
            }
            let outstanding = state
                .outstanding()
                .unwrap_or_else(|| Money::zero(payment.amount.currency()));
            // Compared in minor units: `Money` has no `PartialOrd`, deliberately,
            // because comparing two currencies is a question with no answer. The
            // currency check above is what makes this one meaningful.
            if payment.amount.minor() > outstanding.minor() {
                return Err(PurchaseError::Overpayment {
                    outstanding,
                    offered: payment.amount,
                });
            }
            Ok(Decision::one(BillEvent::PaymentMade {
                payment: payment.reference.clone(),
                amount: payment.amount,
                paid_on: payment.paid_on,
                from: payment.from.clone(),
            }))
        },
    )
    .await?;

    if committed.at.is_some() {
        ledger::post_entry_in(
            conn,
            entry_id,
            payment.paid_on,
            memo,
            &entry_lines,
            &metadata,
        )
        .await
        .map_err(lift)?;
    }

    Ok(committed)
}

/// The accounts a purchase moves, plus metadata stamped with the generation they
/// came from. Same reasoning as `sales::commands::resolve_accounts` (L5).
async fn resolve_accounts(
    conn: &mut sqlx::PgConnection,
    metadata: &Metadata,
) -> Result<(PostingAccounts, Metadata), ExecuteError<PurchaseError>> {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PurchaseError::Config(e)))?;
    let version = spa_eventlog::configuration::version(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(PurchaseError::Config(e)))?;

    let mut stamped = metadata.clone();
    stamped.config_version = Some(version);
    Ok((accounts, stamped))
}

fn derived_id(prefix: &str, parts: &[&str]) -> Result<AggregateId, CommandError<PurchaseError>> {
    let joined = format!("{prefix}.{}", parts.join("."));
    AggregateId::new(&joined)
        .map_err(|_| rejected(PurchaseError::InvalidReference(parts.join("."))))
}

fn rejected(error: PurchaseError) -> CommandError<PurchaseError> {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn contended(id: &AggregateId) -> CommandError<PurchaseError> {
    ExecuteError::Contended {
        stream: StreamId::new(<Bill as spa_eventlog::Aggregate>::domain(), id.clone()),
        attempts: MAX_ATTEMPTS,
    }
    .into()
}

/// Carries a ledger failure into this module's error type without flattening
/// what kind of failure it was. Same as `sales::commands::lift`, and the
/// duplication is the honest kind: the two map into different error enums.
fn lift(error: ExecuteError<LedgerError>) -> ExecuteError<PurchaseError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(PurchaseError::Ledger(e)),
        ExecuteError::Load(e) => ExecuteError::Load(e),
        ExecuteError::Append(e) => ExecuteError::Append(e),
        ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
        ExecuteError::Database(e) => ExecuteError::Database(e),
        ExecuteError::Contended { stream, attempts } => {
            ExecuteError::Contended { stream, attempts }
        }
    }
}

/// The `Send` guard. See `sales::commands` for why this lives here.
const _: fn() = || {
    fn assert_send<T: Send>(_: T) {}
    fn commands_are_send(
        db: &TenantDb,
        id: &AggregateId,
        draft: &Draft,
        payment: &Payment,
        metadata: &Metadata,
    ) {
        assert_send(record_bill(db, id, draft, metadata));
        assert_send(pay_bill(db, id, payment, metadata));
    }
    let _ = commands_are_send;
};

#[cfg(test)]
mod tests {
    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }
    fn money(minor: i64) -> Money {
        Money::from_minor(minor, sar())
    }
    fn code(s: &str) -> AggregateId {
        AggregateId::new(s).unwrap_or_else(|_| unreachable!())
    }
    fn line(net: i64, category: VatCategory, tax: i64) -> BillLine {
        BillLine {
            description: "something".to_owned(),
            account: code("5000"),
            net: money(net),
            category,
            rate_bp: ledger::Rates::saudi_arabia().of(category),
            tax: money(tax),
        }
    }
    fn draft(lines: Vec<BillLine>) -> Draft {
        Draft {
            supplier: Supplier::new("Najd Supplies").with_vat_number("310000000000003"),
            supplier_reference: "S-1".to_owned(),
            billed_on: Timestamp::UNIX_EPOCH,
            due_on: None,
            currency: sar(),
            lines,
            note: String::new(),
        }
    }

    #[test]
    fn totals_are_summed_from_what_the_supplier_stated() {
        let totals = check(&draft(vec![
            line(100_000, VatCategory::Standard, 15_000),
            line(40_000, VatCategory::Zero, 0),
        ]))
        .expect("is plausible");

        assert_eq!(totals.net, money(140_000));
        assert_eq!(totals.tax, money(15_000));
        assert_eq!(totals.gross, money(155_000));
    }

    /// **Their arithmetic wins, not ours.** A supplier whose rounding lands a
    /// halala away from 15% is recorded as they stated it, because the reclaim
    /// is evidenced by their document.
    #[test]
    fn tax_that_is_not_exactly_fifteen_percent_is_still_recorded() {
        let totals = check(&draft(vec![line(100_000, VatCategory::Standard, 14_999)]))
            .expect("a halala of disagreement is not an error");
        assert_eq!(totals.tax, money(14_999));
    }

    #[test]
    fn a_zero_rated_line_cannot_carry_tax() {
        let refused = check(&draft(vec![line(100_000, VatCategory::Zero, 15_000)]));
        assert!(matches!(
            refused,
            Err(PurchaseError::TaxOnAnUntaxedLine { .. })
        ));
    }

    #[test]
    fn tax_cannot_be_negative() {
        let refused = check(&draft(vec![line(100_000, VatCategory::Standard, -1)]));
        assert!(matches!(refused, Err(PurchaseError::NegativeTax)));
    }

    /// The evidence rule: no registration number, no reclaim.
    #[test]
    fn tax_without_a_supplier_registration_number_is_refused() {
        let mut unregistered = draft(vec![line(100_000, VatCategory::Standard, 15_000)]);
        unregistered.supplier = Supplier::new("A Man With A Van");

        assert!(matches!(
            check(&unregistered),
            Err(PurchaseError::NoSupplierVatNumber)
        ));

        // And a bill from the same supplier with no tax on it is fine — plenty
        // of real suppliers are below the registration threshold.
        let mut untaxed = unregistered.clone();
        untaxed.lines = vec![line(100_000, VatCategory::Zero, 0)];
        assert!(check(&untaxed).is_ok());
    }

    #[test]
    fn a_bill_with_no_lines_is_not_a_bill() {
        assert!(matches!(
            check(&draft(Vec::new())),
            Err(PurchaseError::NothingOnIt)
        ));
    }
}
