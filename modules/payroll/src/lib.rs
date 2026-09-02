//! What a business pays its people, and the entry it makes.
//!
//! # A run is a decision, not a derivation
//!
//! What somebody is paid this month depends on their salary **as it was when
//! the run was made**, and a rise recorded next week must not restate last
//! month's payslip. So the amounts are frozen onto the run's events (L5) — the
//! same argument an invoice makes about the buyer's name, and for higher
//! stakes: a payslip is a document a person files with their bank.
//!
//! # Drafting and approving are two steps
//!
//! **Drafting** computes what everybody would be paid and posts nothing.
//! **Approving** posts one journal entry. A business reads the draft, finds the
//! two people whose overtime is wrong, fixes them and runs it over — and a
//! single-step run would have posted the first attempt before anybody looked.
//!
//! # Where this module sits
//!
//! It reads `hr`'s **log** for what somebody is paid, inside the transaction
//! that posts, and writes to `ledger` through `post_entry_in` — the same seam
//! `sales` uses for an invoice, which is what makes a closed period and a
//! branch check apply here without this module repeating either.
//!
//! **It does not enumerate employees.** The caller says who is in the run, and
//! that is not laziness: enumerating means reading `proj_hr`, and L3 forbids a
//! command reading another module's projection group — while payroll is money
//! leaving the business and must not be computed from a table that may be a
//! second behind. A run is reviewed before it posts, so who is in it is a
//! decision somebody makes.
//!
//! # What is not here
//!
//! **GOSI, and the WPS file.** Both are Saudi statute, and both belong in
//! `hr_sa` for the reason VAT belongs in `tax_sa`: a country's rules are a
//! country module's, and a payroll module that knew about GOSI would have to
//! learn about every other country's equivalent.
//!
//! **The payment.** Money leaving the bank is a separate act, days later, in one
//! transfer covering everybody — and pretending the run paid people would say
//! the bank balance moved when it did not.

pub mod http;
pub mod messages;

mod commands;
mod posting;
mod projections;
mod run;

pub use commands::{PayrollError, approve_run, draft_run};
pub use posting::PostingAccounts;
pub use projections::{Payroll, PayslipRow, RunList, RunSummary, payslips, projections, run, runs};
pub use run::{NotAPeriod, Payslip, Period, Run, RunEvent};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Payroll as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Payroll as erp_projection::ProjectionGroup>::NAME,
    <Payroll as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_payroll; SET search_path TO proj_payroll, public;",
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
/// **`hr` and `ledger`.** `hr` because a run is computed from what people are
/// paid; `ledger` because it posts. Nothing else: payroll needs no customers
/// and no diary.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["hr", "ledger"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("payroll")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        RunEvent::NAMES
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
