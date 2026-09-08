//! One conversation, about one thing.
//!
//! # Why the id is derived and not created
//!
//! A thread is not a record somebody makes; it is what you get when you look at
//! a booking and ask what has been said about it. So its id is
//! `v5("{topic}:{id}")` — the thread for a reservation always has the same one,
//! nothing has to exist before a note can be written, and two people opening a
//! booking at the same moment are opening one thread rather than racing to
//! create two.
//!
//! # Why the kinds are three events and not one with a flag
//!
//! An internal note, something said to a customer and something they said back
//! are three different acts with three different consequences. A flag on one
//! event would make "did this leave the building" a value somebody could get
//! wrong; three names make it the thing that decides which arm runs.

use std::collections::VecDeque;

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use messaging::{Channel, Subject};
use serde::{Deserialize, Serialize};

/// How many inbound ids a thread remembers.
///
/// Enough that a gateway replaying a day of webhooks writes nothing twice, and
/// bounded so a conversation running for years does not carry every id it ever
/// saw. The same idiom — and the same reasoning — as `hr::Employee`'s window of
/// recent days.
pub const HEARD_WINDOW: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadEvent {
    /// Somebody wrote something down. **Internal, and it never leaves.**
    Noted { text: String, at: Timestamp },
    /// Something was said to the customer.
    ///
    /// The effect that carries it is promised in the same transaction, so there
    /// is no state in which this system believes it said something it did not.
    Said {
        text: String,
        channel: Channel,
        /// The address it went to, resolved at this moment.
        to: String,
        at: Timestamp,
    },
    /// They answered.
    Heard {
        /// The number or address it came from.
        from: String,
        text: String,
        /// **The gateway's own id**, which is what makes hearing it twice
        /// nothing. Theirs and not ours: theirs is what is stable across their
        /// retries.
        message_id: String,
        at: Timestamp,
    },
    /// A tray thread was about something after all.
    Assigned {
        topic: String,
        subject: AggregateId,
        at: Timestamp,
    },
}

impl ThreadEvent {
    pub const NAMES: [&'static str; 4] = [
        "conversations.thread.noted",
        "conversations.thread.said",
        "conversations.thread.heard",
        "conversations.thread.assigned",
    ];
}

impl DomainEvent for ThreadEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Noted { .. } => Self::NAMES[0],
            Self::Said { .. } => Self::NAMES[1],
            Self::Heard { .. } => Self::NAMES[2],
            Self::Assigned { .. } => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a thread before deciding.
#[derive(Debug, Default, Clone)]
pub struct Thread {
    /// Inbound ids recently landed here, oldest first. Bounded; see
    /// [`HEARD_WINDOW`].
    pub heard: VecDeque<String>,
    /// Where a tray thread was sent, once somebody said.
    pub assigned_to: Option<(String, AggregateId)>,
    pub messages: usize,
}

impl Aggregate for Thread {
    type Event = ThreadEvent;

    fn domain() -> DomainName {
        crate::domain("conversations_thread")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ThreadEvent::Noted { .. } | ThreadEvent::Said { .. } => self.messages += 1,
            ThreadEvent::Heard { message_id, .. } => {
                self.messages += 1;
                self.heard.push_back(message_id.clone());
                while self.heard.len() > HEARD_WINDOW {
                    self.heard.pop_front();
                }
            }
            ThreadEvent::Assigned { topic, subject, .. } => {
                self.assigned_to = Some((topic.clone(), subject.clone()));
            }
        }
    }
}

impl Thread {
    /// Whether this reply has already been landed here.
    #[must_use]
    pub fn has_heard(&self, message_id: &str) -> bool {
        self.heard.iter().any(|seen| seen == message_id)
    }
}

/// **The thread about this**, derived and never minted (L8).
///
/// The same subject is the same conversation, which is what makes a thread
/// something you open rather than something you create.
#[must_use]
pub fn thread_id(subject: &Subject) -> AggregateId {
    derived(&format!(
        "{}:{}",
        subject.topic.as_str(),
        subject.id.as_str()
    ))
}

/// **The tray thread for a number nobody is.**
///
/// Keyed by the address, so every message from one unknown number collects in
/// one place rather than scattering — which is what makes it something a person
/// can read and act on.
#[must_use]
pub fn tray_id(address: &str) -> AggregateId {
    derived(&format!("unmatched:{address}"))
}

fn derived(name: &str) -> AggregateId {
    AggregateId::new(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()).to_string())
        .unwrap_or_else(|_| unreachable!("a uuid satisfies AggregateId"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use messaging::Topic;

    fn code(id: &str) -> AggregateId {
        AggregateId::new(id).unwrap_or_else(|_| unreachable!("a literal id"))
    }

    /// **One subject, one thread — and two subjects, two.**
    #[test]
    fn a_thread_is_named_by_what_it_is_about() {
        let booking = Subject::new(Topic::Reservation, code("BK-1"));
        assert_eq!(thread_id(&booking), thread_id(&booking));
        assert_ne!(
            thread_id(&booking),
            thread_id(&Subject::new(Topic::Reservation, code("BK-2")))
        );
        // The same id under a different topic is a different conversation: an
        // invoice numbered like a booking is not that booking.
        assert_ne!(
            thread_id(&booking),
            thread_id(&Subject::new(Topic::Invoice, code("BK-1")))
        );
        assert_ne!(thread_id(&booking), tray_id("BK-1"));
    }

    /// The window forgets the oldest and keeps the newest, so a gateway
    /// replaying its recent history writes nothing twice.
    #[test]
    fn a_thread_remembers_what_it_has_already_heard() {
        let mut thread = Thread::default();
        for n in 0..=HEARD_WINDOW {
            thread.apply(&ThreadEvent::Heard {
                from: "+966500000001".to_owned(),
                text: "yes".to_owned(),
                message_id: format!("m{n}"),
                at: chrono::Utc::now(),
            });
        }
        assert!(thread.has_heard(&format!("m{HEARD_WINDOW}")));
        assert!(thread.has_heard("m1"));
        assert!(
            !thread.has_heard("m0"),
            "the window is unbounded, so a long conversation carries every id it ever saw"
        );
    }
}
