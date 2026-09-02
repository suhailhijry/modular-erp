//! The org chart, and the claims that travel up it.
//!
//! # Why this module is an authorization structure and not a directory
//!
//! Every other module here answers a question about money or capacity. This one
//! answers *who may decide*. The reporting line is not a decoration on an
//! employee record — it is the structure authority travels along, and §9b's one
//! line is the whole of it:
//!
//! ```text
//! claims(node) = own(node) ∪ ⋃ claims(child) for each child
//! ```
//!
//! See [`claims`] for what follows from that, including the two things that
//! would otherwise be defects: the root being a superuser by construction, and
//! segregation of duties needing an escape hatch to survive.
//!
//! # These claims never leave the tenant
//!
//! **Decided in §9c, and it is the load-bearing decision of this phase.**
//! Authorization in the control plane answers *"may you reach this endpoint at
//! all"* — four coarse capabilities, per identity per tenant, cached across
//! nodes in Redis. These claims answer *"may you approve this particular
//! thing"*, and they are checked **inside module commands** where the decision
//! is made.
//!
//! So `Capability` and `Allowed<C>` are untouched, no `hr` type appears in
//! `erp-control` or `erp-web`, and no org event invalidates a session. A tenant
//! promoting somebody does not have to reach into the platform's cache, and a
//! tenant's own org chart cannot widen what the platform believes about that
//! tenant. `hr/tests/planes.rs` is what keeps those two lines true.
//!
//! # Where a branch fits, and the confusion it invites
//!
//! `Employee::branch` is **where this person works**. The branch in `Metadata`
//! is **where this request happened**. They differ legitimately and often — an
//! Olaya manager visiting Malaz records attendance for a Malaz shift — and a
//! report that read one where it meant the other would be wrong in a way nobody
//! notices for a quarter.
//!
//! Reads here **default** to the caller's branch and widen on request. It
//! cannot be a wall the way `ledger::post_entry_in` is one: payroll, the org
//! chart and an end-of-service calculation are company-wide by nature, and a
//! boundary that refused them would make the module unusable in its first
//! month.

pub mod claims;
pub mod http;
pub mod messages;

mod commands;
mod employee;
mod projections;

pub use claims::{Claim, ClaimError, Held, SEGREGATED, effective, holds, is_segregated};
pub use commands::{
    Hire, HrError, amend_employee, exists, grant_claim, hire, may_work_on, record_document,
    record_leaving, reparent, revoke_claim, transfer,
};
pub use employee::{
    BadEmployee, Details, Document, DocumentKind, Employee, EmployeeEvent, UnknownDocument,
};
pub use projections::{
    EmployeeList, EmployeeSummary, Expiring, Hr, employee, employees, expiring, projections,
};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Hr as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Hr as erp_projection::ProjectionGroup>::NAME,
    <Hr as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS proj_hr; SET search_path TO proj_hr, public;")
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
/// **`branches`.** A person works somewhere, and the branch on an employee is
/// checked against that module's log. Nothing else: an org chart needs no
/// ledger and no customers, and a business that wants to record who works for
/// them should not have to enable accounting first.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["branches"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("hr")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        EmployeeEvent::NAMES
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
