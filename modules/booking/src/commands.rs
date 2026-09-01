//! What a caller can ask `booking` to do.
//!
//! # Why none of these use `TenantDb::execute`
//!
//! Every one of them writes an event **and** moves capacity in
//! `erp_occupancy`, and the two have to commit together. A reservation that
//! exists without its claim is a double-booking waiting to happen; a claim
//! without its reservation is a chair nobody can free. `execute` runs exactly
//! one aggregate and commits it, so it cannot hold both — the same reason
//! `sales` opens its own transaction to write an invoice and its journal entry.
//!
//! # Where the atomicity actually comes from
//!
//! `erp_occupancy::take` writes as it goes and does not roll itself back, so
//! the transaction here is what makes a refused booking leave nothing behind.
//! Every path below either commits or rolls back, and [`attempt`] is the one
//! place that decides which.

use std::collections::BTreeMap;

use erp_eventlog::{
    Aggregate, Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, try_execute,
};
use erp_occupancy::{BadSpan, Claim, OccupancyError, Span};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, DomainName, StreamId, Timestamp};

use crate::availability::{Availability, BadRule, any_covers};
use crate::calendar::Calendar;
use crate::pricing::{PriceError, Tariff, price};
use crate::reservation::{Customer, DraftLine, Line, Reservation, ReservationEvent, Stage};
use crate::resource::{Kind, Resource, ResourceEvent};
use crate::trades::{FittedOut, Trade};

/// The prefix under which a customer is held as a resource.
///
/// See [`customer_resource`]. Reserved, so a tenant cannot declare a bookable
/// resource that would share a row with somebody's diary.
pub const CUSTOMER_PREFIX: &str = "customer.";

#[derive(Debug, thiserror::Error)]
pub enum BookingError {
    #[error("a reservation needs at least one line")]
    NothingToBook,
    #[error("a reservation needs a name to put in the diary")]
    NoName,
    #[error("a resource needs a name")]
    ResourceHasNoName,
    #[error("there is no customer {0} to book this for")]
    NoSuchCustomer(String),
    #[error("there is nothing bookable called {0}")]
    NoSuchResource(String),
    #[error("{0} is out of service")]
    Withdrawn(String),
    #[error("{resource} is not offered at that time")]
    NotOffered { resource: String },
    #[error("reservation {0} does not exist")]
    NoSuchReservation(String),
    #[error("reservation {reservation} is {stage} and nothing more can happen to it")]
    Over { reservation: String, stage: Stage },
    #[error("a reservation cannot go from {from} to {to}")]
    CannotMove { from: Stage, to: Stage },
    #[error("this reservation has no line {0}")]
    NoSuchLine(u16),
    #[error("{CUSTOMER_PREFIX} is reserved for customers and cannot start a resource name")]
    ReservedName,
    #[error("{0} cannot be used as a reference")]
    InvalidReference(String),
    #[error(transparent)]
    Occupancy(#[from] OccupancyError),
    #[error(transparent)]
    Span(#[from] BadSpan),
    #[error(transparent)]
    Rule(#[from] BadRule),
    #[error(transparent)]
    Price(#[from] PriceError),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
}

impl erp_i18n::Localize for BookingError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NothingToBook => Message::new(messages::NOTHING_TO_BOOK),
            Self::NoName => Message::new(messages::NO_NAME),
            Self::ResourceHasNoName => Message::new(messages::RESOURCE_HAS_NO_NAME),
            Self::NoSuchCustomer(id) => {
                Message::new(messages::NO_SUCH_CUSTOMER).with("customer", MessageArg::text(id))
            }
            Self::NoSuchResource(id) => {
                Message::new(messages::NO_SUCH_RESOURCE).with("resource", MessageArg::text(id))
            }
            Self::Withdrawn(id) => {
                Message::new(messages::WITHDRAWN).with("resource", MessageArg::text(id))
            }
            Self::NotOffered { resource } => {
                Message::new(messages::NOT_OFFERED).with("resource", MessageArg::text(resource))
            }
            Self::NoSuchReservation(id) => Message::new(messages::NO_SUCH_RESERVATION)
                .with("reservation", MessageArg::text(id)),
            Self::Over { stage, .. } => {
                Message::new(messages::OVER).with("stage", MessageArg::text(stage.as_str()))
            }
            Self::CannotMove { from, to } => Message::new(messages::CANNOT_MOVE)
                .with("from", MessageArg::text(from.as_str()))
                .with("to", MessageArg::text(to.as_str())),
            Self::NoSuchLine(n) => {
                Message::new(messages::NO_SUCH_LINE).with("line", MessageArg::text(n.to_string()))
            }
            Self::ReservedName => Message::new(messages::RESERVED_NAME),
            Self::InvalidReference(r) => {
                Message::new(messages::INVALID_REFERENCE).with("reference", MessageArg::text(r))
            }
            // Each already says the right thing in both languages, and the
            // occupancy one says it with the numbers a person needs.
            Self::Occupancy(e) => e.message(),
            Self::Span(e) => e.message(),
            Self::Rule(e) => e.message(),
            Self::Price(e) => e.message(),
            Self::Config(e) => e.message(),
        }
    }
}

type Refusal = CommandError<BookingError>;
type Outcome<E> = Result<Committed<E>, Refusal>;

/// Commits, rolls back and retries — the one place that decides which.
///
/// `Ok(None)` means the optimistic-concurrency check lost and the caller should
/// go round again. Rolling back on refusal is not optional: `erp_occupancy`
/// writes as it goes, so a booking refused on its third claim has the first two
/// in this transaction.
///
/// The loop itself is written out at each command rather than taken as a
/// closure. A generic `AsyncFn` helper reads better and does not compile: axum
/// needs a handler's future to be `Send`, and there is no stable way to say
/// that about the future an async closure returns. This is the shape `sales`
/// already uses, and it keeps each transaction boundary visible where the
/// command is.
async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<BookingError>>,
) -> Result<Option<T>, Refusal> {
    match outcome {
        Ok(done) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            Ok(Some(done))
        }
        Err(e) if e.is_conflict() => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Ok(None)
        }
        Err(e) => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Err(e.into())
        }
    }
}

