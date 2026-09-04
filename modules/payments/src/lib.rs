//! Collecting money through a gateway.
//!
//! # Two halves, and this is the one that knows what money means
//!
//! `erp-payments` knows an amount, a buyer and three providers' HTTP APIs. This
//! module knows what a settled payment *does*: which invoice it clears, which
//! accounts it moves, and that a fee is an expense. The split is the one
//! `modules/files` makes over `erp-storage`, and it is what lets a tenant's
//! choice of provider be a deployment fact rather than a fork.
//!
//! # A callback is a doorbell
//!
//! **None of the three gateways signs its webhook bodies.** Moyasar puts a
//! shared secret inside the JSON, Tabby offers a static header, Tamara sends a
//! token that commits to no part of the payload. So a callback proves only that
//! somebody posted to a URL, and this module never reads money out of one.
//!
//! What it does instead: authenticate the caller, take the **gateway id**, ask
//! the gateway what happened over an authenticated connection, and check the
//! amount against what was started before posting anything. A gateway id is not
//! a secret; without that check, anybody who watched a customer pay could
//! settle an invoice for a number of their choosing.
//!
//! # Most of the posting belongs to `sales`
//!
//! A settled payment is a payment against an invoice, and `sales::pay_in`
//! already clears the receivable, refuses an overpayment, and dedupes on the
//! reference — which is the gateway's own id here, so a callback delivered
//! three times records one payment. The only entry this module adds is the fee,
//! and it commits in the same transaction: a fee recorded without its payment
//! is a set of books somebody has to fix by hand.
//!
//! # What it deliberately does not do
//!
//! **Settlement.** A gateway pays out in batches, days later, net of fees. The
//! reconciliation — this payout equals these payments minus this fee — is the
//! bank-statement matching from Phase 8 pointed at a different source, and it
//! is not built. What is built is the half that makes it possible: the fee is
//! posted out of the clearing account, so the clearing account holds net, which
//! is what a payout will actually be.
//!
//! **Saved cards.** A token belongs to a customer rather than to a payment, so
//! it is a `crm` record pointing at a gateway token, and it is not here.

pub mod http;
pub mod messages;

mod commands;
mod payment;
mod posting;
mod projections;

pub use commands::{Attempt, PaymentsError, fail_in, refund_in, settle_in, start_in, void_in};
pub use payment::{Payment, PaymentEvent, Stage};
pub use posting::{PostingAccounts, Settlement, entry_for_fee};
pub use projections::{
    Collected, PaymentRow, Payments, against, by_gateway_id, payment, projections,
};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Payments as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Payments as erp_projection::ProjectionGroup>::NAME,
    <Payments as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_payments; SET search_path TO proj_payments, public;",
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
/// **Requires `sales`**, and not as a formality: a payment is collected against
/// an invoice, and settling one calls `sales::pay_in`. A tenant with this
/// module and no invoices has nothing to collect against.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["sales", "ledger"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("payments")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
///
/// **Composed with `sales`' and `ledger`'s**, because settling one payment
/// writes into all three logs in one transaction and a projection run that
/// cannot read the others' events would refuse them as undeclared.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        PaymentEvent::NAMES
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_valid() {
        for literal in PaymentEvent::NAMES {
            let _ = name(literal);
        }
        let _ = domain("payments_payment");
        let _ = module_id();
        let _ = upcasters();
    }
}
