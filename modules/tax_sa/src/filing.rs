//! A return that has been filed.
//!
//! # Why filing is recorded rather than inferred
//!
//! Every other part of this system already makes re-running a period give the
//! number that was filed: documents are reported on their own tax point, and a
//! closed period refuses back-dated writes. Those are properties of the
//! *arithmetic*, and they hold as long as nobody makes a mistake.
//!
//! Recording the filing is stronger and cheaper: the numbers that went to ZATCA
//! are in the log, with the date they went and who sent them. "Does the system
//! still agree with what we filed?" becomes a comparison rather than an
//! argument, and the answer survives a rebuild because it is an event rather
//! than a derivation.

use serde::{Deserialize, Serialize};
use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{CurrencyCode, DomainName, EventName, Money, SchemaVersion, Timestamp};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilingEvent {
    Filed {
        /// Inclusive.
        from: Timestamp,
        /// **Exclusive.**
        until: Timestamp,
        currency: CurrencyCode,
        /// What was charged, as it stood when this was filed.
        output_tax: Money,
        /// What was reclaimed.
        input_tax: Money,
        /// What was paid, or reclaimed if negative.
        payable: Money,
        /// The date the business treats the filing as made — not a clock
        /// reading, for the same reason a tax point is not one.
        filed_on: Timestamp,
        /// ZATCA's acknowledgement, once there is one to record.
        ///
        /// **Not invoice clearance** — that is `tax_sa.zatca.accepted`, one
        /// event per document. This is the acknowledgement for the *return*,
        /// which is filed on ZATCA's portal rather than through the invoicing
        /// API, so there is nothing here to automate yet. The field is here
        /// because a filing without a reference is a filing nobody can prove.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
}

impl FilingEvent {
    pub const NAMES: [&'static str; 1] = ["tax_sa.return.filed"];
}

impl DomainEvent for FilingEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Filed { .. } => Self::NAMES[0],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a period before filing it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filing {
    pub filed: bool,
    /// What was declared, for telling a repeat caller what already went.
    pub payable: Option<Money>,
    pub filed_on: Option<Timestamp>,
}

impl Aggregate for Filing {
    type Event = FilingEvent;

    fn domain() -> DomainName {
        crate::domain("tax_sa_filing")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            FilingEvent::Filed {
                payable, filed_on, ..
            } => {
                self.filed = true;
                self.payable = Some(*payable);
                self.filed_on = Some(*filed_on);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_period_is_unfiled_until_it_is_filed() {
        let currency = CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!());
        let mut filing = Filing::default();
        assert!(!filing.filed);

        filing.apply(&FilingEvent::Filed {
            from: Timestamp::UNIX_EPOCH,
            until: Timestamp::UNIX_EPOCH,
            currency,
            output_tax: Money::from_minor(15_000, currency),
            input_tax: Money::from_minor(6_000, currency),
            payable: Money::from_minor(9_000, currency),
            filed_on: Timestamp::UNIX_EPOCH,
            reference: None,
        });

        assert!(filing.filed);
        assert_eq!(filing.payable, Some(Money::from_minor(9_000, currency)));
    }
}
