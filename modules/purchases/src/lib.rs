//! Supplier bills, and the input-tax side of a Saudi VAT return.
//!
//! The **third** module, which is a different job from the second. `sales`
//! answered "how do two modules meet"; this one answers "was that answer a
//! general one, or did it just happen to fit sales".
//!
//! # What generalised
//!
//! All of the mechanism. An aggregate, events at version 1, a projection group
//! nobody else reads, `ModuleSetup` describing what to install, a `requires` on
//! the ledger, a rejection-to-status mapping in the API, and a command that
//! writes its document and its journal entry in one transaction through
//! [`ledger::post_entry_in`]. None of that needed changing, and the closed-period
//! check arrived for free — this module never mentions a fiscal period and
//! cannot post into one, because every posting goes through the same seam.
//!
//! Exactly one thing moved: `VatCategory`, from `sales` to `ledger`. Two sibling
//! modules must not depend on each other, so what they share has to live in the
//! one they both stand on. That is a two-module rule that only a third module
//! could have tested.
//!
//! # What is genuinely different, and it is the domain rather than the plumbing
//!
//! **Sales computes tax; purchases records it.** Input VAT is reclaimed against
//! the supplier's tax invoice, so the figure in the books has to be the figure on
//! the document you hold — a recomputation landing a halala away produces a
//! reclaim that does not match its own evidence. So there is no `vat::total` here
//! and no rounding: the module validates that the stated tax is *possible*
//! (never negative, zero on anything not standard-rated, and never claimed
//! without the supplier's registration number) and then stores what it was told.
//!
//! Two smaller consequences fall out of the same fact. There is **no gapless
//! numbering**, because we did not issue this document — the supplier's own
//! number is recorded, and a duplicate of it against the same supplier is
//! refused by a unique index, since recording one bill twice is a duplicate
//! reclaim. And **exempt input tax never reaches `1200 Input VAT`**: it is
//! irrecoverable, so it is a cost of the purchase and rides on the line's own
//! account.
//!
//! # What is deliberately absent
//!
//! Supplier credit notes, supplier records, purchase orders and goods receipts.
//! Each is additive; none of them changes the shape above.

mod bill;
mod commands;
pub mod messages;
mod posting;
mod projections;

pub use bill::{Bill, BillEvent, BillLine, Supplier};
pub use commands::{Draft, Payment, PurchaseError, pay_bill, record_bill};
pub use posting::{PostingAccounts, entry_for_bill, entry_for_payment};
pub use projections::{
    BillDetail, BillLineRow, BillSummary, Bills, InputBand, InputTax, Overpaid, PaymentRow,
    Purchases, bill, bills, input_tax, overpaid, projections,
};

use spa_i18n::StaticCatalog;
use spa_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Creates this module's read models in a tenant database.
///
/// Idempotent, and deliberately not a numbered migration chain — see
/// `modules/ledger/schema/install.sql`.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Purchases as spa_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Purchases as spa_projection::ProjectionGroup>::NAME,
    <Purchases as spa_projection::ProjectionGroup>::SCHEMA,
)];

/// What a tenant enabling this module needs installed.
///
/// **Requires the ledger**, and for the same reason sales does: a bill that
/// cannot post is a filing cabinet, not accounting.
#[must_use]
pub fn setup() -> spa_control::ModuleSetup {
    spa_control::ModuleSetup::new(module_id(), include_str!("../schema/install.sql"), GROUPS)
        .requiring(&["ledger"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> spa_types::ModuleId {
    spa_types::ModuleId::new("purchases")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static spa_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<spa_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        BillEvent::NAMES
            .iter()
            .fold(spa_eventlog::Upcasters::new(), |u, n| {
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