fn contended<T>(stream: &AggregateId, domain: DomainName) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: StreamId::new(domain, stream.clone()),
        attempts: MAX_ATTEMPTS,
    }))
}

/// Everything a bookable resource is, when it is first declared.
#[derive(Debug, Clone)]
pub struct Details {
    pub name: String,
    pub name_latin: Option<String>,
    /// A person, a place or a thing. **Set once.** A chair does not become a
    /// stylist, so there is no event that changes it and [`Amendment`] does not
    /// carry one.
    pub kind: Kind,
    /// How many can be held at once. One stylist, six covers, eight rooms of a
    /// type, five hundred places in a museum slot.
    pub capacity: u16,
}

/// What can be changed about one afterwards.
///
/// [`Details`] without the kind, and a separate type rather than the same one
/// with a field to ignore: a caller passing a kind that quietly did nothing is
/// a caller who thinks they renamed a chair into a person.
#[derive(Debug, Clone)]
pub struct Amendment {
    pub name: String,
    pub name_latin: Option<String>,
    pub capacity: u16,
}

impl Details {
    fn check(&self) -> Result<(), BookingError> {
        named(&self.name)
    }
}

impl Amendment {
    fn check(&self) -> Result<(), BookingError> {
        named(&self.name)
    }
}

fn named(name: &str) -> Result<(), BookingError> {
    if name.trim().is_empty() {
        return Err(BookingError::ResourceHasNoName);
    }
    Ok(())
}

/// Everything a reservation needs.
#[derive(Debug, Clone)]
pub struct Booking {
    pub customer: Customer,
    pub lines: Vec<DraftLine>,
    pub note: String,
    /// When it was taken. Not the wall clock — a booking entered this morning
    /// for a call that came in yesterday is yesterday's.
    pub at: Timestamp,
}

// ---------------------------------------------------------------- resources

