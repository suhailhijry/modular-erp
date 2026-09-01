//! The counter: a shift, a till sale, and the variance a manager reads.
//!
//! # This module does not invoice, and that is the point
//!
//! A till transaction **is** a ZATCA simplified invoice. `sales` already builds
//! one, numbers it from a gapless statutory series, hashes it, chains it, signs
//! it and reports it within the day; `tax_sa` already decides whether a buyer's
//! VAT number makes it standard and therefore cleared before the customer walks
//! away with it.
//!
//! Writing a second document model here would duplicate VAT, discounts,
//! numbering, the ZATCA chain and credit notes — and, worse, give revenue two
//! sources of truth, so that the VAT return and the till report could disagree
//! with nobody able to say which was right.
//!
//! So `pos` composes. [`sell`] writes the shift's own event, the invoice and its
//! payment **in one transaction**, through `sales::issue_in` and
//! `sales::pay_in` — the same seam `sales` itself uses on `ledger`, and for the
//! same reason: a sale that exists in one place and not the other is a state
//! nobody could explain and nothing would clean up.
//!
//! # What is left, once the document is somebody else's
//!
//! The **drawer**. Money physically arrives in a mix of tenders, into a box
//! somebody counts at the end of the day, and the number that box is short by is
//! the only number in this module a manager actually reads. That is [`Shift`],
//! and it is the whole of what this module owns.
//!
//! | it holds | because |
//! |---|---|
//! | the opening float | the drawer did not start empty |
//! | takings by tender | only cash is in the box; a card sale settles to a bank |
//! | refunds and pay-outs | cash leaves the box for reasons that are not sales |
//! | the declared count | what a person actually counted |
//! | **the variance** | the difference, which is the number that gets read |
//!
//! # Offline is deliberately absent
//!
//! A till that queues sales locally and reconciles later is a second write path
//! with its own ordering problem, and L1 — gapless, commit-ordered positions —
//! is not negotiable. It is the kind of feature that is cheap to demonstrate and
//! expensive to be correct about. Revisit when a customer loses money to its
//! absence, not before.

pub mod http;
pub mod messages;

mod commands;
mod posting;
mod projections;
mod shift;

pub use commands::{
    Basket, Opening, PayOut, PosError, Rung, close_shift, open_shift, pay_out, sell,
};
pub use posting::PostingAccounts;
pub use projections::{Pos, ShiftSummary, Shifts, TakingRow, projections, shift, shifts, takings};
pub use shift::{Method, Shift, ShiftEvent, Takings, Tender, UnknownMethod};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Pos as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Pos as erp_projection::ProjectionGroup>::NAME,
    <Pos as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS proj_pos; SET search_path TO proj_pos, public;")
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
/// **`sales`, `ledger` and `crm`.** `sales` because every till transaction is
/// one of its invoices and this module writes none of its own; `ledger` because
/// the drawer posts; `crm` because a named customer on a receipt is a record and
/// not a spelling.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["sales", "ledger", "crm"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("pos")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        ShiftEvent::NAMES
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
