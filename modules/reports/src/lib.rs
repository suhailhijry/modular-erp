//! Figures that agree with the books.
//!
//! # Why this is a module and not four screens reading four groups
//!
//! A dashboard mixing sales, bookings, takings and payroll looks like it must
//! read four projection groups. **L3 forbids that**, and not out of tidiness: a
//! group is the unit of consistency, so four checkpoints can be at four
//! positions, and a total read across them is a number that was never true at
//! any moment.
//!
//! The system this phase was read against made exactly that mistake. Its
//! projectors declare which *other projections* they read, and it needed a
//! bespoke check to police the rebuild order that created.
//!
//! So a report module **subscribes to the log**. It decodes
//! `sales::InvoiceEvent`, `booking::ReservationEvent`, `pos::ShiftEvent` and
//! `payroll::RunEvent`, and maintains its own group on one checkpoint. Every
//! figure on a screen built from it was true at one position, together.
//!
//! # What that costs, honestly
//!
//! **It keeps its own copies of what it needs.** A credit note carries the
//! credit note's number and not the invoice's amounts, so this module remembers
//! what each invoice came to; a booking's stage change carries a stage and not
//! the resources it holds, so it remembers those too. Those are working tables,
//! not figures anybody reads, and they are the price of not reaching into
//! another group.
//!
//! It is the right price. The alternative is a report that is occasionally
//! wrong in a way nobody can reproduce.
//!
//! # A discrepancy is a failure, not a coloured cell
//!
//! [`reconciles`] asserts that what this module says was sold equals what the
//! ledger says was earned, and the worker runs it as a health check. **L6**: a
//! report that quietly disagrees with the books is worse than no report,
//! because somebody acts on it.
//!
//! The warning that shaped this, from the system this was read against: its
//! customer statement is built from invoices rather than from the ledger,
//! because the ledger was unfinished. Two financial truths that disagree is
//! what this module exists to prevent.

pub mod http;
pub mod messages;

mod projections;
mod reconcile;

pub use projections::{
    Book, Counter, Diary, MONTHS, PeopleCostRow, Reports, Revenue, RevenueRow, TakingsRow,
    UtilisationRow, Wages, people_cost, projections, revenue, takings, utilisation,
};
pub use reconcile::{Discrepancy, reconciles};

use erp_i18n::StaticCatalog;

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Reports as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Reports as erp_projection::ProjectionGroup>::NAME,
    <Reports as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_reports; SET search_path TO proj_reports, public;",
    )
    .execute(&mut *conn)
    .await?;

    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// What a tenant enabling this module needs installed.
///
/// **`ledger`, and nothing else.** The reconciliation is against the ledger, so
/// that one is required; `sales`, `booking`, `pos` and `payroll` are not,
/// because a business with only a till still wants its takings reported and a
/// report of a module nobody enabled is an empty table rather than an error.
///
/// The crate dependencies and the entitlement dependencies are different
/// things, which is the distinction `sales` and `crm` already draw.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["ledger"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("reports")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// **All of them belong to other modules**, which is what subscribing means.
/// This module declares no events of its own: it has nothing to say that is not
/// already in the log.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        erp_eventlog::Upcasters::new()
            .also(sales::upcasters())
            .also(booking::upcasters())
            .also(pos::upcasters())
            .also(payroll::upcasters())
            // **The ledger's too.** The §10b reconciliation is against this
            // group's own copy of the books, so `ledger.entry.posted` is an
            // event this module reads like any other. Leaving it out made
            // every projection run stop at the first journal entry, which is
            // the right failure (L6) and was found by the first test that ran.
            .also(ledger::upcasters())
    })
}
