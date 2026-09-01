//! Capacity over time.
//!
//! A chair, a shower, a treatment room, a restaurant table, a hotel room type,
//! a class, a museum time slot and the stylist who does the work are one
//! concept: **a thing with a capacity, and intervals during which some of that
//! capacity is held.** This crate is that concept and nothing else. It does not
//! know what a resource is for, and the day it does is the day it stops fitting
//! the next trade.
//!
//! # Why this is a crate and not a module
//!
//! Modules own projections, and a projection is a pure function of the log
//! (L2) that can be dropped and rebuilt at will. Occupancy is the opposite
//! kind of thing. **A read model can be rebuilt; an accepted booking cannot be
//! un-accepted.** These rows are write-side state, consulted inside the
//! transaction that adds to them, in the same category as
//! [`erp_eventlog::numbering`] — and they live in the tenant migration chain
//! for the same reason, where `rebuild_schema` cannot reach them.
//!
//! It follows that nobody enables `occupancy`. A tenant enables `booking`, and
//! `booking` links this.
//!
//! # How a caller uses it
//!
//! Every function here takes `&mut PgConnection` and **must be given a
//! transaction**. The guards are row locks, and a row lock outside a
//! transaction is released at the end of the statement that took it, which
//! would make the whole thing silently do nothing.
//!
//! ```text
//! let mut tx = db.begin().await?;
//! occupancy::declare(&mut tx, &chair, 1).await?;          // once, when set up
//! occupancy::take(&mut tx, &reservation, &claims).await?; // with the booking
//! tx.commit().await?;
//! ```
//!
//! Rescheduling is [`reschedule`], which releases before it probes so a booking
//! never conflicts with where it already was. Cancelling is [`release`], which
//! is by owner and idempotent.
//!
//! **Roll back on a refusal.** [`take`] writes each claim before probing the
//! next, so a batch refused halfway leaves the first half in your transaction.
//! The rollback is what makes a booking all or nothing; this crate never opens
//! a transaction behind your back, exactly as `sales::issue_in` does not.
//!
//! # What is deliberately absent
//!
//! **Slot granularity.** Instants are stored; fifteen-minute slots are
//! validation and display, and belong to whoever draws the calendar.
//!
//! **Buffers.** A cleaning or setup allowance widens the interval at claim
//! time, so the probe stays one comparison and this crate never learns the word.
//!
//! **Availability and downtime.** When a resource is *offered* is a recurrence,
//! and it belongs in `booking`. All this answers is whether one more fits.
//!
//! **Retirement.** [`declare`] with a capacity of zero says the same thing and
//! keeps the claims already against it.

pub mod messages;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{NaiveDate, SubsecRound, TimeDelta};
use erp_types::{AggregateId, Timestamp};
use sqlx::PgConnection;

/// This crate's messages, in every supported language.
pub static CATALOG: erp_i18n::StaticCatalog =
    erp_i18n::StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// The longest a single claim may run.
///
/// Not a business rule and not a guess about hotels: it bounds the number of
/// guard rows one claim locks, which is one per day it touches and one round
/// trip each. A year is far past anything an appointment or a stay is, and
/// something has gone wrong in the caller long before it is reached.
pub const MAX_SPAN_DAYS: i64 = 366;

/// A half-open interval of instants, `[from, until)`.
///
/// Half-open is what makes back-to-back bookings work without a fudge: a claim
/// ending at 11:00 and one starting at 11:00 do not overlap, so a salon can run
/// appointments end to end and a hotel's checkout and check-in can share a date.
///
/// # Normalised on construction
///
/// Both ends are truncated to whole seconds, because that is the granularity
/// anything here is booked at and because a comparison between two
/// representations of "the same moment" that disagree in the sixth decimal
/// place is how an overlap check silently passes. That system stores
/// wall-clock times and records exactly that failure in its own source.
///
/// [`Timestamp`] is already UTC, so there is no timezone to normalise. A
/// tenant's local day is a display concern and this crate never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span {
    from: Timestamp,
    until: Timestamp,
}

/// Why an interval is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BadSpan {
    #[error("a span must end after it starts")]
    Empty,
    #[error("a span may not run longer than {MAX_SPAN_DAYS} days")]
    TooLong,
}

