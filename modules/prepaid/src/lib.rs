//! Everything the customer has already paid for.
//!
//! Packages, courses, deposits and subscriptions are one accounting problem
//! wearing four names: **money received now for value delivered later.**
//! Building them as separate modules would write deferred revenue four times,
//! and law L3 would then forbid the one screen every one of these businesses
//! wants — *what does this customer have with us?* Tables in different
//! projection groups never read each other, so four modules would mean four
//! reads at four checkpoints that can disagree while somebody is taking money
//! against the answer.
//!
//! The name is `prepaid` and not `entitlements` because `entitlement` already
//! means something here: the control plane's table of which **modules** a
//! tenant has switched on.
//!
//! # The two recognition models, which are not interchangeable
//!
//! | shape | liability | revenue recognised |
//! |---|---|---|
//! | Package, ten sessions | yes | when each session is **delivered** |
//! | Subscription, a gym year | yes | **ratably over the term**, attended or not |
//! | Deposit against a booking | yes | when it is drawn, or forfeited |
//! | Coupon | **no** | never — no consideration was received |
//!
//! This is the part that is an accounting error if it is got wrong, and it is
//! why [`Entitlement`] and [`Subscription`] are two aggregates rather than one
//! with a flag. Rekaz splits its own product along the same line, which is
//! evidence the distinction is real and not theoretical.
//!
//! # This module posts the deferral, not the sale
//!
//! A divergence from the plan, and the reason is ZATCA: a Saudi business
//! selling a gym year issues a tax invoice, and `sales` already does that
//! whole path. See [`posting`] for the entries and the argument.
//!
//! **No tax anywhere in this module**, which follows from the above and is the
//! reason it can stay out of a question it has no business answering.
//!
//! # Loyalty, and the shortcut that is not available
//!
//! Points, stamps and visits are the third aggregate, [`Loyalty`]. IFRS 15
//! treats what they award as a **separate performance obligation**, so part of
//! the sale that earned them is allocated to them and deferred until they are
//! honoured. The common SMB shortcut accrues a liability at redemption value
//! instead, which overstates the liability and charges the difference to
//! expense.
//!
//! Saudi requires IFRS, so **only the rigorous treatment is implemented and
//! there is no setting that selects the other**: what an accountant may not
//! choose, a tenant may not either. See [`loyalty`] for the allocation.
//!
//! # An open-value gift card, which is refused
//!
//! A card spendable on anything is a *multi-purpose voucher*: what it buys is
//! not known when it is sold, so neither is the rate it should have been taxed
//! at. Every shape here is single-purpose — what the money buys is named when
//! it is sold — and [`grant`] refuses an amount that names neither uses nor
//! what it is held against.
//!
//! The refusal is deliberate and not a gap: it is what keeps the claim above,
//! that there is no tax anywhere in this module, true by construction rather
//! than by hoping callers cooperate.

pub mod http;
pub mod messages;

mod commands;
mod entitlement;
pub mod loyalty;
mod posting;
mod projections;
mod subscription;

pub use commands::{
    Card, Earning, Grant, PointsRedemption, PrepaidError, Redemption, Term, cancel_subscription,
    earn, expire, expire_points, freeze, grant, open_card, recognise_through, redeem,
    redeem_points, renew_subscription, resume, revoke, start_subscription,
};
pub use entitlement::{Balance, Closed, Entitlement, EntitlementEvent, Reason, UnknownReason};
pub use loyalty::{Loyalty, LoyaltyEvent, Mechanic, Scheme, Tier, UnknownMechanic};
pub use posting::PostingAccounts;
pub use projections::{
    CardSummary, Cards, EntitlementSummary, Entitlements, Prepaid, SubscriptionSummary,
    Subscriptions, card, cards, entitlement, entitlements, outstanding, projections, subscription,
    subscriptions,
};
pub use subscription::{Subscription, SubscriptionEvent};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Prepaid as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Prepaid as erp_projection::ProjectionGroup>::NAME,
    <Prepaid as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_prepaid; SET search_path TO proj_prepaid, public;",
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
/// **`ledger` and `crm`, both required.** The ledger because every shape here
/// posts and a liability nobody can see is worse than no liability at all; and
/// `crm` because a balance is something a named customer holds, and a walk-in
/// with no record has nowhere to hold one.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["ledger", "crm"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("prepaid")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        EntitlementEvent::NAMES
            .iter()
            .chain(SubscriptionEvent::NAMES.iter())
            .chain(LoyaltyEvent::NAMES.iter())
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
