//! A supplier's invoice, as an aggregate.

use ledger::VatCategory;
use serde::{Deserialize, Serialize};
use spa_eventlog::{Aggregate, DomainEvent};
use spa_types::{CurrencyCode, DomainName, EventName, Money, SchemaVersion, Timestamp};

/// Who sent the bill, **as they were on it**.
///
/// A snapshot for the same reason [`sales::Customer`](../../sales) is one: a tax
/// invoice is a legal document, and a supplier changing their registered name
/// next year must not rewrite the copy in the filing cabinet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Supplier {
    pub name: String,
    /// Their VAT registration number.
    ///
    /// **Input tax cannot be reclaimed without one.** A bill from an
    /// unregistered supplier carries no recoverable VAT, and recording it as if
    /// it did is a reclaim ZATCA will disallow — so a line with tax on it needs
    /// this, and [`crate::record_bill`] refuses otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
}

impl Supplier {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            vat_number: None,
        }
    }

    #[must_use]
    pub fn with_vat_number(mut self, number: impl Into<String>) -> Self {
        let number = number.into();
        self.vat_number = (!number.trim().is_empty()).then_some(number);
        self
    }
}

/// One thing being charged for, **as the supplier stated it**.
///
/// # Why the tax is a field and not a calculation
///
/// This is the whole difference between a bill and an invoice, and it is a
/// domain rule rather than a shortcut. Input tax is reclaimed against the
/// supplier's tax invoice, so what goes in the books is the figure on the
/// document you hold. A recomputation that lands a halala away from theirs
/// produces a reclaim that does not match the evidence for it — and the evidence
/// is what an inspector asks to see.
///
/// So `tax` is recorded, and `crate::record_bill` checks it is *plausible*
/// rather than exact: never negative, and zero on anything not standard-rated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillLine {
    pub description: String,
    /// The expense or asset account this line lands in. Named per line, because
    /// one bill routinely covers rent and stationery.
    pub account: spa_types::AggregateId,
    /// Excluding tax.
    pub net: Money,
    pub category: VatCategory,
    /// The rate the supplier charged, in basis points. Recorded rather than
    /// resolved: it is on their document, and if it disagrees with today's
    /// statutory rate that is a thing worth being able to see.
    pub rate_bp: i32,
    /// The tax the supplier charged on this line.
    pub tax: Money,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BillEvent {
    Received {
        supplier: Supplier,
        /// **The supplier's own invoice number.** Not ours: we did not issue this
        /// document, so there is no gapless series to take it from. It is what a
        /// reclaim is evidenced by, and what a duplicate check looks at.
        supplier_reference: String,
        /// The tax point — the date the supply was made, from their document.
        billed_on: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_on: Option<Timestamp>,
        currency: CurrencyCode,
        lines: Vec<BillLine>,
        net: Money,
        tax: Money,
        gross: Money,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    PaymentMade {
        /// Our own reference for the payment — a transfer number, a cheque.
        /// Recording it twice is a no-op, which is what makes a retry safe.
        payment: String,
        amount: Money,
        paid_on: Timestamp,
        /// The cash or bank account it left.
        from: spa_types::AggregateId,
    },
}

impl BillEvent {
    pub const NAMES: [&'static str; 2] = ["purchases.bill.received", "purchases.bill.paid"];
}

impl DomainEvent for BillEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Received { .. } => Self::NAMES[0],
            Self::PaymentMade { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a bill before deciding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bill {
    pub received: bool,
    pub currency: Option<CurrencyCode>,
    pub gross: Option<Money>,
    /// Total paid so far. `None` until the bill is recorded.
    pub paid: Option<Money>,
    /// Payment references already recorded — the same shape, and the same
    /// reasoning, as `sales::Invoice::payments`.
    pub payments: Vec<String>,
}

impl Aggregate for Bill {
    type Event = BillEvent;

    fn domain() -> DomainName {
        crate::domain("purchases_bill")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            BillEvent::Received {
                currency, gross, ..
            } => {
                self.received = true;
                self.currency = Some(*currency);
                self.gross = Some(*gross);
                self.paid = Some(Money::zero(*currency));
            }
            BillEvent::PaymentMade {
                payment, amount, ..
            } => {
                self.payments.push(payment.clone());
                self.paid = match self.paid {
                    Some(paid) => paid.checked_add(*amount).ok(),
                    None => None,
                };
            }
        }
    }
}

impl Bill {
    /// What is still owed to the supplier. `None` before the bill exists.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        self.gross?.checked_sub(self.paid?).ok()
    }

    /// Whether this payment reference has already been recorded.
    #[must_use]
    pub fn has_payment(&self, reference: &str) -> bool {
        self.payments.iter().any(|p| p == reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }

    fn received(gross: i64) -> BillEvent {
        let currency = sar();
        BillEvent::Received {
            supplier: Supplier::new("Najd Supplies").with_vat_number("310000000000003"),
            supplier_reference: "S-1".to_owned(),
            billed_on: Timestamp::UNIX_EPOCH,
            due_on: None,
            currency,
            lines: Vec::new(),
            net: Money::from_minor(gross, currency),
            tax: Money::zero(currency),
            gross: Money::from_minor(gross, currency),
            note: String::new(),
        }
    }

    fn paid(reference: &str, amount: i64) -> BillEvent {
        BillEvent::PaymentMade {
            payment: reference.to_owned(),
            amount: Money::from_minor(amount, sar()),
            paid_on: Timestamp::UNIX_EPOCH,
            from: spa_types::AggregateId::new("1000").unwrap_or_else(|_| unreachable!()),
        }
    }

    #[test]
    fn a_bill_owes_its_gross_until_it_is_paid() {
        let mut bill = Bill::default();
        assert_eq!(bill.outstanding(), None, "nothing is owed on nothing");

        bill.apply(&received(11_500));
        assert_eq!(bill.outstanding(), Some(Money::from_minor(11_500, sar())));

        bill.apply(&paid("wire-1", 5_000));
        assert_eq!(bill.outstanding(), Some(Money::from_minor(6_500, sar())));

        bill.apply(&paid("wire-2", 6_500));
        assert_eq!(bill.outstanding(), Some(Money::zero(sar())));
    }

    #[test]
    fn a_payment_reference_is_remembered_so_a_retry_is_a_no_op() {
        let mut bill = Bill::default();
        bill.apply(&received(11_500));
        bill.apply(&paid("wire-1", 5_000));

        assert!(bill.has_payment("wire-1"));
        assert!(!bill.has_payment("wire-2"));
    }
}
