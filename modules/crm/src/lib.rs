//! Customers as records.
//!
//! The module everything above it points at. A reservation is made *by*
//! somebody, a package *belongs to* somebody, points *accrue to* somebody, and
//! until this existed there was nobody for any of them to be about.
//!
//! # What it does not do
//!
//! It does not replace the copy a document freezes. `sales::Customer` holds the
//! name and address that were printed, and that stays: a tax invoice is a legal
//! statement about what was issued, and a customer moving offices next year
//! must not rewrite it (L5). This module is the record beside that copy, and
//! the reference from a document to here is what lets the two be told apart.
//!
//! # Why it depends on nothing
//!
//! A customer is not an accounting document, so there is no `ledger` dependency
//! and nothing to post. That is deliberate and load-bearing: `sales`, `booking`
//! and `prepaid` can all name a customer without any of them depending on each
//! other, because the thing they share sits underneath all three.
//!
//! # The gap this leaves open, on purpose
//!
//! A customer's *whole* picture, their invoices and bookings and packages,
//! spans several projection groups, and L3 forbids reading across them. That
//! composition belongs in a module that declares all of them, the way `tax_sa`
//! nets sales against purchases. It is not this module's job and doing it here
//! would be the cross-group read the law exists to prevent.

pub mod http;
pub mod messages;

mod commands;
mod customer;
mod projections;

pub use commands::{
    CrmError, Details, accepts_documents, amend_customer, archive_customer, register_customer,
    restore_customer,
};
pub use customer::{
    Address, Contact, Customer, CustomerEvent, CustomerKind, TaxRegistration, UnknownKind,
};
pub use projections::{
    Crm, CustomerDetail, CustomerSummary, Customers, customer, customers, projections,
};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Crm as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Crm as erp_projection::ProjectionGroup>::NAME,
    <Crm as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
///
/// Idempotent, and deliberately not a numbered migration chain. Everything it
/// creates is derived from the event log, so a change drops and rebuilds.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // **The install SQL is schema-relative**, so this is what aims it — the same
    // thing `ControlPlane::install_schema` and `erp_projection::rebuild_swap` do,
    // and the reason a rebuild can aim it somewhere else.
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS proj_crm; SET search_path TO proj_crm, public;")
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;

    // Handed back the way it was found; it goes on to a pool either way.
    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// What a tenant enabling this module needs installed.
///
/// **No `requires`.** A customer list is useful on its own: a business can keep
/// one before it issues a single invoice, and a tenant that only ever wants
/// contacts should not be made to enable accounting for them.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("crm")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        CustomerEvent::NAMES
            .iter()
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn name(literal: &'static str) -> EventName {
    EventName::new(literal).expect("event names in this crate are valid literals")
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn domain(literal: &'static str) -> DomainName {
    DomainName::new(literal).expect("domain names in this crate are valid literals")
}
