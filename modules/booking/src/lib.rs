//! Reservations.
//!
//! The half of the wedge the accounting vendors do not have. Three of the
//! booking product this was measured against are *integrations to* accounting
//! systems — Qoyod, Odoo, Daftra — which is the shape of a company that has one
//! half and buys the other. A salon today keeps its diary in one product and
//! its books in another and reconciles them by hand.
//!
//! # What is here and what is a layer down
//!
//! `erp_occupancy` answers one question: **does one more fit?** It knows a
//! resource has a capacity and that intervals overlap, and nothing else. This
//! module knows what is being booked, who it is for, what stage it has reached
//! and when a resource is *offered* — and it calls the engine for the one
//! question the engine owns.
//!
//! The split is what makes a salon, a clinic, a restaurant, a hotel, a class
//! and a museum the same code. The moment a rule in here branches on what kind
//! of thing is being booked, that has stopped being true.
//!
//! # One lifecycle, once
//!
//! [`Stage`] is five stages and two ends, and
//! [`ReservationEvent::Moved`](reservation::ReservationEvent::Moved) is the one
//! event that walks it. The system this was read against writes the same
//! lifecycle three times over — once for seats, once for showers, once for
//! services — and most of its seventy reservation events are that repetition.
//!
//! # The customer is a resource
//!
//! Held at capacity one, under a reserved id prefix, in the same engine as
//! every chair. That is the whole of "they are already in another chair": no
//! second table, no special case, and the same concurrency guarantee a stylist
//! gets. See [`commands::customer_resource`].
//!
//! # What is deliberately absent
//!
//! **Money.** A reservation carries no price, no tax and no ledger posting.
//! Pricing is one pure function and it arrives in 8d; invoicing a completed
//! booking is after that. Putting a number on a line now would mean writing the
//! pricing rules twice.
//!
//! **Blueprints.** The four fixtures that prove this is general — salon,
//! restaurant, hotel, class, gym, ticketed slot — are 8b, and each is a
//! configuration rather than a branch. If one of them needs a code change, this
//! module is not finished.

pub mod availability;
pub mod calendar;
pub mod http;
pub mod messages;

mod commands;
pub mod pricing;
mod projections;
mod reservation;
mod resource;
pub mod trades;

pub use availability::{Availability, BadRule, any_covers};
pub use calendar::{Calendar, NotAnOffset};
pub use commands::{
    Amendment, Booking as Draft, BookingError, CUSTOMER_PREFIX, Details, amend_resource, assign,
    customer_resource, declare_resource, fit_out, move_to, reschedule, reserve, restore_resource,
    schedule_resource, withdraw_resource,
};
pub use pricing::{
    Allowance, Applied, Band, Charge, Charged, PriceError, PublicBooking, Tariff, price,
};
pub use projections::{
    Booking, ReservationDetail, ReservationLine, ReservationSummary, Reservations, ResourceDetail,
    ResourceSummary, Resources, projections, reservation, reservations, resource, resources,
    stages,
};
pub use reservation::{
    Customer, DraftLine, Held, Line, Reservation, ReservationEvent, Stage, UnknownStage,
};
pub use resource::{Kind, Resource, ResourceEvent, UnknownKind};
pub use trades::{FittedOut, TRADES, TemplateHours, TemplateResource, Trade, trade};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Booking as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Booking as erp_projection::ProjectionGroup>::NAME,
    <Booking as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
///
/// Idempotent, and deliberately not a numbered migration chain. Everything it
/// creates is derived from the event log, so a change drops and rebuilds.
///
/// **It does not create the occupancy tables.** Those are in the tenant
/// migration chain, every tenant has them, and a rebuild must not go near them
/// — see `migrations/tenant/0007_occupancy.sql`.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_booking; SET search_path TO proj_booking, public;",
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
/// **`crm` is required**, and this is the one place in the codebase where that
/// kind of dependency is real rather than optional. `sales` can issue an
/// invoice to a walk-in with a name and no record; a diary cannot, because the
/// thing that stops a customer being booked into two chairs at once is their
/// record being a resource. A booking with no customer at all is a slot nobody
/// asked for.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["crm"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("booking")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        ReservationEvent::NAMES
            .iter()
            .chain(ResourceEvent::NAMES.iter())
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
