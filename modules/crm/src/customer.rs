//! A customer, as a record the business keeps.
//!
//! # What this is for, and what it does not replace
//!
//! An invoice freezes the buyer's name and address onto the document, because a
//! tax invoice is a legal statement about what was printed and a customer who
//! moves offices next year must not change what was issued this year. That is
//! law L5 and it stays exactly as it is.
//!
//! What it cannot do is answer *"everything for this customer"*. Two spellings
//! of one name are two rows, receivables groups by a string, and a reservation
//! has nobody to belong to. This is the record those questions need, and it
//! sits **beside** the frozen copy and never instead of it.
//!
//! # Why this module depends on nothing
//!
//! A customer is not an accounting document, so `crm` needs no ledger. Keeping
//! it at the bottom means `sales`, `booking` and `prepaid` can all reference a
//! customer without any of them depending on each other.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// Whether the customer is a person or an organisation.
///
/// Not cosmetic: ZATCA wants a buyer VAT number on a standard invoice, and only
/// an organisation has one. It also decides what a form asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomerKind {
    Person,
    Company,
}

impl CustomerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Company => "company",
        }
    }
}

impl std::str::FromStr for CustomerKind {
    type Err = UnknownKind;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "person" => Ok(Self::Person),
            "company" => Ok(Self::Company),
            other => Err(UnknownKind(other.to_owned())),
        }
    }
}

/// A stored kind this build does not recognise.
///
/// Refused and never defaulted, for the reason `UnknownRole` is: guessing
/// silently changes what a document says about a buyer.
#[derive(Debug, thiserror::Error)]
#[error("{0} is not a customer kind")]
pub struct UnknownKind(pub String);

/// How to reach them.
///
/// Both optional, and at least one is required by [`crate::register_customer`].
/// A customer nobody can contact is a row that cannot be sent a reminder, a
/// receipt or a confirmation, which is most of what this module exists to
/// enable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl Contact {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.phone.is_none() && self.email.is_none()
    }
}

/// Where they are.
///
/// # Why this is a third address type
///
/// `sales::Address` is what a document froze and `tax_sa::taxpayer::Address` is
/// the seller's own registration. This one is the customer's current address,
/// which is a different fact with a different lifetime: it changes when they
/// move, and the other two must not.
///
/// The shape matches what ZATCA wants on a standard invoice (BT-50, BT-52,
/// BT-55) so that copying it onto a document needs no translation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    pub city: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// ISO 3166-1 alpha-2. `SA` for the first market.
    pub country: String,
}

/// What the tax authority knows them by.
///
/// Separate from [`Address`] because a business can move without its
/// registration changing, and because this is the field that decides whether an
/// invoice to them is standard or simplified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxRegistration {
    /// The 15-digit Saudi VAT number, when they have one.
    pub vat_number: String,
    /// The other register they are identified in — `CRN` and the rest of
    /// ZATCA's `schemeID` list. Kept as a string here because the list is the
    /// authority's and `tax_sa` owns the enum; duplicating it would give two
    /// places to disagree about what is valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CustomerEvent {
    Registered {
        name: String,
        /// The Latin spelling, when the primary name is Arabic. For our own
        /// screens and for sorting; a document prints `name`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        kind: CustomerKind,
        contact: Contact,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tax: Option<TaxRegistration>,
        registered_on: Timestamp,
    },
    /// Any of the mutable fields, as a whole.
    ///
    /// One event and not six, because a form saves once and a customer record
    /// has no field whose change means something on its own. Six events would
    /// be six projections of the same edit.
    Amended {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        kind: CustomerKind,
        contact: Contact,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<Address>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tax: Option<TaxRegistration>,
    },
    /// Out of the lists, still on every document they are named on.
    ///
    /// Never a delete. A customer on a cleared tax invoice is part of a record
    /// the authority requires kept, and erasing them would orphan it. Erasing a
    /// *person* is a different request and lives in the control plane.
    Archived {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Restored,
}

impl CustomerEvent {
    /// Every name this event type can carry.
    pub const NAMES: [&'static str; 4] = [
        "crm.customer.registered",
        "crm.customer.amended",
        "crm.customer.archived",
        "crm.customer.restored",
    ];
}

impl DomainEvent for CustomerEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Registered { .. } => Self::NAMES[0],
            Self::Amended { .. } => Self::NAMES[1],
            Self::Archived { .. } => Self::NAMES[2],
            Self::Restored => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a customer before deciding.
#[derive(Debug, Default)]
pub struct Customer {
    registered: bool,
    archived: bool,
    name: String,
    tax: Option<TaxRegistration>,
}

impl Aggregate for Customer {
    type Event = CustomerEvent;

    fn domain() -> DomainName {
        crate::domain("crm_customer")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            CustomerEvent::Registered { name, tax, .. } => {
                self.registered = true;
                self.archived = false;
                self.name.clone_from(name);
                self.tax.clone_from(tax);
            }
            CustomerEvent::Amended { name, tax, .. } => {
                self.name.clone_from(name);
                self.tax.clone_from(tax);
            }
            CustomerEvent::Archived { .. } => self.archived = true,
            CustomerEvent::Restored => self.archived = false,
        }
    }
}

impl Customer {
    /// Whether this record exists at all.
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.registered
    }

    /// Whether they are out of the lists.
    #[must_use]
    pub const fn is_archived(&self) -> bool {
        self.archived
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Their VAT number, which is what decides whether an invoice to them is a
    /// standard document or a simplified one.
    #[must_use]
    pub fn vat_number(&self) -> Option<&str> {
        self.tax.as_ref().map(|t| t.vat_number.as_str())
    }
}
