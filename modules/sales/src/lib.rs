//! Sales invoicing, with Saudi VAT, posting to the ledger.
//!
//! The second module, and the one that answers a question the first could only
//! assert: **how two modules meet.**
//!
//! # The answer, and the one it replaced
//!
//! The plan said "cross-module integration by event": sales would emit an event,
//! the outbox would carry a promise, and a handler would post to the ledger a
//! moment later. Building it made the cost obvious. The outbox is at-least-once
//! delivery to something this process cannot roll back — the right tool for an
//! email or a call to ZATCA, and a strictly worse one for two aggregates in the
//! same database, where atomicity is *available*. Taking the asynchronous route
//! would have traded a guarantee for a dead-letter queue and a sweeper.
//!
//! So an invoice and its journal entry commit together, and
//! [`ledger::post_entry_in`] is the seam that makes it possible: the ledger owns
//! what posting means, sales owns when. What sales does **not** get is a
//! connection to the ledger's tables — `proj_sales` and `proj_ledger` never read
//! each other (architecture L3). They share the event log, and nothing else.
//!
//! The dependency is real and declared: `sales` depends on `ledger`, a tenant
//! enabling sales needs the ledger too, and that is honest. Invoicing without
//! accounting is a document printer.
//!
//! # What is deliberately absent
//!
//! Customers as records, quantities and unit prices, partial credit notes, and
//! ZATCA clearance. The last is the module's commercial reason to exist and
//! needs a shape nobody has specified yet — a certificate and an outbox handler.
//! Every one of them is additive.

mod commands;
pub mod http;
mod invoice;
pub mod messages;
mod posting;
mod projections;
mod vat;

pub use commands::{
    Draft, Numbered, Receipt, SalesError, cancel_invoice, issue_invoice, record_payment,
};
pub use invoice::{
    Address, Customer, Discount, DraftDiscount, DraftLine, Invoice, InvoiceEvent, InvoiceLine,
};
pub use posting::{PostingAccounts, entry_for_issue, entry_for_payment};
pub use projections::{
    AgedCustomer, InvoiceDetail, InvoiceLineRow, InvoiceSummary, Invoices, Overpaid, PaymentRow,
    Sales, TaxRow, VatBand, VatReturn, invoice, invoices, overpaid, projections, receivables,
    vat_return,
};
pub use vat::{TaxBand, TaxError, Totals, Vat, VatCategory, total};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// The gapless series an invoice number comes from.
///
/// Namespaced like a configuration key, because it is the same kind of thing:
/// tenant state owned by the module that gives it meaning.
pub const INVOICE_SERIES: &str = "sales.invoice";

/// A credit note is a statutory document too, and ZATCA numbers it separately
/// from the invoices it credits.
pub const CREDIT_NOTE_SERIES: &str = "sales.credit_note";

const INVOICE_PREFIX: &str = "INV-";
const CREDIT_NOTE_PREFIX: &str = "CN-";

/// How a document number reads.
///
/// ponytail: the prefix and the five-digit width are fixed. They become a
/// `sales.numbering` configuration the first time a tenant asks — the store and
/// the typed surface both already exist (`erp_eventlog::configuration`), and the
/// only new thing would be the route. Deliberately not built on speculation,
/// but worth knowing the shape: a tenant must choose **before** their first
/// invoice, because a number that has been on a document cannot be restated.
///
/// A year-reset series (`INV-2026-00001`) is the other common shape and is a
/// bigger change than a format string: the reset has to be atomic, and "which
/// year" has to come from the tax point rather than the clock.
#[must_use]
pub fn format_number(prefix: &str, value: i64) -> String {
    format!("{prefix}{value:05}")
}

/// Creates this module's read models in a tenant database.
///
/// Idempotent, and deliberately not a numbered migration chain — see
/// `schema/install.sql`. Pair it with `erp_projection::ensure_group_schema`
/// so the checkpoint exists too.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    // **The install SQL is schema-relative**, so this is what aims it — the same
    // thing `ControlPlane::install_schema` and `erp_projection::rebuild_swap` do,
    // and the reason a rebuild can aim it somewhere else.
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS proj_sales; SET search_path TO proj_sales, public;")
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;

    // Handed back the way it was found; it goes on to a pool either way.
    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Sales as erp_projection::ProjectionGroup>::NAME;

/// This module's projection groups, as `(name, schema)`.
const GROUPS: &[(&str, &str)] = &[(
    <Sales as erp_projection::ProjectionGroup>::NAME,
    <Sales as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// What a tenant enabling this module needs installed, and what it needs
/// underneath it.
///
/// The dependency on the ledger is real: every invoice posts a journal entry in
/// the same transaction. Declaring it here rather than checking for it at each
/// call site is what lets signup, enabling later, and refusing to disable the
/// ledger all give the same answer.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    // **The ledger, and deliberately not `crm`.**
    //
    // The crate dependency and the entitlement dependency are not the same
    // thing, and this is the clearest case of it in the build. `sales` links
    // `crm` to check a customer reference at issue, but that reference is
    // *optional*: a till issuing simplified invoices to walk-ins never names a
    // customer record and should not be made to keep one.
    //
    // A tenant without `crm` who sends a customer id is refused with
    // `sales.no_such_customer`, which is the honest answer — there is no such
    // customer, because there are none.
    .requiring(&["ledger"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("sales")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        InvoiceEvent::NAMES
            .iter()
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

/// A `&'static str` from this crate, as an [`EventName`].
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
