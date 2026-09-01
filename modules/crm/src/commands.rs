//! What a caller can ask `crm` to do.
//!
//! # Why these use `TenantDb::execute` and sales does not
//!
//! A customer touches one aggregate and nothing else. `sales` has to open its
//! own transaction because an invoice and its journal entry commit together;
//! there is no second thing here, so the plain path is the right one.

use erp_eventlog::{Committed, Decision, Metadata};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, Timestamp};

use crate::customer::{Address, Contact, Customer, CustomerEvent, CustomerKind, TaxRegistration};

/// The longest a name may be.
///
/// Matches what `sales` will freeze onto a document and what ZATCA accepts in
/// the buyer name field, so a name that can be stored is a name that can be
/// invoiced.
const MAX_NAME: usize = 200;

/// A Saudi VAT number is fifteen digits, starts and ends with 3.
///
/// Checked here rather than at clearance for the reason `tax_sa::Registration`
/// checks the seller's: by the time ZATCA says no, the invoice exists and a
/// standard one cannot be given to the buyer until it is cleared.
const VAT_DIGITS: usize = 15;

#[derive(Debug, thiserror::Error)]
pub enum CrmError {
    #[error("a customer needs a name")]
    NoName,
    #[error("a name may not be longer than {MAX_NAME} characters")]
    NameTooLong,
    #[error("a customer needs a phone number or an email address")]
    NoContact,
    #[error("customer {0} does not exist")]
    NoSuchCustomer(String),
    #[error("customer {0} already exists")]
    AlreadyExists(String),
    #[error("customer {0} is archived")]
    Archived(String),
    #[error("{0} is not a Saudi VAT number")]
    NotAVatNumber(String),
    #[error("a company that gives a VAT number is not a person")]
    PersonWithVatNumber,
}

impl erp_i18n::Localize for CrmError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoName => Message::new(messages::NO_NAME),
            Self::NameTooLong => Message::new(messages::NAME_TOO_LONG).with(
                "n",
                MessageArg::Count(i64::try_from(MAX_NAME).unwrap_or(i64::MAX)),
            ),
            Self::NoContact => Message::new(messages::NO_CONTACT),
            Self::NoSuchCustomer(id) => {
                Message::new(messages::NO_SUCH_CUSTOMER).with("customer", MessageArg::text(id))
            }
            Self::AlreadyExists(id) => {
                Message::new(messages::ALREADY_EXISTS).with("customer", MessageArg::text(id))
            }
            Self::Archived(id) => {
                Message::new(messages::ARCHIVED).with("customer", MessageArg::text(id))
            }
            Self::NotAVatNumber(n) => {
                Message::new(messages::NOT_A_VAT_NUMBER).with("value", MessageArg::text(n))
            }
            Self::PersonWithVatNumber => Message::new(messages::PERSON_WITH_VAT_NUMBER),
        }
    }
}

type Outcome = Result<Committed<CustomerEvent>, CommandError<CrmError>>;

/// Everything a customer record holds.
///
/// A struct and not eight parameters, for the reason `sales::Draft` is one:
/// most of them are strings and transposing two strings is a bug no type can
/// catch.
#[derive(Debug, Clone)]
pub struct Details {
    pub name: String,
    pub name_latin: Option<String>,
    pub kind: CustomerKind,
    pub contact: Contact,
    pub address: Option<Address>,
    pub tax: Option<TaxRegistration>,
}

impl Details {
    /// Everything checkable without the stored state.
    ///
    /// Separate from the commands so both of them apply exactly the same rules,
    /// and so a caller can check a form before opening a transaction.
    pub fn check(&self) -> Result<(), CrmError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(CrmError::NoName);
        }
        if name.chars().count() > MAX_NAME {
            return Err(CrmError::NameTooLong);
        }
        if self.contact.is_empty() {
            return Err(CrmError::NoContact);
        }
        if let Some(tax) = &self.tax {
            let vat = tax.vat_number.trim();
            if vat.chars().count() != VAT_DIGITS
                || !vat.chars().all(|c| c.is_ascii_digit())
                || !vat.starts_with('3')
                || !vat.ends_with('3')
            {
                return Err(CrmError::NotAVatNumber(tax.vat_number.clone()));
            }
            // A natural person does not hold a VAT registration, and an invoice
            // to one is simplified. Storing both would make the standard or
            // simplified decision ambiguous at the moment it is taken.
            if self.kind == CustomerKind::Person {
                return Err(CrmError::PersonWithVatNumber);
            }
        }
        Ok(())
    }
}