/// Records something that can be booked, and gives the engine its capacity.
///
/// Declaring the same id twice is a no-op on the log. The capacity is written
/// to `occupancy_resource` on **every** call including that one, because it is
/// working state that a rebuilt tenant has to have back and a no-op that left
/// it unset would give the next booking nothing to check against.
pub async fn declare_resource(
    db: &TenantDb,
    id: &AggregateId,
    details: &Details,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ResourceEvent> {
    details.check().map_err(rejected)?;
    if id.as_str().starts_with(CUSTOMER_PREFIX) {
        return Err(rejected(BookingError::ReservedName));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Resource, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Resource>| {
                    let resource = &loaded.aggregate;
                    if resource.declared {
                        return Ok(Decision::nothing());
                    }
                    Ok::<_, BookingError>(Decision::one(ResourceEvent::Declared {
                        name: details.name.clone(),
                        name_latin: details.name_latin.clone(),
                        kind: details.kind,
                        capacity: details.capacity,
                        at,
                    }))
                },
            )
            .await?;

            let loaded = erp_eventlog::load::<Resource>(&mut *conn, id, crate::upcasters()).await?;
            erp_occupancy::declare(&mut *conn, id, loaded.aggregate.effective_capacity())
                .await
                .map_err(|e| ExecuteError::Rejected(BookingError::Occupancy(e)))?;
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Resource::domain())
}

