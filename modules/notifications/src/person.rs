//! One person's side of the bell: what they want, and clearing it.
//!
//! # Why "everything is read" is one event and not one per notification
//!
//! Somebody with two hundred unread notifications clicking *mark all read*
//! would otherwise append two hundred events, in one transaction, on two
//! hundred aggregates. This is one event on the person.
//!
//! # Why it needs no watermark
//!
//! The projection applies events in position order, live and during a rebuild
//! alike. So "every row of this person's that exists at the moment this event
//! applies" is exactly "every notification announced before they cleared it" —
//! the ordering does the work a `read_up_to` column would otherwise do, and
//! cannot disagree with itself.
//!
//! A notification announced *after* they cleared it is inserted unread, which
//! is the answer anybody would expect.

use std::collections::BTreeMap;

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion, Timestamp};
use messaging::Channel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PersonEvent {
    /// What this person wants told to them, and where.
    ///
    /// **The whole grid, replacing.** A partial update is how two open tabs
    /// produce a setting nobody chose; a preference grid is read as "what do I
    /// want", never as a sequence of amendments.
    PreferencesSet {
        identity: String,
        /// Kind → the channels they accept. A kind that is absent falls back to
        /// [`crate::DEFAULT_CHANNELS`].
        entries: BTreeMap<String, Vec<Channel>>,
        at: Timestamp,
    },
    /// Everything in this person's inbox at this point in the log is read.
    ReadAll { identity: String, at: Timestamp },
}

impl PersonEvent {
    pub const NAMES: [&'static str; 2] = [
        "notifications.person.preferences_set",
        "notifications.person.read_all",
    ];
}

impl DomainEvent for PersonEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::PreferencesSet { .. } => Self::NAMES[0],
            Self::ReadAll { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a person before deciding.
#[derive(Debug, Default, Clone)]
pub struct Person {
    /// What they have said they want. Empty means they have never said.
    pub preferences: BTreeMap<String, Vec<Channel>>,
}

impl Aggregate for Person {
    type Event = PersonEvent;

    fn domain() -> DomainName {
        crate::domain("notifications_person")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            PersonEvent::PreferencesSet { entries, .. } => self.preferences.clone_from(entries),
            // Clearing the bell says nothing about what this person wants next
            // time, so there is nothing to fold.
            PersonEvent::ReadAll { .. } => {}
        }
    }
}
