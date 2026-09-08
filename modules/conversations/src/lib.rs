//! A thread against a subject.
//!
//! # What this is, in one sentence
//!
//! Everything said about one booking, one invoice or one customer — the notes
//! staff wrote, the messages that went out, and the replies that came back — in
//! the one place somebody would look for them.
//!
//! # Not a chat room
//!
//! A chat room is findable only by having been in it. A thread is **named by
//! what it is about**: its id is `v5("{topic}:{id}")`, so opening the
//! conversation about booking `BK-1` is a computation and not a search, nothing
//! has to be created before the first note, and two people opening it at once
//! open one thread.
//!
//! # The three kinds, and why they are three events
//!
//! An internal note, something said to a customer, and something they said back
//! are three acts with three consequences. A flag on one event would make "did
//! this leave the building" a value somebody could set wrong; three names make
//! it the thing that decides which arm runs.
//!
//! # The layering rule
//!
//! ```text
//! conversations  →  messaging, crm  →  booking, sales
//!         ↑
//! erp-api, bin/worker.rs   (the composition roots)
//! ```
//!
//! Saying something resolves the client of a subject, which is `messaging`'s
//! work, which reads the domain modules. **So a domain module can never open a
//! thread** — `booking → conversations → messaging → booking` is a cycle cargo
//! would refuse — and inbound messages are landed from a worker job, which is
//! also the only place they could be: a webhook handler is given no database
//! connection.
//!
//! # How a reply finds what it answers
//!
//! An SMS arrives as a number, a body and the gateway's id. [`land`] asks
//! `messaging::last_sent_to` what was last said to that number **as of the
//! reply's own instant** — never the clock, so the answer never moves and the
//! sweep can re-run over an overlapping window without a cursor. Failing that,
//! `crm` matches the number to a customer. Failing that, it goes in the tray.
//!
//! # What is deliberately not here
//!
//! **A customer-facing view.** A conversation on the public booking page is a
//! public read surface with its own authorization story, and nobody has asked.
//!
//! **Attachments.** `files` attaches documents to a reservation already, and a
//! thread that also held them would be a second answer to "where is that
//! photo".
//!
//! **Assignment, unread state, who is handling it.** This is not a helpdesk.
//! When a business asks for a queue, that is a queue.

pub mod commands;
pub mod http;
pub mod inbound;
pub mod messages;
pub mod projections;
pub mod thread;

pub use commands::{ConversationError, assign, hear, note, say};
pub use inbound::{Inbound, Landed, Landing, PROVIDER, land};
pub use projections::{Conversations, Line, Messages, Unmatched, messages, projections, unmatched};
pub use thread::{Thread, ThreadEvent, thread_id, tray_id};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Conversations as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Conversations as erp_projection::ProjectionGroup>::NAME,
    <Conversations as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(
        "CREATE SCHEMA IF NOT EXISTS proj_conversations; \
         SET search_path TO proj_conversations, public;",
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
/// **`messaging` and `crm`.** The first resolves who a subject's customer is and
/// promises what goes out; the second turns the number a reply arrives from into
/// a person. Without either, a conversation could hold notes and nothing else.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
    .requiring(&["messaging", "crm"])
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("conversations")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        ThreadEvent::NAMES
            .iter()
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

/// The subject line an email from a thread carries.
///
/// **What the thread is about**, because a person typing into a conversation is
/// not writing a subject line and asking them for one would be asking them to
/// name something they are looking at.
#[must_use]
pub fn subject_line(subject: &messaging::Subject) -> String {
    format!("{} {}", subject.topic.as_str(), subject.id.as_str())
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