/// Creates a customer.
///
/// Re-registering an existing id is **refused and not ignored**, the same call
/// `ledger::open_account` makes: the second caller almost certainly meant a
/// different customer, and silently returning the first one would attach their
/// next invoice to somebody else.
pub async fn register_customer(
    db: &TenantDb,
    id: &AggregateId,
    details: &Details,
    registered_on: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    details.check().map_err(rejected)?;
    let details = details.clone();
    let key = id.to_string();

    db.execute::<Customer, _, CrmError>(id, crate::upcasters(), metadata, move |loaded| {
        if loaded.aggregate.exists() {
            return Err(CrmError::AlreadyExists(key.clone()));
        }
        Ok(Decision::one(CustomerEvent::Registered {
            name: details.name.trim().to_owned(),
            name_latin: details.name_latin.clone(),
            kind: details.kind,
            contact: details.contact.clone(),
            address: details.address.clone(),
            tax: details.tax.clone(),
            registered_on,
        }))
    })
    .await
}

/// Changes what is known about a customer.
///
/// A no-op when nothing moved, so a form saved twice writes one event. That is
/// what keeps a customer's history readable: every event in it is a change
/// somebody made.
pub async fn amend_customer(
    db: &TenantDb,
    id: &AggregateId,
    details: &Details,
    metadata: &Metadata,
) -> Outcome {
    details.check().map_err(rejected)?;
    let details = details.clone();
    let key = id.to_string();

    db.execute::<Customer, _, CrmError>(id, crate::upcasters(), metadata, move |loaded| {
        if !loaded.aggregate.exists() {
            return Err(CrmError::NoSuchCustomer(key.clone()));
        }
        if loaded.aggregate.is_archived() {
            return Err(CrmError::Archived(key.clone()));
        }
        let name = details.name.trim();
        if loaded.aggregate.name() == name
            && loaded.aggregate.vat_number() == details.tax.as_ref().map(|t| t.vat_number.as_str())
        {
            // Cheap comparison on the two fields the aggregate keeps. The
            // projection holds the rest, and re-writing an identical row is
            // harmless where re-writing an identical *event* is not.
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(CustomerEvent::Amended {
            name: name.to_owned(),
            name_latin: details.name_latin.clone(),
            kind: details.kind,
            contact: details.contact.clone(),
            address: details.address.clone(),
            tax: details.tax.clone(),
        }))
    })
    .await
}

/// Takes a customer out of the lists, keeping every document they are on.
///
/// Idempotent: archiving an archived customer is a no-op, because the caller
/// wanted them archived and they are.
pub async fn archive_customer(
    db: &TenantDb,
    id: &AggregateId,
    reason: Option<String>,
    metadata: &Metadata,
) -> Outcome {
    let key = id.to_string();
    db.execute::<Customer, _, CrmError>(id, crate::upcasters(), metadata, move |loaded| {
        if !loaded.aggregate.exists() {
            return Err(CrmError::NoSuchCustomer(key.clone()));
        }
        if loaded.aggregate.is_archived() {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(CustomerEvent::Archived {
            reason: reason.clone(),
        }))
    })
    .await
}

/// Puts them back.
pub async fn restore_customer(db: &TenantDb, id: &AggregateId, metadata: &Metadata) -> Outcome {
    let key = id.to_string();
    db.execute::<Customer, _, CrmError>(id, crate::upcasters(), metadata, move |loaded| {
        if !loaded.aggregate.exists() {
            return Err(CrmError::NoSuchCustomer(key.clone()));
        }
        if !loaded.aggregate.is_archived() {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(CustomerEvent::Restored))
    })
    .await
}

/// Whether a customer exists and may be named on a new document **right now**.
///
/// Reads the log and not `proj_crm.customer`, for the reason
/// `ledger::accepts_postings` does: the read model is driven by a worker and
/// lags, so a customer created a moment ago is not in it yet, and validating
/// against it would tell somebody the customer they just created does not
/// exist.
pub async fn accepts_documents(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
) -> Result<bool, erp_eventlog::LoadError> {
    let loaded = erp_eventlog::load::<Customer>(conn, id, crate::upcasters()).await?;
    Ok(loaded.aggregate.exists() && !loaded.aggregate.is_archived())
}

fn rejected(e: CrmError) -> CommandError<CrmError> {
    CommandError::Execute(erp_eventlog::ExecuteError::Rejected(e))
}
