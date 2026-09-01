//! The reservation, as an aggregate.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_occupancy::Span;
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};

use crate::pricing::{Charge, Charged};
use serde::{Deserialize, Serialize};

/// Where a reservation is in its life.
///
/// # One lifecycle, once
///
/// That system spells this out three times over — `SeatActivated`,
/// `ShowerActivated`, `ServiceActivated`, and again for start, end, notes,
/// cancel and restore. Most of its seventy reservation events are this list
/// written once per kind of thing being booked. It is one list, it is here, and
/// [`ReservationEvent::Moved`] is the single event that walks it.
///
/// That it reached the same five stages independently is the reason to trust
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Written down. Nobody has promised anything yet.
    Reserved,
    /// The customer has confirmed, or the business has.
    Confirmed,
    /// They are here.
    Arrived,
    /// The work has started.
    InService,
    /// Done.
    Completed,
    /// Called off. The capacity goes back.
    Cancelled,
    /// They never came. The capacity goes back, and the fact is kept, because
    /// "how often does this customer not turn up" is a question every one of
    /// these businesses asks.
    NoShow,
}

impl Stage {
    /// Whether this stage can be moved to that one.
    ///
    /// **The whole lifecycle, in one match.** Written as pairs rather than as a
    /// `next()` chain on purpose: the chain cannot express the two ends, and a
    /// `match` on a pair is what makes adding an eighth stage a compile error
    /// in the one place the rules live instead of a silent gap in six.
    ///
    /// This is architecture §4's typestate argument applied where it pays.
    /// Phantom types over an event-sourced aggregate would buy nothing extra:
    /// every command starts from a `load`, so the stage is only ever known at
    /// run time, and the boundary check would be this same match with seven
    /// zero-sized types stacked on top. Nothing else in this codebase carries
    /// them either, and `Permit<C>` is the one place they earn their keep.
    #[must_use]
    pub const fn allows(self, next: Self) -> bool {
        match (self, next) {
            // Forwards along the chain, and **skipping is allowed**: a walk-in
            // arrives without ever being confirmed, and a counter that only
            // marks things done should not have to fake three steps first.
            (Self::Reserved, Self::Confirmed | Self::Arrived | Self::InService | Self::Completed)
            | (Self::Confirmed, Self::Arrived | Self::InService | Self::Completed)
            | (Self::Arrived, Self::InService | Self::Completed)
            | (Self::InService, Self::Completed)
            // Called off, while there is still something to call off.
            | (
                Self::Reserved | Self::Confirmed | Self::Arrived | Self::InService,
                Self::Cancelled,
            )
            // Not turning up is only possible before turning up. Marking
            // somebody a no-show after they arrived is a contradiction, and one
            // that would make the no-show rate a number nobody could trust.
            | (Self::Reserved | Self::Confirmed, Self::NoShow) => true,

            // Backwards, sideways, and out of an ending. A mistake is corrected
            // by a new reservation, not by reopening a closed one: the capacity
            // has already gone back and somebody else may be holding it.
            (Self::Completed | Self::Cancelled | Self::NoShow, _)
            | (
                Self::Reserved | Self::Confirmed | Self::Arrived | Self::InService,
                Self::Reserved | Self::Confirmed | Self::Arrived | Self::InService | Self::NoShow,
            ) => false,
        }
    }

    /// Whether nothing more can happen.
    #[must_use]
    pub const fn is_over(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::NoShow)
    }

    /// Whether reaching this stage gives the capacity back.
    ///
    /// Cancelled and no-show do; **completed does not**. A finished appointment
    /// held that chair, and deleting the claim would make the past look free.
    /// The claims of a completed reservation are history, and history is swept
    /// by a retention policy rather than by the command that ends the booking.
    #[must_use]
    pub const fn frees_capacity(self) -> bool {
        matches!(self, Self::Cancelled | Self::NoShow)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Confirmed => "confirmed",
            Self::Arrived => "arrived",
            Self::InService => "in_service",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::NoShow => "no_show",
        }
    }

    /// Every stage, for the transition table test and for the catalogue.
    pub const ALL: [Self; 7] = [
        Self::Reserved,
        Self::Confirmed,
        Self::Arrived,
        Self::InService,
        Self::Completed,
        Self::Cancelled,
        Self::NoShow,
    ];
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stage that is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a stage of a reservation")]
pub struct UnknownStage(pub String);

impl std::str::FromStr for Stage {
    type Err = UnknownStage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|stage| stage.as_str() == s)
            .ok_or_else(|| UnknownStage(s.to_owned()))
    }
}

/// Who the reservation is for, **and what the diary prints**.
///
/// The reference and the frozen copy, both, for the reason `sales::Customer`
/// carries both — with one extra argument that is specific to a projection:
/// `proj_booking` may not read `proj_crm` (L3), so a diary that showed the
/// current name would have to join across two projection groups running on two
/// checkpoints. Freezing the name is what makes the diary a single read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Customer {
    /// The `crm` record, when there is one. A walk-in has none, and that stays
    /// legal for the same reason it does on an invoice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<AggregateId>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
}

/// What one line takes, and how much of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub resource: AggregateId,
    /// Four covers at a table, two places in a class, one of a stylist.
    pub quantity: u16,
}

