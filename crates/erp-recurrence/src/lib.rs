//! When something repeats: **which days, and between which two times on those
//! days.**
//!
//! # Why this is a crate and not part of a module
//!
//! Opening hours, a stylist's shifts, a room's out-of-service week, a studio's
//! Ramadan timetable and an employee's working pattern are one shape. It lived
//! in `booking` while `booking` was the only thing that needed it — and
//! `erp-occupancy` said so at the time: *"when a resource is offered is a
//! recurrence, and it belongs in `booking`."*
//!
//! `hr` needs the same shape for a shift and cannot reach it there: `booking`
//! already depends on `hr`, because a bookable resource names an employee and a
//! lapsed work document stops the rota. The other direction would close a
//! cycle.
//!
//! So it moves down, which is the same argument that made `erp-occupancy` a
//! crate rather than part of `booking`: one idea that two modules need belongs
//! below both of them.
//!
//! # Why the error codes changed
//!
//! They were `booking.not_a_window` and six more. A shift refused with a code
//! naming a module the tenant may not have enabled is a client's problem, so
//! the codes moved with the type and are `recurrence.` now.
//!
//! **A code is a client-facing identifier and this API tells clients to branch
//! on it**, so that is a breaking change. It is free here because nothing is
//! released, and it would not have been in six months. Recorded rather than
//! done quietly.
//!
//! # What is deliberately not here
//!
//! **Anything that claims capacity.** A rule says when something is *offered*;
//! whether one more fits is `erp-occupancy`, and keeping the two apart is what
//! lets a rule be a pure predicate over a [`Span`](erp_occupancy::Span).

pub mod messages;

mod availability;
mod calendar;

pub use availability::{Availability, BadRule, any_covers};
pub use calendar::{Calendar, NotAnOffset};

/// This crate's messages, in every supported language.
pub static CATALOG: erp_i18n::StaticCatalog =
    erp_i18n::StaticCatalog::new(messages::ENTRIES, messages::CODES);