/// Changes a resource's name or its capacity.
///
/// **Lowering capacity below what is held is allowed** and evicts nobody. The
/// class already has twelve people in it; what changes is that the thirteenth
/// cannot join. Refusing would leave the tenant unable to record a decision
/// they have already taken.
pub async fn amend_resource(
    db: &TenantDb,
    id: &AggregateId,
    amendment: &Amendment,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ResourceEvent> {
    amendment.check().map_err(rejected)?;
    with_resource(db, id, metadata, move |resource| {
        if resource.name == amendment.name
            && resource.name_latin == amendment.name_latin
            && resource.capacity == amendment.capacity
        {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(ResourceEvent::Amended {
            name: amendment.name.clone(),
            name_latin: amendment.name_latin.clone(),
            capacity: amendment.capacity,
            at,
        }))
    })
    .await
}

/// Sets a resource's whole timetable.
pub async fn schedule_resource(
    db: &TenantDb,
    id: &AggregateId,
    availability: &[Availability],
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ResourceEvent> {
    with_resource(db, id, metadata, move |resource| {
        if resource.availability == availability {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(ResourceEvent::Scheduled {
            availability: availability.to_vec(),
            at,
        }))
    })
    .await
}

/// Takes a resource out of service. Bookings already against it stand.
pub async fn withdraw_resource(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ResourceEvent> {
    with_resource(db, id, metadata, move |resource| {
        if resource.withdrawn {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(ResourceEvent::Withdrawn {
            why: why.to_owned(),
            at,
        }))
    })
    .await
}

/// Puts it back, at the capacity it had.
pub async fn restore_resource(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ResourceEvent> {
    with_resource(db, id, metadata, move |resource| {
        if resource.withdrawn {
            Ok(Decision::one(ResourceEvent::Restored { at }))
        } else {
            Ok(Decision::nothing())
        }
    })
    .await
}

/// One decision on an existing resource, with the engine's capacity brought
/// back into line afterwards.
///
/// Every amendment can change what the engine should be holding — a new
/// capacity outright, or a withdrawal that takes it to zero — so the capacity
/// is re-declared from the aggregate after every one of them rather than at the
/// three call sites that happen to change it today.
async fn with_resource<F>(
    db: &TenantDb,
    id: &AggregateId,
    metadata: &Metadata,
    decide: F,
) -> Outcome<ResourceEvent>
where
    F: Fn(&Resource) -> Result<Decision<ResourceEvent>, BookingError> + Send + Sync,
{
    let decide = &decide;
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Resource, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Resource>| {
                    let resource = &loaded.aggregate;
                    if !resource.declared {
                        return Err(BookingError::NoSuchResource(id.to_string()));
                    }
                    decide(resource)
                },
            )
            .await?;

            let loaded = erp_eventlog::load::<Resource>(&mut *conn, id, crate::upcasters()).await?;
            erp_occupancy::declare(&mut *conn, id, loaded.aggregate.effective_capacity())
                .await
                .map_err(|e| ExecuteError::Rejected(BookingError::Occupancy(e)))?;
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Resource::domain())
}

/// Fits out a tenant for a trade: the rota a salon, a hotel or a museum starts
/// with.
///
/// **A blueprint is a list of commands (D8).** This runs `declare_resource` and
/// `schedule_resource`, the same two a person clicking through the screens
/// would run, so a trade cannot produce anything the domain would refuse.
///
/// Running it twice is safe. A resource already there is counted as skipped and
/// keeps whatever it has been renamed to, which is what makes the button
/// harmless to press again.
///
/// # Why this takes a locale
///
/// The names are what a person reads on a calendar all day, and the first
/// market is Saudi Arabia. A rota installed in English that has to be renamed
/// five times is not a starting point.
pub async fn fit_out(
    db: &TenantDb,
    trade: &Trade,
    locale: erp_i18n::Locale,
    at: Timestamp,
    metadata: &Metadata,
) -> Result<FittedOut, Refusal> {
    let timetable = trade
        .timetable()
        .map_err(|e| rejected(BookingError::Rule(e)))?;
    let mut fitted = FittedOut::default();

    for template in trade.resources {
        let id = AggregateId::new(template.id)
            .map_err(|_| rejected(BookingError::InvalidReference(template.id.to_owned())))?;
        let details = Details {
            name: template.name(locale).to_owned(),
            // The Latin spelling beside the Arabic one, and nothing beside the
            // English one — a name and its own duplicate is not two names.
            name_latin: match locale {
                erp_i18n::Locale::Arabic => Some(template.name_en.to_owned()),
                erp_i18n::Locale::English => None,
            },
            kind: template.kind,
            capacity: template.capacity,
        };

        let declared = declare_resource(db, &id, &details, at, metadata).await?;
        if declared.at.is_some() {
            fitted.declared += 1;
        } else {
            fitted.skipped += 1;
        }

        // A trade with no hours leaves everything always open, which is what a
        // hotel and a museum's own storeroom want. A resource that does not
        // keep the trade's hours is one that is offered whenever, and that is
        // an empty rule set rather than a rule saying so.
        if template.keeps_hours && !timetable.is_empty() {
            let scheduled = schedule_resource(db, &id, &timetable, at, metadata).await?;
            if scheduled.at.is_some() {
                fitted.scheduled += 1;
            }
        }
    }

    Ok(fitted)
}

// ------------------------------------------------------------- reservations

/// Takes a booking: the event and every claim it needs, together.
///
/// Reserving the same id twice is a no-op, and the second call takes no claims.
/// That gate matters more here than anywhere else in this codebase: taking them
/// again would either collide with the reservation's own rows or, on a resource
/// with room, quietly book it twice for one customer.
pub async fn reserve(
    db: &TenantDb,
    id: &AggregateId,
    booking: &Booking,
    metadata: &Metadata,
) -> Outcome<ReservationEvent> {
    if booking.lines.is_empty() {
        return Err(rejected(BookingError::NothingToBook));
    }
    if booking.customer.name.trim().is_empty() {
        return Err(rejected(BookingError::NoName));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_customer(&mut *conn, booking.customer.id.as_ref()).await?;
            let lines = priced(&mut *conn, &booking.lines).await?;
            check_offered(&mut *conn, &lines).await?;

            let committed = try_execute::<Reservation, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Reservation>| {
                    let reservation = &loaded.aggregate;
                    if reservation.exists() {
                        return Ok(Decision::nothing());
                    }
                    Ok::<_, BookingError>(Decision::one(ReservationEvent::Reserved {
                        customer: Box::new(booking.customer.clone()),
                        lines: lines.clone(),
                        note: booking.note.clone(),
                        at: booking.at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                rehold(&mut *conn, id, false).await?;
            }
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Reservation::domain())
}

/// Walks the lifecycle.
///
/// Moving to the stage it is already in is a no-op, which is what makes a
/// retried "mark them arrived" harmless. Every other move goes through
/// [`Stage::allows`], and the two that give capacity back release it in this
/// transaction.
pub async fn move_to(
    db: &TenantDb,
    id: &AggregateId,
    to: Stage,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ReservationEvent> {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Reservation, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Reservation>| {
                    let reservation = &loaded.aggregate;
                    let from = reservation
                        .stage
                        .ok_or_else(|| BookingError::NoSuchReservation(id.to_string()))?;
                    if from == to {
                        return Ok(Decision::nothing());
                    }
                    if !from.allows(to) {
                        return Err(if from.is_over() {
                            BookingError::Over {
                                reservation: id.to_string(),
                                stage: from,
                            }
                        } else {
                            BookingError::CannotMove { from, to }
                        });
                    }
                    Ok(Decision::one(ReservationEvent::Moved {
                        to,
                        why: why.to_owned(),
                        at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() && to.frees_capacity() {
                erp_occupancy::release(&mut *conn, id)
                    .await
                    .map_err(|e| ExecuteError::Rejected(BookingError::Occupancy(e)))?;
            }
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Reservation::domain())
}

/// Moves a booking in time, or onto different resources.
///
/// One command for both, because underneath they are one operation: give back
/// everything this reservation holds, then take what it wants. Giving back
/// first is what stops a booking colliding with where it already was, so
/// nudging an appointment ten minutes later works instead of being refused by
/// its own claim.
pub async fn reschedule(
    db: &TenantDb,
    id: &AggregateId,
    lines: &[DraftLine],
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ReservationEvent> {
    if lines.is_empty() {
        return Err(rejected(BookingError::NothingToBook));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let lines = priced(&mut *conn, lines).await?;
            check_offered(&mut *conn, &lines).await?;

            let committed = try_execute::<Reservation, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Reservation>| {
                    let reservation = &loaded.aggregate;
                    let stage = reservation
                        .stage
                        .ok_or_else(|| BookingError::NoSuchReservation(id.to_string()))?;
                    if stage.is_over() {
                        return Err(BookingError::Over {
                            reservation: id.to_string(),
                            stage,
                        });
                    }
                    if reservation.lines == lines {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(ReservationEvent::Rescheduled {
                        lines: lines.clone(),
                        at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                // The units go with the old lines — see `Reservation::apply` — so
                // the rebuilt set has none, and a pooled line has to be assigned
                // again at its new hour.
                rehold(&mut *conn, id, true).await?;
            }
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Reservation::domain())
}

/// Picks a unit out of a pool.
///
/// A hotel books "a double" and gives out room 302 at check-in; a salon books
/// "any stylist" and names one on the morning. The pool holds the **count** and
/// the unit holds the **identity**, so assigning takes a second claim on a
/// different resource and nothing is counted twice.
///
/// Assigning the same unit again is a no-op. Assigning a different one replaces
/// it, and the whole claim set is rebuilt, which is what gives the old unit back.
pub async fn assign(
    db: &TenantDb,
    id: &AggregateId,
    line: u16,
    unit: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome<ReservationEvent> {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            available(&mut *conn, unit).await?;

            let committed = try_execute::<Reservation, _, _>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Reservation>| {
                    let reservation = &loaded.aggregate;
                    let stage = reservation
                        .stage
                        .ok_or_else(|| BookingError::NoSuchReservation(id.to_string()))?;
                    if stage.is_over() {
                        return Err(BookingError::Over {
                            reservation: id.to_string(),
                            stage,
                        });
                    }
                    if reservation.lines.get(line as usize).is_none() {
                        return Err(BookingError::NoSuchLine(line));
                    }
                    if reservation.unit_of(line as usize) == Some(unit) {
                        return Ok(Decision::nothing());
                    }

                    Ok(Decision::one(ReservationEvent::Assigned {
                        line,
                        unit: unit.clone(),
                        at,
                    }))
                },
            )
            .await?;

            if committed.at.is_some() {
                rehold(&mut *conn, id, true).await?;
            }
            Ok(committed)
        }
        .await;
        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id, Reservation::domain())
}

// ------------------------------------------------------------------ helpers

/// Rebuilds a reservation's whole claim set **from the log** and takes it.
///
/// Reading the aggregate back rather than reusing what the caller sent is what
/// keeps the claims and the events from ever disagreeing: whatever the decision
/// actually recorded is what gets held, including the units already assigned
/// and the customer already named. Three commands need this and one derivation
/// serves all of them.
async fn rehold(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    replacing: bool,
) -> Result<(), ExecuteError<BookingError>> {
    let loaded = erp_eventlog::load::<Reservation>(&mut *conn, id, crate::upcasters())
        .await
        .map_err(ExecuteError::Load)?;
    let reservation = &loaded.aggregate;
    let claims = claims_for(
        &reservation.lines,
        &reservation.units,
        reservation
            .customer
            .as_ref()
            .and_then(|customer| customer.id.as_ref()),
    )
    .map_err(ExecuteError::Rejected)?;
    hold(conn, id, &claims, replacing).await
}

/// Takes or re-takes a whole claim set, and turns the engine's refusal into
/// this module's.
async fn hold(
    conn: &mut sqlx::PgConnection,
    owner: &AggregateId,
    claims: &[Claim],
    replacing: bool,
) -> Result<(), ExecuteError<BookingError>> {
    // Declared here rather than at the customer's own command, because a
    // customer is not a bookable resource — nobody puts one on a rota — and the
    // engine still has to know one exists before anything can be held against
    // it. Idempotent, so every booking re-asserting it costs one upsert.
    for claim in claims {
        if claim.resource.as_str().starts_with(CUSTOMER_PREFIX) {
            erp_occupancy::declare(&mut *conn, &claim.resource, 1)
                .await
                .map_err(|e| ExecuteError::Rejected(BookingError::Occupancy(e)))?;
        }
    }

    let outcome = if replacing {
        erp_occupancy::reschedule(&mut *conn, owner, claims).await
    } else {
        erp_occupancy::take(&mut *conn, owner, claims).await
    };
    outcome.map_err(|e| ExecuteError::Rejected(BookingError::Occupancy(e)))
}

/// Every claim a reservation makes, derived in one place.
///
/// The one place, deliberately: `reserve`, `reschedule` and `assign` all have
/// to arrive at the same set, and three derivations of it would be three
/// chances for one of them to forget the customer.
fn claims_for(
    lines: &[Line],
    units: &[Option<AggregateId>],
    customer: Option<&AggregateId>,
) -> Result<Vec<Claim>, BookingError> {
    let mut claims = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        for held in &line.takes {
            claims.push(Claim::many(held.resource.clone(), line.span, held.quantity));
        }
        if let Some(unit) = units.get(index).and_then(Option::as_ref) {
            claims.push(Claim::one(unit.clone(), line.span));
        }
    }

    // **The customer is a resource with a capacity of one**, which is the whole
    // of "already in another chair" — no second table, no special case, and the
    // same concurrency guarantee everything else gets.
    //
    // Once per *distinct* span, not once per line. Four seats at one showing is
    // one customer at one time and must be allowed; a haircut at ten and a
    // massage at half past is one person in two places and must not be.
    if let Some(customer) = customer {
        let resource = customer_resource(customer)?;
        let mut spans: Vec<Span> = lines.iter().map(|line| line.span).collect();
        spans.sort_unstable();
        spans.dedup();
        claims.extend(
            spans
                .into_iter()
                .map(|span| Claim::one(resource.clone(), span)),
        );
    }
    Ok(claims)
}

/// The occupancy resource that stands for a customer's own diary.
pub fn customer_resource(customer: &AggregateId) -> Result<AggregateId, BookingError> {
    AggregateId::new(format!("{CUSTOMER_PREFIX}{customer}"))
        .map_err(|_| BookingError::InvalidReference(customer.to_string()))
}

/// Prices every line against the tenant's bands, **in this transaction**.
///
/// The band is resolved here and frozen onto the line (L5), for the same
/// reason `sales` resolves the VAT rate in the transaction that issues an
/// invoice: a tenant who moves their peak hours next month must not restate
/// what was booked this month, and a client must not be able to send its own
/// idea of what peak costs.
///
/// A line with no charge stays unpriced and costs nothing. That is a business
/// that bills elsewhere, not an error.
async fn priced(
    conn: &mut sqlx::PgConnection,
    drafts: &[DraftLine],
) -> Result<Vec<Line>, ExecuteError<BookingError>> {
    let calendar = Calendar::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(BookingError::Config(e)))?;
    let tariff = Tariff::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(BookingError::Config(e)))?;

    drafts
        .iter()
        .map(|draft| {
            let charge = match &draft.charge {
                Some(charge) => Some(
                    price(charge, tariff.band_for(draft.span, calendar))
                        .map_err(|e| ExecuteError::Rejected(BookingError::Price(e)))?,
                ),
                None => None,
            };
            Ok(Line {
                what: draft.what.clone(),
                span: draft.span,
                takes: draft.takes.clone(),
                charge,
            })
        })
        .collect()
}

/// Refuses a `crm` reference nothing answers to.
///
/// Against the **log** and not `proj_crm`, for the reason `sales` does the
/// same: `crm` is another projection group on another checkpoint, and a
/// customer created a moment ago is not in that table yet.
async fn check_customer(
    conn: &mut sqlx::PgConnection,
    customer: Option<&AggregateId>,
) -> Result<(), ExecuteError<BookingError>> {
    if let Some(customer) = customer
        && !crm::accepts_documents(&mut *conn, customer)
            .await
            .map_err(ExecuteError::Load)?
    {
        return Err(ExecuteError::Rejected(BookingError::NoSuchCustomer(
            customer.to_string(),
        )));
    }
    Ok(())
}

/// Every resource a set of lines names has to exist, be in service, and be
/// **offered at the hour asked for**.
///
/// The last of those is the only thing in this module that reads a timetable,
/// and it reads it from the aggregate rather than from `proj_booking` for the
/// reason the customer check does: a rota set a moment ago has not reached the
/// projection, and refusing a booking against a shift the manager has just
/// entered is the wrong answer.
async fn check_offered(
    conn: &mut sqlx::PgConnection,
    lines: &[Line],
) -> Result<(), ExecuteError<BookingError>> {
    let calendar = Calendar::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(BookingError::Config(e)))?;
    let offset = calendar.offset();

    // One load per distinct resource, however many lines name it.
    let mut loaded: BTreeMap<&str, Resource> = BTreeMap::new();
    for line in lines {
        for held in &line.takes {
            let key = held.resource.as_str();
            if !loaded.contains_key(key) {
                let resource = available(&mut *conn, &held.resource).await?;
                loaded.insert(key, resource);
            }
            // `expect` is unreachable: the entry was just inserted.
            let Some(resource) = loaded.get(key) else {
                continue;
            };
            if !any_covers(&resource.availability, line.span, offset) {
                return Err(ExecuteError::Rejected(BookingError::NotOffered {
                    resource: key.to_owned(),
                }));
            }
        }
    }
    Ok(())
}

/// Loads a resource and refuses one that is missing or out of service.
async fn available(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
) -> Result<Resource, ExecuteError<BookingError>> {
    let loaded = erp_eventlog::load::<Resource>(&mut *conn, id, crate::upcasters())
        .await
        .map_err(ExecuteError::Load)?;
    if !loaded.aggregate.declared {
        return Err(ExecuteError::Rejected(BookingError::NoSuchResource(
            id.to_string(),
        )));
    }
    if loaded.aggregate.withdrawn {
        return Err(ExecuteError::Rejected(BookingError::Withdrawn(
            id.to_string(),
        )));
    }
    Ok(loaded.aggregate)
}

fn rejected(error: BookingError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}
