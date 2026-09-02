//! What can be booked, as an aggregate.
//!
//! A stylist, a chair, a treatment room, a table, a room type, a class slot.
//! The occupancy engine holds the *capacity* of each of these and nothing else;
//! this holds what a person needs to see — a name, what kind of thing it is,
//! and when it is offered.
//!
//! # Why the capacity is in two places
//!
//! It is not, quite. The number lives in `occupancy_resource`, which is where a
//! booking reads it under a lock. This aggregate records the **decision** to set
//! it, so a replay can put the engine's table back and so "who changed the class
//! size, and when" is answerable. Same relationship the invoice has with its
//! gapless number: the log is the record, the table is the working state.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

use erp_recurrence::Availability;

/// A person, a place or a thing.
///
/// Display and filtering only. **No rule in this module branches on it**, and
/// the moment one does the engine has stopped being general — which is the
/// whole failure this phase exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A stylist, a nurse, an instructor, a trainer.
    Person,
    /// A room, a chair, a table, a hall, a pitch.
    Place,
    /// A machine, a bicycle, a horse, a piece of equipment.
    Thing,
}

impl Kind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Place => "place",
            Self::Thing => "thing",
        }
    }

    pub const ALL: [Self; 3] = [Self::Person, Self::Place, Self::Thing];
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a person, a place or a thing")]
pub struct UnknownKind(pub String);

impl std::str::FromStr for Kind {
    type Err = UnknownKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownKind(s.to_owned()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceEvent {
    Declared {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        kind: Kind,
        capacity: u16,
        /// **Where it is. Set once, like `kind`.**
        ///
        /// A chair does not move between branches — and if the physical one
        /// does, declaring a new resource is the honest record, because moving
        /// this field would retroactively re-attribute every booking the
        /// resource ever held to a place it was not at.
        ///
        /// Optional, and `None` on every resource declared before branches
        /// existed. That is a single-branch business, which is most of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<AggregateId>,
        /// **Which member of staff this is**, when the resource is a person.
        ///
        /// Set once, like `branch` and for the same reason: a chair booked as
        /// "Sara" is not the same resource once it is "Noura", and moving this
        /// field would retroactively re-attribute every booking to somebody who
        /// did not do the work.
        ///
        /// Optional, and the module works exactly as before without it. A
        /// business that keeps a diary and no staff records — most of them, at
        /// first — declares a person resource with no employee and nothing
        /// changes. What naming one buys is the refusal in `assign`: somebody
        /// whose iqama has lapsed may not legally be rostered, and this is what
        /// lets `booking` know.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        employee: Option<AggregateId>,
        at: Timestamp,
    },
    Amended {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name_latin: Option<String>,
        capacity: u16,
        at: Timestamp,
    },
    /// The whole timetable, replaced.
    ///
    /// Not "a rule added" and "a rule removed": a timetable is edited as a
    /// whole on every screen anybody has drawn for one, and the two-event
    /// version means a client has to diff its own form to work out which
    /// commands to send.
    Scheduled {
        availability: Vec<Availability>,
        at: Timestamp,
    },
    /// Out of service. Capacity goes to zero and existing bookings stand.
    ///
    /// Deliberately not a deletion. A chair that broke on Tuesday was still
    /// booked on Monday, and the reservations that used it have to keep making
    /// sense.
    Withdrawn {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// Back in service, at the capacity it had.
    Restored { at: Timestamp },
}

impl DomainEvent for ResourceEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Declared { .. } => Self::NAMES[0],
            Self::Amended { .. } => Self::NAMES[1],
            Self::Scheduled { .. } => Self::NAMES[2],
            Self::Withdrawn { .. } => Self::NAMES[3],
            Self::Restored { .. } => Self::NAMES[4],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl ResourceEvent {
    pub const NAMES: [&'static str; 5] = [
        "booking.resource.declared",
        "booking.resource.amended",
        "booking.resource.scheduled",
        "booking.resource.withdrawn",
        "booking.resource.restored",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Resource {
    pub declared: bool,
    pub name: String,
    pub name_latin: Option<String>,
    pub kind: Option<Kind>,
    /// Where it is. `None` on a resource declared before branches existed, or
    /// by a business that has one.
    pub branch: Option<AggregateId>,
    /// Which member of staff this is, when the business keeps staff records.
    pub employee: Option<AggregateId>,
    /// What it is set to when it is in service. **Not** what the engine holds
    /// while it is withdrawn, which is zero — see [`Self::effective_capacity`].
    pub capacity: u16,
    pub availability: Vec<Availability>,
    pub withdrawn: bool,
}

impl Aggregate for Resource {
    type Event = ResourceEvent;

    fn domain() -> DomainName {
        crate::domain("booking_resource")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ResourceEvent::Declared {
                name,
                name_latin,
                kind,
                capacity,
                branch,
                employee,
                ..
            } => {
                self.declared = true;
                self.name.clone_from(name);
                self.name_latin.clone_from(name_latin);
                self.kind = Some(*kind);
                self.capacity = *capacity;
                self.branch.clone_from(branch);
                self.employee.clone_from(employee);
            }
            ResourceEvent::Amended {
                name,
                name_latin,
                capacity,
                ..
            } => {
                self.name.clone_from(name);
                self.name_latin.clone_from(name_latin);
                self.capacity = *capacity;
            }
            ResourceEvent::Scheduled { availability, .. } => {
                self.availability.clone_from(availability);
            }
            ResourceEvent::Withdrawn { .. } => self.withdrawn = true,
            ResourceEvent::Restored { .. } => self.withdrawn = false,
        }
    }
}

impl Resource {
    /// What the occupancy engine should be holding for this resource.
    ///
    /// Zero while it is withdrawn, which is how the engine expresses out of
    /// service — it needs no second column and it keeps the claims already
    /// against it.
    #[must_use]
    pub const fn effective_capacity(&self) -> u16 {
        if self.withdrawn { 0 } else { self.capacity }
    }
}
