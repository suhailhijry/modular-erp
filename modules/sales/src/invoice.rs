//! The invoice, as an aggregate.

use serde::{Deserialize, Serialize};
use spa_eventlog::{Aggregate, DomainEvent};
use spa_types::{
    AggregateId, CurrencyCode, DomainName, EventName, Money, SchemaVersion, Timestamp,
};

use crate::vat::{Totals, Vat};

/// Who the invoice is addressed to, **as it was at the time**.
///
/// A snapshot, not a reference. A tax invoice is a legal document: changing a
/// customer's registered name next year must not rewrite what was issued this
/// year, and a foreign key would do exactly that. This is architecture L5
/// applied to the most visible place it matters.
///
/// ponytail: no customer aggregate yet. When someone wants a customer list or a
/// statement of account, that is what earns one — and it will still be copied
/// onto the invoice at issue, for the reason above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Customer {
    pub name: String,
    /// The buyer's VAT registration number. Required by ZATCA on a B2B invoice
    /// and absent on a simplified one, which is why it is optional here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vat_number: Option<String>,
}

impl Customer {
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

/// One thing being charged for.
///
/// ponytail: no quantity or unit price. A client that shows "3 × 250.00" already
/// computed the 750.00 it sends; storing the factors matters when ZATCA's
/// line-level fields are implemented, and adding them then is an upcaster — the
/// mechanism this system already has and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvoiceLine {
    pub description: String,
    /// Excluding tax. Negative is allowed: a discount is a line.
    pub net: Money,
    pub vat: Vat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InvoiceEvent {
    Issued {
        customer: Customer,
        /// The tax point — the date the supply is treated as made. Not when the
        /// row was written.
        issued_on: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_on: Option<Timestamp>,
        currency: CurrencyCode,
        lines: Vec<InvoiceLine>,
        /// Computed once, at issue, and stored. Recomputing on read would let a
        /// rate change or a rounding fix silently restate a document somebody
        /// has already filed a return against.
        totals: Totals,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    /// Cancelled by a credit note, which reversed its journal entry.
    ///
    /// Not a deletion: the invoice was issued, somebody may hold a copy, and
    /// the books show both it and the credit. What changes is that nothing is
    /// owed on it.
    Cancelled {
        /// The credit note's own identifier, and the id of the reversing
        /// journal entry.
        credit_note: String,
        reason: String,
        on: Timestamp,
    },
    PaymentRecorded {
        /// The payer's or the client's own reference. Recording it twice is a
        /// no-op, which is what makes a retried request safe.
        payment: String,
        amount: Money,
        received_on: Timestamp,
        /// Which cash or bank account took it. Chosen per payment, because a
        /// business with two banks needs to say which one.
        account: AggregateId,
    },
}

impl InvoiceEvent {
    pub const NAMES: [&'static str; 3] = [
        "sales.invoice.issued",
        "sales.invoice.payment_recorded",
        "sales.invoice.cancelled",
    ];
}

impl DomainEvent for InvoiceEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Issued { .. } => Self::NAMES[0],
            Self::PaymentRecorded { .. } => Self::NAMES[1],
            Self::Cancelled { .. } => Self::NAMES[2],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about an invoice before deciding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Invoice {
    pub issued: bool,
    pub currency: Option<CurrencyCode>,
    pub gross: Option<Money>,
    /// Total received so far. `None` until the invoice is issued.
    pub paid: Option<Money>,
    /// Cancelled, and by which credit note.
    pub cancelled_by: Option<String>,
    /// Payment references already recorded. Small — an invoice is settled in a
    /// handful of instalments at most — and the only way to make recording a
    /// payment idempotent without a separate table.
    pub payments: Vec<String>,
}

impl Aggregate for Invoice {
    type Event = InvoiceEvent;

    fn domain() -> DomainName {
        crate::domain("sales_invoice")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            InvoiceEvent::Issued {
                currency, totals, ..
            } => {
                self.issued = true;
                self.currency = Some(*currency);
                self.gross = Some(totals.gross);
                self.paid = Some(Money::zero(*currency));
            }
            InvoiceEvent::Cancelled { credit_note, .. } => {
                self.cancelled_by = Some(credit_note.clone());
            }
            InvoiceEvent::PaymentRecorded {
                payment, amount, ..
            } => {
                self.payments.push(payment.clone());
                // Saturating rather than checked: `apply` cannot fail, and the
                // command already refused anything that would not fit. A total
                // that overflows here would mean the log itself is corrupt,
                // which the outstanding-amount check then catches.
                self.paid = match self.paid {
                    Some(paid) => paid.checked_add(*amount).ok(),
                    None => None,
                };
            }
        }
    }
}

impl Invoice {
    /// What is still owed. `None` before the invoice exists.
    #[must_use]
    pub fn outstanding(&self) -> Option<Money> {
        self.gross?.checked_sub(self.paid?).ok()
    }

    /// Whether a credit note has cancelled this invoice.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled_by.is_some()
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
    use crate::vat::{VatCategory, total};

    fn sar() -> CurrencyCode {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!())
    }

    fn issued(gross_net: i64) -> InvoiceEvent {
        let currency = sar();
        let vat = Vat::current(VatCategory::Standard);
        let net = Money::from_minor(gross_net, currency);
        InvoiceEvent::Issued {
            customer: Customer::new("Acme"),
            issued_on: Timestamp::UNIX_EPOCH,
            due_on: None,
            currency,
            lines: vec![InvoiceLine {
                description: "Consulting".to_owned(),
                net,
                vat,
            }],
            totals: total([(vat, net)], currency).unwrap_or_else(|_| unreachable!()),
            note: String::new(),
        }
    }

    #[test]
    fn a_payment_reduces_what_is_outstanding() {
        let mut invoice = Invoice::default();
        // 100.00 net, 15.00 tax, 115.00 gross.
        invoice.apply(&issued(10_000));
        assert_eq!(
            invoice.outstanding(),
            Some(Money::from_minor(11_500, sar()))
        );

        invoice.apply(&InvoiceEvent::PaymentRecorded {
            payment: "wire-1".to_owned(),
            amount: Money::from_minor(5_000, sar()),
            received_on: Timestamp::UNIX_EPOCH,
            account: AggregateId::new("1010").unwrap_or_else(|_| unreachable!()),
        });

        assert_eq!(invoice.outstanding(), Some(Money::from_minor(6_500, sar())));
        assert!(invoice.has_payment("wire-1"));
        assert!(!invoice.has_payment("wire-2"));
    }

    #[test]
    fn an_unissued_invoice_owes_nothing_rather_than_zero() {
        // `None`, not `Some(0)` — the difference between "not there" and
        // "settled", which is what stops a payment landing on a blank id.
        let invoice = Invoice::default();
        assert_eq!(invoice.outstanding(), None);
        assert!(!invoice.issued);
    }
}