impl erp_i18n::Localize for BadSpan {
    fn message(&self) -> erp_i18n::Message {
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::Empty => Message::new(messages::EMPTY_SPAN),
            Self::TooLong => {
                Message::new(messages::SPAN_TOO_LONG).with("n", MessageArg::Count(MAX_SPAN_DAYS))
            }
        }
    }
}

impl Span {
    /// An interval, normalised, or the reason it is not one.
    pub fn new(from: Timestamp, until: Timestamp) -> Result<Self, BadSpan> {
        let from = from.trunc_subsecs(0);
        let until = until.trunc_subsecs(0);
        if until <= from {
            return Err(BadSpan::Empty);
        }
        if (until - from).num_days() > MAX_SPAN_DAYS {
            return Err(BadSpan::TooLong);
        }
        Ok(Self { from, until })
    }

    #[must_use]
    pub const fn from(&self) -> Timestamp {
        self.from
    }

    #[must_use]
    pub const fn until(&self) -> Timestamp {
        self.until
    }

    /// Every UTC date this interval touches.
    ///
    /// The guard set. Two intervals that overlap are live at some instant, and
    /// that instant falls on a date both of them touch — which is the whole
    /// reason a per-day lock is enough to serialize them.
    ///
    /// UTC and not the tenant's timezone, for the same reason: the property
    /// above holds in any single consistent calendar, and choosing the tenant's
    /// would mean this crate had to learn what a tenant is.
    ///
    /// The interval is half-open, so one ending exactly at midnight stops on
    /// the day before.
    fn dates(&self) -> impl Iterator<Item = NaiveDate> + use<> {
        let last = (self.until - TimeDelta::seconds(1)).date_naive();
        self.from
            .date_naive()
            .iter_days()
            .take_while(move |day| *day <= last)
    }
}

/// One resource, one interval, one quantity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub resource: AggregateId,
    pub span: Span,
    /// How many units of the resource this takes.
    ///
    /// One stylist, one chair, but four covers at a table and two places in a
    /// class. `u16` because the largest thing anyone here books is a hall, and
    /// because it converts to the column's `INTEGER` without a fallible step.
    pub quantity: u16,
}

impl Claim {
    /// The common case: one of it.
    #[must_use]
    pub const fn one(resource: AggregateId, span: Span) -> Self {
        Self {
            resource,
            span,
            quantity: 1,
        }
    }

    #[must_use]
    pub const fn many(resource: AggregateId, span: Span, quantity: u16) -> Self {
        Self {
            resource,
            span,
            quantity,
        }
    }
}

/// Why a claim was refused.
#[derive(Debug, thiserror::Error)]
pub enum OccupancyError {
    /// There is not enough of it free.
    ///
    /// Carries the numbers rather than a bare no, because every caller wants to
    /// say *how* full: "two of six seats left" is an answer somebody can act on
    /// and "unavailable" is not.
    #[error("{resource} holds {held} of {capacity} then, so {wanted} more will not fit")]
    Overbooked {
        resource: String,
        capacity: u16,
        held: i64,
        wanted: u16,
    },
    #[error("there is no resource {0}")]
    NoSuchResource(String),
    #[error("a claim must be for at least one")]
    NothingClaimed,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for OccupancyError {
    fn message(&self) -> erp_i18n::Message {
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::Overbooked {
                resource,
                capacity,
                held,
                wanted,
            } => Message::new(messages::OVERBOOKED)
                .with("resource", MessageArg::text(resource))
                .with("capacity", MessageArg::Int(i64::from(*capacity)))
                .with("held", MessageArg::Int(*held))
                .with("wanted", MessageArg::Int(i64::from(*wanted))),
            Self::NoSuchResource(id) => {
                Message::new(messages::NO_SUCH_RESOURCE).with("resource", MessageArg::text(id))
            }
            Self::NothingClaimed => Message::new(messages::NOTHING_CLAIMED),
            // Ours, and there is nothing a user could do about it.
            Self::Database(_) => Message::new(messages::INTERNAL),
        }
    }
}

