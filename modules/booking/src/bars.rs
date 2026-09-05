//! Customers a resource must not be booked with.
//!
//! # Why this is a constraint and not a note
//!
//! Because a note is not enforced, and a note that looks like a rule is worse
//! than no note at all: a manager reads "do not book with Noura" on a customer
//! record, believes the system is holding that line, and the system has never
//! looked at it. The first booking that breaks it is made by somebody who
//! trusted a field.
//!
//! So this is checked in the command, against the log, and a reservation that
//! would break it is **refused**. There is no override. A bar that a click can
//! step past is a note again — and these are set for reasons that do not want
//! stepping past.
//!
//! # Why it is an aggregate and not a row
//!
//! Two reasons, and the second decides it.
//!
//! A bar has a history: who raised it, why, and whether it was ever lifted.
//! That is a sequence of facts about a relationship, which is what an aggregate
//! is for.
//!
//! And **a bar raised a moment ago has to stop the very next booking**. A
//! projection is another checkpoint that can lag, so a check against one would
//! have a window in which the rule is set and not yet enforced — which is the
//! window somebody raising a bar in a hurry is standing in. `branches` makes the
//! same call for the same reason, and `crm::accepts_documents` before it.
//!
//! # Only somebody the system can name
//!
//! Bars are keyed on a `crm` customer, so a walk-in with no record cannot be
//! barred from anything. That is a real limit and an honest one: barring
//! somebody means recognising them next time, and a booking that carries only a
//! typed-in name is not a recognition.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What happened to one customer's bars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BarEvent {
    /// This customer must not be booked with this resource.
    Raised {
        resource: AggregateId,
        /// Why, for the person who has to explain the refusal.
        ///
        /// **Kept short and shown to staff, never to the customer.** A refusal
        /// that repeated this back would tell somebody exactly what a colleague
        /// wrote about them.
        why: String,
        at: Timestamp,
    },
    /// The bar is over.
    ///
    /// **Lifted rather than deleted**, because whether a bar was ever in place
    /// is the question somebody asks afterwards, and an answer that quietly
    /// disappeared is no answer.
    ///
    /// No reason on this one. Who lifted it is the event's actor and when is
    /// `at`, which is what the question afterwards is actually about — and a
    /// field no screen can fill is a field that is always empty.
    Lifted {
        resource: AggregateId,
        at: Timestamp,
    },
}

impl BarEvent {
    pub const NAMES: [&'static str; 2] = ["booking.bars.raised", "booking.bars.lifted"];
}

impl DomainEvent for BarEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Raised { .. } => Self::NAMES[0],
            Self::Lifted { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// Which resources one customer must not be booked with.
///
/// **One aggregate per customer, not per pair.** A reservation names several
/// resources at once, and answering "may this booking be made" has to be one
/// load rather than one per line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bars {
    /// In force now. A lifted bar leaves here and stays in the log.
    pub barred: Vec<AggregateId>,
}

impl Bars {
    /// Whether this customer must not be booked with that resource.
    #[must_use]
    pub fn against(&self, resource: &AggregateId) -> bool {
        self.barred.iter().any(|r| r == resource)
    }

    /// The first resource in this list that is barred, if any.
    ///
    /// **The first and not all of them**, because a refusal names one thing a
    /// person can act on. A list of three would be read as three problems.
    #[must_use]
    pub fn first_barred<'a>(
        &self,
        resources: impl IntoIterator<Item = &'a AggregateId>,
    ) -> Option<AggregateId> {
        resources.into_iter().find(|r| self.against(r)).cloned()
    }
}

impl Aggregate for Bars {
    type Event = BarEvent;

    fn domain() -> DomainName {
        crate::domain("booking_bars")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            BarEvent::Raised { resource, .. } => {
                if !self.against(resource) {
                    self.barred.push(resource.clone());
                }
            }
            BarEvent::Lifted { resource, .. } => self.barred.retain(|r| r != resource),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AggregateId {
        AggregateId::new(value).unwrap_or_else(|_| unreachable!())
    }

    fn raised(resource: &str) -> BarEvent {
        BarEvent::Raised {
            resource: id(resource),
            why: "a complaint".to_owned(),
            at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn lifted(resource: &str) -> BarEvent {
        BarEvent::Lifted {
            resource: id(resource),
            at: Timestamp::from(chrono::Utc::now()),
        }
    }

    fn replay(events: &[BarEvent]) -> Bars {
        let mut bars = Bars::default();
        for event in events {
            Aggregate::apply(&mut bars, event);
        }
        bars
    }

    #[test]
    fn a_bar_holds_until_it_is_lifted() {
        let bars = replay(&[raised("stylist-1")]);
        assert!(bars.against(&id("stylist-1")));
        assert!(!bars.against(&id("stylist-2")));

        let after = replay(&[raised("stylist-1"), lifted("stylist-1")]);
        assert!(!after.against(&id("stylist-1")));
    }

    /// Raising the same bar twice is one bar, so lifting it once is enough.
    /// Otherwise a second click would leave a rule nobody could see to remove.
    #[test]
    fn raising_the_same_bar_twice_leaves_one() {
        let bars = replay(&[raised("stylist-1"), raised("stylist-1")]);
        assert_eq!(bars.barred.len(), 1);

        let after = replay(&[
            raised("stylist-1"),
            raised("stylist-1"),
            lifted("stylist-1"),
        ]);
        assert!(
            !after.against(&id("stylist-1")),
            "one lift left a bar behind"
        );
    }

    /// **The refusal names one thing.** A booking that takes a stylist and a
    /// chair, where the stylist is barred, is refused because of the stylist.
    #[test]
    fn a_booking_is_refused_by_the_first_thing_that_is_barred() {
        let bars = replay(&[raised("stylist-1")]);
        let takes = [id("chair-1"), id("stylist-1"), id("room-1")];
        assert_eq!(bars.first_barred(takes.iter()), Some(id("stylist-1")));

        let clear = [id("chair-1"), id("room-1")];
        assert_eq!(bars.first_barred(clear.iter()), None);
    }

    #[test]
    fn every_event_has_a_name_and_they_are_all_different() {
        let events = [raised("a"), lifted("a")];
        let names: Vec<_> = events.iter().map(|e| e.event_name().to_string()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "{names:?}");
        assert_eq!(names.len(), BarEvent::NAMES.len());
    }
}
