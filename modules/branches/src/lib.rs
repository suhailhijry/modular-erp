//! Places a business trades from, and the dimension every document is reported
//! by.
//!
//! # Why this is a module of its own
//!
//! Because everything else depends on it and it depends on nothing. A branch is
//! a leaf, like `crm`: `sales`, `pos` and `booking` name one on a document, and
//! `ledger` carries it through to the posting so a trial balance can be read per
//! place. Putting it inside any of them would make the other three depend on
//! that one.
//!
//! # What "a dimension" means here, concretely
//!
//! The branch on a document is an **opaque id**, checked against the log at the
//! moment it is written and never joined to afterwards. No module reads
//! `proj_branches` — L3 forbids it — so a report that wants a branch's *name*
//! reads this module's group and the numbers from another, which is the same
//! arrangement `crm` and `sales` already have.
//!
//! # Where the branch is not, and why that is the honest answer
//!
//! **A per-branch trial balance does not have to balance, and this module does
//! not pretend otherwise.** Debits and credits balance per *currency*, which is
//! the invariant `ledger` asserts; a transfer of cash from one branch to another
//! debits one and credits the other, so each side is out by the transfer until
//! inter-branch clearing accounts exist. What Phase 16 delivers is that every
//! branch can be *reported* separately and that the branches sum to the whole —
//! not that each is a balanced set of books.

pub mod http;
pub mod messages;

mod branch;
mod commands;
mod projections;

pub use branch::{Address, BadBranch, Branch, BranchEvent, Details};
pub use commands::{
    BranchError, accepts_documents, amend_branch, close_branch, open_branch, reopen_branch,
};
pub use projections::{BranchList, BranchSummary, Branches, branch, branches, projections};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Branches as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Branches as erp_projection::ProjectionGroup>::NAME,
    <Branches as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_branches; SET search_path TO proj_branches, public;",
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
/// **Nothing.** A branch is a place, and a place needs no ledger, no customers
/// and no calendar to exist. That is what makes it safe for everything else to
/// depend on.
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
    erp_types::ModuleId::new("branches")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        BranchEvent::NAMES
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