/// Records a resource and how much of it there is.
///
/// Idempotent, and the way capacity is changed. **Claims already taken stand**
/// — a room that loses a bed does not evict the guest in it — so lowering
/// capacity below what is currently held is allowed and simply means nothing
/// more fits until those claims end. Zero is how a resource is taken out of
/// service without losing its history.
pub async fn declare(
    conn: &mut PgConnection,
    resource: &AggregateId,
    capacity: u16,
) -> Result<(), OccupancyError> {
    sqlx::query!(
        "INSERT INTO occupancy_resource (id, capacity) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE
            SET capacity = EXCLUDED.capacity, updated_at = now()",
        resource.as_str(),
        i32::from(capacity),
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Takes every claim in the batch, or none of them.
///
/// **Call inside a transaction, and roll it back on a refusal.** That is not a
/// suggestion: this writes as it goes, so a batch refused on its third claim
/// has the first two sitting in your transaction, and a caller that logs the
/// error and commits anyway has written half a booking. Rolling back is what
/// makes "or none of them" true, exactly as it is for `sales::issue_in` and
/// every other write path here — the caller's transaction is the unit of
/// atomicity and this crate does not open one behind your back.
///
/// # What it does, in order
///
/// 1. Collects every `(resource, date)` the batch touches, **sorted and
///    deduplicated**, and locks each one. Sorted because two requests naming
///    the same two resources in opposite orders deadlock otherwise, which is
///    that system's recorded bug and is fixed by total order and nothing else.
/// 2. Reads the capacities, under those locks, so a concurrent [`declare`]
///    cannot land between the read and the probe.
/// 3. Then, **one claim at a time**: probe, and if it fits, insert it.
///
/// # Why one at a time
///
/// It is what checks the batch against itself. That system probed the whole
/// request and then wrote the whole request, so a booking naming the same chair
/// twice at the same hour found nothing already held, wrote both claims, and
/// double-booked the chair against itself. Inserting before the next probe
/// means the second claim sees the first, and no separate self-check exists to
/// forget. The rows are invisible to everyone else until the transaction
/// commits.
pub async fn take(
    conn: &mut PgConnection,
    owner: &AggregateId,
    claims: &[Claim],
) -> Result<(), OccupancyError> {
    if claims.is_empty() {
        return Ok(());
    }
    if claims.iter().any(|claim| claim.quantity == 0) {
        return Err(OccupancyError::NothingClaimed);
    }

    lock_guards(&mut *conn, claims).await?;
    let capacities = capacities_of(&mut *conn, claims).await?;

    for claim in claims {
        let resource = claim.resource.as_str();
        let capacity = *capacities
            .get(resource)
            .ok_or_else(|| OccupancyError::NoSuchResource(resource.to_owned()))?;
        let held = peak(&mut *conn, resource, claim.span).await?;
        if held + i64::from(claim.quantity) > i64::from(capacity) {
            return Err(OccupancyError::Overbooked {
                resource: resource.to_owned(),
                capacity,
                held,
                wanted: claim.quantity,
            });
        }

        sqlx::query!(
            "INSERT INTO occupancy_claim (resource, owner, starts_at, ends_at, quantity)
             VALUES ($1, $2, $3, $4, $5)",
            resource,
            owner.as_str(),
            claim.span.from,
            claim.span.until,
            i32::from(claim.quantity),
        )
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

/// Gives back everything an owner holds. Idempotent.
///
/// Returns how many claims went, which is how a caller tells "cancelled" from
/// "was already cancelled" without asking first. Releasing what is not held is
/// not an error: a retried cancellation must be harmless (L8).
pub async fn release(conn: &mut PgConnection, owner: &AggregateId) -> Result<u64, OccupancyError> {
    Ok(sqlx::query!(
        "DELETE FROM occupancy_claim WHERE owner = $1",
        owner.as_str()
    )
    .execute(conn)
    .await?
    .rows_affected())
}

/// Moves an owner's claims, all or nothing.
///
/// **Call inside a transaction**, and the reason is the whole point of the
/// function: the release and the take have to be one atomic step, or a
/// reschedule that cannot fit gives up the slot the booking already had.
///
/// Releasing first is also what makes a booking not conflict with itself. Its
/// old rows are gone by the time the probe runs, in this transaction and
/// nobody else's, so moving an appointment ten minutes later does not collide
/// with where it was.
///
/// A refusal here has given up the old claims and not taken the new ones, so
/// rolling back matters more than anywhere else in this crate: committing over
/// a refused reschedule loses the slot the booking already had.
pub async fn reschedule(
    conn: &mut PgConnection,
    owner: &AggregateId,
    claims: &[Claim],
) -> Result<(), OccupancyError> {
    release(&mut *conn, owner).await?;
    take(conn, owner, claims).await
}

/// How much of a resource is free for the **whole** of a span.
///
/// The number a calendar shows and a booking form checks before it submits. It
/// takes no locks, so it is what was free a moment ago; the answer that counts
/// is the one [`take`] computes under them.
pub async fn free(
    conn: &mut PgConnection,
    resource: &AggregateId,
    span: Span,
) -> Result<u16, OccupancyError> {
    let capacity: i32 = sqlx::query_scalar!(
        "SELECT capacity FROM occupancy_resource WHERE id = $1",
        resource.as_str()
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| OccupancyError::NoSuchResource(resource.to_string()))?;

    let held = peak(conn, resource.as_str(), span).await?;
    Ok(u16::try_from(i64::from(capacity) - held).unwrap_or(0))
}

/// Locks one guard row per `(resource, date)` the batch touches, in order.
///
/// The insert is sorted for the same reason the locking is: `ON CONFLICT DO
/// NOTHING` waits on a conflicting insert that has not committed, so two
/// batches inserting the same two keys in opposite orders deadlock in the
/// insert, before either reaches a single `FOR UPDATE`.
///
/// ponytail: one round trip per `(resource, date)`, because `ORDER BY` with
/// `FOR UPDATE` does not promise Postgres locks in the order it returns. At the
/// ceiling that is [`MAX_SPAN_DAYS`] round trips for one absurd claim; in
/// practice it is one or two, and a booking runs at human speed. Batch it with
/// a `LATERAL` over an ordered list if that ever shows up in a profile, and
/// prove the order before you do.
async fn lock_guards(conn: &mut PgConnection, claims: &[Claim]) -> Result<(), sqlx::Error> {
    let keys: BTreeSet<(&str, NaiveDate)> = claims
        .iter()
        .flat_map(|claim| {
            let resource = claim.resource.as_str();
            claim.span.dates().map(move |day| (resource, day))
        })
        .collect();

    let resources: Vec<String> = keys.iter().map(|(r, _)| (*r).to_owned()).collect();
    let dates: Vec<NaiveDate> = keys.iter().map(|(_, d)| *d).collect();
    sqlx::query!(
        "INSERT INTO occupancy_guard (resource, on_date)
         SELECT r, d FROM unnest($1::TEXT[], $2::DATE[]) AS k(r, d)
          ORDER BY r, d
         ON CONFLICT DO NOTHING",
        &resources[..],
        &dates[..],
    )
    .execute(&mut *conn)
    .await?;

    for (resource, day) in &keys {
        let locked: Option<i32> = sqlx::query_scalar!(
            r#"SELECT 1 AS "one!" FROM occupancy_guard
                WHERE resource = $1 AND on_date = $2
                  FOR UPDATE"#,
            resource,
            day,
        )
        .fetch_optional(&mut *conn)
        .await?;

        // Unreachable: the insert above put it there or found it there, and
        // guard rows are never deleted. Loud rather than silent, because the
        // silent version is a probe that ran with no lock at all.
        if locked.is_none() {
            return Err(sqlx::Error::RowNotFound);
        }
    }
    Ok(())
}

/// The capacity of every distinct resource in the batch, read under the locks.
async fn capacities_of(
    conn: &mut PgConnection,
    claims: &[Claim],
) -> Result<BTreeMap<String, u16>, OccupancyError> {
    let names: Vec<String> = claims
        .iter()
        .map(|claim| claim.resource.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let found = sqlx::query!(
        "SELECT id, capacity FROM occupancy_resource WHERE id = ANY($1::TEXT[])",
        &names[..]
    )
    .fetch_all(conn)
    .await?;

    let capacities: BTreeMap<String, u16> = found
        .into_iter()
        .map(|row| {
            // The column is `CHECK (capacity BETWEEN 0 AND 65535)`, so this
            // cannot narrow. Saturating rather than unwrapping keeps L6's
            // "no silent degrade" honest without a panic in a query path.
            (row.id, u16::try_from(row.capacity).unwrap_or(u16::MAX))
        })
        .collect();

    for name in &names {
        if !capacities.contains_key(name) {
            return Err(OccupancyError::NoSuchResource(name.clone()));
        }
    }
    Ok(capacities)
}

/// The most that is held at any single instant inside the span.
///
/// # Why this is a peak and not a sum
///
/// The obvious query is `SUM(quantity)` over everything that overlaps, and it
/// is wrong in a direction that refuses real business. A room type with eight
/// units and eight one-night stays spread across a week sums to eight, so a
/// guest asking for the whole week is turned away — even though only one room
/// is taken on any given night. The sum counts claims that never coexist.
///
/// So the claims are turned into `+quantity` at each start and `-quantity` at
/// each end, ordered, and run through a running total. The largest value that
/// total reaches is what is actually held at once, and it is exact.
///
/// Ordering by `(at, delta)` puts the decrements first at an equal instant,
/// which is the half-open rule again: a claim ending at 11:00 has let go before
/// one starting at 11:00 takes hold.
///
/// Claims that start before the span are already counted by the time the total
/// reaches it, and none of them can have ended, because ending before the span
/// would have excluded them. So a maximum taken over every point up to the
/// span's end is the maximum inside it.
async fn peak(conn: &mut PgConnection, resource: &str, span: Span) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"WITH overlapping AS (
               SELECT starts_at, ends_at, quantity
                 FROM occupancy_claim
                WHERE resource = $1 AND ends_at > $2 AND starts_at < $3
           ),
           edges AS (
               SELECT starts_at AS at, quantity AS delta FROM overlapping
               UNION ALL
               SELECT ends_at AS at, -quantity AS delta FROM overlapping
           ),
           running AS (
               SELECT at, SUM(delta) OVER (ORDER BY at, delta) AS held FROM edges
           )
           SELECT COALESCE(MAX(held), 0) AS "held!" FROM running WHERE at < $3"#,
        resource,
        span.from,
        span.until,
    )
    .fetch_one(conn)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse()
            .unwrap_or_else(|_| unreachable!("a literal instant"))
    }

    #[test]
    fn a_span_is_normalised_to_whole_seconds() {
        let coarse = Span::new(at("2026-03-01T10:00:00Z"), at("2026-03-01T11:00:00Z"))
            .unwrap_or_else(|_| unreachable!("an hour is a span"));
        let fine = Span::new(at("2026-03-01T10:00:00.4Z"), at("2026-03-01T11:00:00.999Z"))
            .unwrap_or_else(|_| unreachable!("an hour is a span"));
        assert_eq!(coarse, fine, "sub-second noise survived construction");
    }

    #[test]
    fn a_span_that_does_not_move_forward_is_refused() {
        let noon = at("2026-03-01T12:00:00Z");
        assert_eq!(Span::new(noon, noon), Err(BadSpan::Empty));
        assert_eq!(
            Span::new(noon, at("2026-03-01T11:00:00Z")),
            Err(BadSpan::Empty)
        );
        // Under a second is under the granularity, so it is empty too.
        assert_eq!(
            Span::new(noon, at("2026-03-01T12:00:00.5Z")),
            Err(BadSpan::Empty)
        );
        assert_eq!(
            Span::new(noon, at("2028-03-01T12:00:00Z")),
            Err(BadSpan::TooLong)
        );
    }

    /// **The guard set is what makes a per-day lock enough.**
    ///
    /// A claim has to name every day it touches, and a claim that stops
    /// exactly at midnight must not name the day after — otherwise every
    /// overnight booking would take a lock it has no business holding.
    #[test]
    fn the_guard_set_covers_every_day_the_span_touches_and_no_more() {
        let dates = |from: &str, until: &str| -> Vec<String> {
            Span::new(at(from), at(until))
                .unwrap_or_else(|_| unreachable!("a valid span"))
                .dates()
                .map(|d| d.to_string())
                .collect()
        };

        assert_eq!(
            dates("2026-03-01T10:00:00Z", "2026-03-01T11:00:00Z"),
            ["2026-03-01"]
        );
        assert_eq!(
            dates("2026-03-01T23:00:00Z", "2026-03-02T01:00:00Z"),
            ["2026-03-01", "2026-03-02"]
        );
        // Half-open: this ends *at* the second midnight and does not touch it.
        assert_eq!(
            dates("2026-03-01T00:00:00Z", "2026-03-02T00:00:00Z"),
            ["2026-03-01"]
        );
        assert_eq!(
            dates("2026-03-01T22:00:00Z", "2026-03-03T09:00:00Z"),
            ["2026-03-01", "2026-03-02", "2026-03-03"]
        );
    }

    #[test]
    fn every_message_this_crate_can_produce_is_translated() {
        erp_i18n::testing::assert_complete(&CATALOG);
    }
}