impl Held {
    #[must_use]
    pub const fn one(resource: AggregateId) -> Self {
        Self {
            resource,
            quantity: 1,
        }
    }
}

/// One thing being booked, at one time, taking some resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// What the business calls it — a service code, a room type, a table. The
    /// occupancy engine never sees this and neither does any rule here.
    pub what: String,
    pub span: Span,
    /// Every resource this line takes at once. A salon line takes the stylist
    /// and the chair; a class line takes the instructor and the room.
    pub takes: Vec<Held>,
    /// What it costs, **as it was priced at the moment of booking** (L5).
    ///
    /// `Option`, and it stays optional. Every reservation taken before pricing
    /// existed has none, and a clinic billing through an insurer never sets
    /// one. `#[serde(default)]`, so those older events still decode, which is
    /// why this needs no upcaster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge: Option<Charged>,
}

/// A line as a caller sends it: what, when, what it takes, and the rate asked
/// for. **Not what it comes to** — the band is the tenant's, resolved in the
/// command's own transaction, because a rate that changed between the request
/// and the write would stamp a booking with one that was never current.
///
/// The same split `sales` makes between `DraftLine` and `InvoiceLine`, for the
/// same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftLine {
    pub what: String,
    pub span: Span,
    pub takes: Vec<Held>,
    /// What to charge, before the tenant's bands are applied. Absent for a
    /// business that does not price here.
    pub charge: Option<Charge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReservationEvent {
    Reserved {
        /// Boxed for the reason `sales::InvoiceEvent::Issued` boxes its
        /// customer: it is the heavy variant, and `Box<T>` serialises as `T`.
        customer: Box<Customer>,
        lines: Vec<Line>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        at: Timestamp,
    },
    /// **The whole lifecycle, in one event.**
    ///
    /// The stage is a field and not seven event names, which is the correction
    /// to the system this was read against. A stage added later is a new value
    /// of an existing enum, so nothing has to learn a new name to keep reading
    /// the log.
    Moved {
        to: Stage,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
    /// Moved in time, or moved onto different resources. Both, because they are
    /// the same operation to the engine underneath: give back what was held,
    /// then take what is wanted, in one transaction.
    Rescheduled { lines: Vec<Line>, at: Timestamp },
    /// A unit picked out of a pool. See `crate::commands::assign`.
    ///
    /// Assigning again **replaces**: a room reassigned from 302 to 305 is one
    /// more of these, and the claim on 302 goes when the whole set is rebuilt.
    /// There is no unassign, because nobody has wanted a line that had a unit
    /// and then had none — and if they do, it is a second event and this one
    /// does not change.
    Assigned {
        line: u16,
        unit: AggregateId,
        at: Timestamp,
    },
}

impl DomainEvent for ReservationEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Reserved { .. } => Self::NAMES[0],
            Self::Moved { .. } => Self::NAMES[1],
            Self::Rescheduled { .. } => Self::NAMES[2],
            Self::Assigned { .. } => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

impl ReservationEvent {
    pub const NAMES: [&'static str; 4] = [
        "booking.reservation.reserved",
        "booking.reservation.moved",
        "booking.reservation.rescheduled",
        "booking.reservation.assigned",
    ];
}

#[derive(Debug, Default, Clone)]
pub struct Reservation {
    /// `None` until it is reserved, which is how "does it exist" is answered.
    pub stage: Option<Stage>,
    pub customer: Option<Customer>,
    pub lines: Vec<Line>,
    /// The unit picked for each line, by index. Sparse: most lines never have
    /// one, because most businesses book the thing itself.
    pub units: Vec<Option<AggregateId>>,
}

impl Aggregate for Reservation {
    type Event = ReservationEvent;

    fn domain() -> DomainName {
        crate::domain("booking_reservation")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ReservationEvent::Reserved {
                customer, lines, ..
            } => {
                self.stage = Some(Stage::Reserved);
                self.customer = Some((**customer).clone());
                self.lines.clone_from(lines);
                self.units = vec![None; lines.len()];
            }
            ReservationEvent::Moved { to, .. } => self.stage = Some(*to),
            ReservationEvent::Rescheduled { lines, .. } => {
                // The units go with the old lines. A line that moved in time
                // has to be assigned again, because the unit that was free at
                // the old hour is a different question from the new one.
                self.lines.clone_from(lines);
                self.units = vec![None; lines.len()];
            }
            ReservationEvent::Assigned { line, unit, .. } => {
                if let Some(slot) = self.units.get_mut(*line as usize) {
                    *slot = Some(unit.clone());
                }
            }
        }
    }
}

impl Reservation {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.stage.is_some()
    }

    /// Whether nothing more can happen to it.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.stage.is_some_and(Stage::is_over)
    }

    /// The first instant anything on this reservation starts.
    #[must_use]
    pub fn starts_at(&self) -> Option<Timestamp> {
        self.lines.iter().map(|line| line.span.from()).min()
    }

    /// The last instant anything on it ends.
    #[must_use]
    pub fn ends_at(&self) -> Option<Timestamp> {
        self.lines.iter().map(|line| line.span.until()).max()
    }

    /// Whether this line already has a unit out of its pool.
    #[must_use]
    pub fn unit_of(&self, line: usize) -> Option<&AggregateId> {
        self.units.get(line).and_then(Option::as_ref)
    }
}
