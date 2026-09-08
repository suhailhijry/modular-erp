//! One thing the system told somebody about.
//!
//! # Why the recipients are on the event
//!
//! An audience is a **query** — "whoever runs that branch" — and the answer
//! moves. Resolving it again when somebody opens their bell would mean a
//! manager who was promoted last week sees notifications from before they were
//! anybody, and one who left stops being able to read what they were told.
//!
//! So the answer is frozen the moment it is announced, exactly as
//! `messaging::send` freezes a resolved address into the effect. Who was told
//! is part of what happened.
//!
//! # Why reading is an event
//!
//! Because read state is per person and must survive a rebuild. A `read_at`
//! column somebody updates is a fact with no record behind it: rebuild the
//! projection and everybody's inbox fills up again. The projection here is
//! derived from the log like every other, read state included.

use std::collections::{BTreeMap, BTreeSet};

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// A title and a body, in one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wording {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationEvent {
    /// The system told somebody something.
    ///
    /// **The record, written before anything leaves the building.** The effects
    /// that carry it to email or SMS are promised after this in the same
    /// transaction, so a notification never exists only as a message somebody
    /// may or may not have received.
    Announced {
        /// A [`crate::Kind`], as its string. Held as a string rather than the
        /// enum because an event is read for ever and a kind this build does
        /// not know must still load — the same reason `files` holds an owner
        /// kind as a string in its read model.
        kind: String,
        /// A `messaging::Topic`, as its string, and which one.
        topic: String,
        subject: AggregateId,
        /// The logins this reached, resolved at this moment. See the module
        /// docs for why they are frozen here.
        recipients: Vec<String>,
        /// Locale code → what it says. Both languages, rendered here.
        wording: BTreeMap<String, Wording>,
        at: Timestamp,
    },
    /// One person has seen it.
    Read { by: String, at: Timestamp },
}

impl NotificationEvent {
    pub const NAMES: [&'static str; 2] = [
        "notifications.notification.announced",
        "notifications.notification.read",
    ];
}

impl DomainEvent for NotificationEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Announced { .. } => Self::NAMES[0],
            Self::Read { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a notification before deciding.
#[derive(Debug, Default, Clone)]
pub struct Notification {
    pub announced: bool,
    /// Who it was addressed to. Empty until it is announced.
    pub recipients: Vec<String>,
    /// Who has already seen it.
    pub read: BTreeSet<String>,
}

impl Aggregate for Notification {
    type Event = NotificationEvent;

    fn domain() -> DomainName {
        crate::domain("notifications_notification")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            NotificationEvent::Announced { recipients, .. } => {
                self.announced = true;
                self.recipients.clone_from(recipients);
            }
            NotificationEvent::Read { by, .. } => {
                self.read.insert(by.clone());
            }
        }
    }
}

impl Notification {
    /// Whether this notification was addressed to somebody.
    ///
    /// **What makes a stranger a 404 rather than a 403.** A person who was not
    /// told cannot mark it read, and saying "forbidden" would confirm that a
    /// notification with that id exists.
    #[must_use]
    pub fn addressed_to(&self, identity: &str) -> bool {
        self.recipients.iter().any(|r| r == identity)
    }

    #[must_use]
    pub fn already_read_by(&self, identity: &str) -> bool {
        self.read.contains(identity)
    }
}
