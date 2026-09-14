//! The bell inside the product.
//!
//! # What this is, in one sentence
//!
//! A notification is a **durable record first and a live signal second** — one
//! that only existed on a socket did not happen for whoever was at lunch.
//!
//! ```text
//! notifications::announce(&mut tx, &Announcing {
//!     kind: Kind::BookingReserved,
//!     subject: Subject::new(Topic::Reservation, booking),
//!     to: Vec::new(),
//!     at: now,
//! }, &metadata).await?;
//! ```
//!
//! The caller supplies what happened and about which record. Who hears about
//! it, in what language, and on which channels is resolved here.
//!
//! # The layering rule, which is not negotiable
//!
//! ```text
//! notifications  →  messaging  →  booking, crm, hr, inventory, sales
//!         ↑
//! erp-api, bin/worker.rs   (the composition roots)
//! ```
//!
//! Announcing needs an audience resolved, which is `messaging`'s job, which
//! reads the domain modules' read models. **So a domain module can never
//! announce**: `booking → notifications → messaging → booking` is a cycle cargo
//! would refuse to build.
//!
//! Announcements are therefore raised from the composition roots — worker jobs
//! and API handlers — which is where three of the four producers already lived.
//! In practice this costs nothing: what a bell is for is telling somebody about
//! something they were *not* doing, and the things a person did themselves need
//! no announcement.
//!
//! # Why every producer can be a scan that runs twice
//!
//! The aggregate id is derived — `Uuid::new_v5` over the kind and the subject —
//! so the same thing announced again loads a notification that already exists
//! and writes nothing. That is what removes the queue: no cursor, no checkpoint
//! table, no exactly-once delivery to get wrong. A producer sweeps a read model
//! for what is worth telling somebody and announces all of it, every tick.
//!
//! # What is deliberately not here
//!
//! **A customer inbox.** `Audience::Client` resolves to nobody in-system
//! because a customer has no login; reaching customers is `messaging::send`'s
//! job. A portal is a product decision, not a notification mechanism.
//!
//! **Digests and quiet hours.** "Not between 22:00 and 07:00" is a scheduling
//! problem, and [`Preferences`] is where it will attach when somebody asks.
//!
//! **A watermark on read state.** See [`person`]: events apply in position
//! order, so "everything unread now" is exact during a rebuild too.

pub mod announce;
pub mod commands;
pub mod copy;
pub mod http;
pub mod kind;
pub mod messages;
pub mod notification;
pub mod person;
pub mod projections;
pub mod sweep;

pub use announce::{AnnounceError, Announced, Announcing, announce};
pub use commands::{NotificationError, read, read_all, set_preferences};
pub use copy::Copy;
pub use kind::{Kind, UnknownKind};
pub use notification::{Notification, NotificationEvent, Wording};
pub use person::{Person as Preferences, PersonEvent};
pub use projections::{
    Inbox, InboxRow, Notifications, announced_subjects, inbox, preferences, preferences_for,
    projections, unread, unread_ids,
};
pub use sweep::{SweepError, Swept, announce_all};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Notifications as erp_projection::ProjectionGroup>::NAME;

/// **What somebody who has never said gets.**
///
/// The bell, and nothing that costs money. A default that spends is a default
/// nobody chose: a tenant who wants every arrival texted to a manager can say
/// so, and until they do, nothing is billed for a notification.
pub const DEFAULT_CHANNELS: [messaging::Channel; 1] = [messaging::Channel::InSystem];

const GROUPS: &[(&str, &str, i16)] = &[(
    <Notifications as erp_projection::ProjectionGroup>::NAME,
    <Notifications as erp_projection::ProjectionGroup>::SCHEMA,
    <Notifications as erp_projection::ProjectionGroup>::VERSION,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_notifications; \
         SET search_path TO proj_notifications, public;",
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
/// **`messaging`**, which is where an audience is resolved and where a tenant's
/// own wording for a kind lives. Everything else is optional by construction: a
/// tenant without `booking` simply never has a booking announced.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["messaging"])
    .reading(&["messaging"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("notifications")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        NotificationEvent::NAMES
            .iter()
            .chain(PersonEvent::NAMES.iter())
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
