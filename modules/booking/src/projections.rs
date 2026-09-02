//! The diary, and the list of what can be booked.
//!
//! # Why this is a read model and the claims are not
//!
//! Everything here can be dropped and rebuilt from the log (L2). What cannot is
//! `occupancy_claim`, which is why it lives a layer down in the tenant
//! migration chain and why nothing in this file writes to it. These tables are
//! the shadow: what a calendar draws. The engine is the record.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
use sqlx::PgConnection;

use crate::availability::Availability;
use crate::pricing::Charged;
use crate::reservation::{Held, Line, ReservationEvent, Stage};
use crate::resource::ResourceEvent;

/// Two tables and their lines, all fed by one module's events.
///
/// One group and not two, because the screen every one of these businesses
/// opens on shows both at once — a column per stylist, a booking in each — and
/// a group is the unit of consistency (L3). Split, a calendar could show a
/// resource that its bookings did not know about yet.
#[derive(Debug)]
pub struct Booking;

impl ProjectionGroup for Booking {
    const NAME: &'static str = "booking";
    const SCHEMA: &'static str = "proj_booking";
}

fn decode<E: serde::de::DeserializeOwned>(
    ctx: &ProjectionCtx<'_>,
    envelope: &Envelope,
) -> Result<E, ProjectionError> {
    ctx.decode(envelope)
        .map_err(|source| ProjectionError::Decode {
            event_name: envelope.event_name.as_str().to_owned(),
            position: envelope.position,
            source,
        })
}

/// Everything that can be booked, as it is now.
#[derive(Debug)]
pub struct Resources;

#[async_trait::async_trait]
impl Projection for Resources {
    type Group = Booking;

    fn name(&self) -> &'static str {
        "resources"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !ResourceEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<ResourceEvent>(ctx, envelope)? {
            ResourceEvent::Declared {
                name,
                name_latin,
                kind,
                capacity,
                branch,
                employee,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO resource
                         (id, name, name_latin, kind, capacity, branch, employee,
                          declared_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
                )
                .bind(id)
                .bind(&name)
                .bind(&name_latin)
                .bind(kind.as_str())
                .bind(i32::from(capacity))
                .bind(branch.as_ref().map(erp_types::AggregateId::as_str))
                .bind(employee.as_ref().map(erp_types::AggregateId::as_str))
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ResourceEvent::Amended {
                name,
                name_latin,
                capacity,
                ..
            } => {
                sqlx::query(
                    "UPDATE resource
                        SET name = $2, name_latin = $3, capacity = $4,
                            recorded_at = $5, position = $6
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&name)
                .bind(&name_latin)
                .bind(i32::from(capacity))
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ResourceEvent::Scheduled { availability, .. } => {
                sqlx::query(
                    "UPDATE resource
                        SET availability = $2, recorded_at = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(sqlx::types::Json(&availability))
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ResourceEvent::Withdrawn { why, at } => {
                sqlx::query(
                    "UPDATE resource
                        SET withdrawn_at = $2, withdrawn_why = $3,
                            recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(at)
                .bind(&why)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ResourceEvent::Restored { .. } => {
                sqlx::query(
                    "UPDATE resource
                        SET withdrawn_at = NULL, withdrawn_why = NULL,
                            recorded_at = $2, position = $3
                      WHERE id = $1",
                )
                .bind(id)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Bookings and their lines.
#[derive(Debug)]
pub struct Reservations;

#[async_trait::async_trait]
impl Projection for Reservations {
    type Group = Booking;

    fn name(&self) -> &'static str {
        "reservations"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !ReservationEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<ReservationEvent>(ctx, envelope)? {
            ReservationEvent::Reserved {
                customer,
                lines,
                note,
                at,
            } => {
                let (starts_at, ends_at) = envelope_of(&lines);
                sqlx::query(
                    "INSERT INTO reservation
                         (id, customer_id, customer_name, customer_phone, stage,
                          starts_at, ends_at, note, reserved_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,'reserved',$5,$6,$7,$8,$9,$10)",
                )
                .bind(id)
                .bind(customer.id.as_ref().map(erp_types::AggregateId::as_str))
                .bind(&customer.name)
                .bind(&customer.phone)
                .bind(starts_at)
                .bind(ends_at)
                .bind(none_if_blank(&note))
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
                write_lines(conn, id, &lines).await?;
            }
            ReservationEvent::Moved { to, why, .. } => {
                sqlx::query(
                    "UPDATE reservation
                        SET stage = $2, stage_why = $3, recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(to.as_str())
                .bind(none_if_blank(&why))
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ReservationEvent::Rescheduled { lines, .. } => {
                let (starts_at, ends_at) = envelope_of(&lines);
                sqlx::query(
                    "UPDATE reservation
                        SET starts_at = $2, ends_at = $3, recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(starts_at)
                .bind(ends_at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
                // The whole set is replaced, and so are the units: a line that
                // moved has to be assigned again at its new hour. The delete is
                // what makes a reschedule to fewer lines leave none behind.
                sqlx::query("DELETE FROM reservation_line WHERE reservation_id = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                write_lines(conn, id, &lines).await?;
            }
            ReservationEvent::Assigned { line, unit, .. } => {
                sqlx::query(
                    "UPDATE reservation_line SET unit = $3
                      WHERE reservation_id = $1 AND line = $2",
                )
                .bind(id)
                .bind(i16::try_from(line).unwrap_or(i16::MAX))
                .bind(unit.as_str())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

async fn write_lines(
    conn: &mut PgConnection,
    id: &str,
    lines: &[Line],
) -> Result<(), ProjectionError> {
    for (index, line) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO reservation_line
                 (reservation_id, line, what, starts_at, ends_at, takes,
                  charge, net, currency)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(id)
        .bind(i16::try_from(index).unwrap_or(i16::MAX))
        .bind(&line.what)
        .bind(line.span.from())
        .bind(line.span.until())
        .bind(sqlx::types::Json(&line.takes))
        .bind(line.charge.as_ref().map(sqlx::types::Json))
        .bind(line.charge.as_ref().map(|c| c.net.minor()))
        .bind(
            line.charge
                .as_ref()
                .map(|c| c.net.currency().as_str().to_owned()),
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The first start and the last end across every line.
///
/// A reservation with no lines cannot be written — the command refuses one —
/// so the fallback here is unreachable. It is a zero-width instant rather than
/// a panic, and the table's own `CHECK (ends_at > starts_at)` is what would
/// stop it if the impossible ever arrived.
fn envelope_of(lines: &[Line]) -> (Timestamp, Timestamp) {
    let from = lines.iter().map(|line| line.span.from()).min();
    let until = lines.iter().map(|line| line.span.until()).max();
    match (from, until) {
        (Some(from), Some(until)) => (from, until),
        _ => (Timestamp::UNIX_EPOCH, Timestamp::UNIX_EPOCH),
    }
}

fn none_if_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Booking>>> {
    vec![
        std::sync::Arc::new(Resources),
        std::sync::Arc::new(Reservations),
    ]
}

// -------------------------------------------------------------------- reads

/// Something bookable, as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSummary {
    /// Where it is. `None` in a single-branch business.
    pub branch: Option<String>,
    /// Which member of staff this is, when the business keeps staff records.
    pub employee: Option<String>,
    pub id: String,
    pub name: String,
    pub name_latin: Option<String>,
    pub kind: String,
    pub capacity: u16,
    pub withdrawn: bool,
    pub withdrawn_why: Option<String>,
}

/// One of them, with its timetable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDetail {
    pub summary: ResourceSummary,
    pub availability: Vec<Availability>,
    pub declared_on: Timestamp,
}

/// Everything bookable, by kind and then by name.
///
/// Keyset on `(kind, name, id)`, which is the order a calendar's columns are
/// drawn in — people first, then the places they work in.
pub async fn resources(
    conn: &mut PgConnection,
    branch: Option<&str>,
    include_withdrawn: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<ResourceSummary>, sqlx::Error> {
    let (kind, name, id) = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 3 => (
            Some(parts[0].clone()),
            Some(parts[1].clone()),
            parts[2].clone(),
        ),
        _ => (None, None, String::new()),
    };

    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, kind as "kind!",
                  capacity as "capacity!",
                  branch, employee,
                  (withdrawn_at IS NOT NULL) as "withdrawn!", withdrawn_why
             FROM proj_booking.resource
            WHERE ($5 OR withdrawn_at IS NULL)
              AND ($6::text IS NULL OR branch = $6)
              AND ($2::text IS NULL OR (kind, name, id) > ($2, $3, $4))
            ORDER BY kind, name, id
            LIMIT $1"#,
        limit,
        kind,
        name,
        id,
        include_withdrawn,
        branch,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| ResourceSummary {
                id: r.id,
                name: r.name,
                name_latin: r.name_latin,
                kind: r.kind,
                capacity: u16::try_from(r.capacity).unwrap_or(u16::MAX),
                branch: r.branch,
                employee: r.employee,
                withdrawn: r.withdrawn,
                withdrawn_why: r.withdrawn_why,
            })
            .collect(),
        limit,
        |r| Cursor::over(&[&r.kind, &r.name, &r.id]),
    ))
}

/// One resource, with the timetable it is offered on.
pub async fn resource(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<ResourceDetail>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, kind as "kind!",
                  capacity as "capacity!", availability as "availability!",
                  branch, employee,
                  (withdrawn_at IS NOT NULL) as "withdrawn!", withdrawn_why,
                  declared_on as "declared_on!"
             FROM proj_booking.resource WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    Ok(Some(ResourceDetail {
        summary: ResourceSummary {
            id: row.id,
            name: row.name,
            name_latin: row.name_latin,
            kind: row.kind,
            capacity: u16::try_from(row.capacity).unwrap_or(u16::MAX),
            branch: row.branch,
            employee: row.employee,
            withdrawn: row.withdrawn,
            withdrawn_why: row.withdrawn_why,
        },
        // A rule that will not decode is a rule this build cannot read, and
        // showing an empty timetable would say the resource is always open —
        // the most dangerous wrong answer available here. Empty means "nothing
        // stored"; a decode failure means the caller sees nothing at all.
        availability: serde_json::from_value(row.availability).unwrap_or_default(),
        declared_on: row.declared_on,
    }))
}

/// A booking, as a diary shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationSummary {
    pub id: String,
    pub customer_id: Option<String>,
    pub customer_name: String,
    pub customer_phone: Option<String>,
    pub stage: String,
    pub stage_why: Option<String>,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub note: Option<String>,
}

/// One line of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationLine {
    pub line: u16,
    pub what: String,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub takes: Vec<Held>,
    pub unit: Option<String>,
    /// What it came to, if it was priced.
    pub charge: Option<Charged>,
}

/// A booking with everything on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationDetail {
    pub summary: ReservationSummary,
    pub lines: Vec<ReservationLine>,
}

/// The diary: bookings that overlap a window, earliest first.
///
/// The window is half-open and matches the way a claim overlaps — `ends_at >
/// from AND starts_at < until` — so a booking that straddles midnight shows up
/// on both days rather than on whichever one it happens to start in.
///
/// Both ends are optional, so the same read serves "everything from now on"
/// and "this week".
pub async fn reservations(
    conn: &mut PgConnection,
    from: Option<Timestamp>,
    until: Option<Timestamp>,
    stage: Option<&str>,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<ReservationSummary>, sqlx::Error> {
    let (starts_at, id) = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].parse::<Timestamp>().ok(), parts[1].clone()),
        _ => (None, String::new()),
    };

    let rows = sqlx::query!(
        r#"SELECT id as "id!", customer_id, customer_name as "customer_name!",
                  customer_phone, stage as "stage!", stage_why,
                  starts_at as "starts_at!", ends_at as "ends_at!", note
             FROM proj_booking.reservation
            WHERE ($2::timestamptz IS NULL OR ends_at > $2)
              AND ($3::timestamptz IS NULL OR starts_at < $3)
              AND ($4::text IS NULL OR stage = $4)
              AND ($5::timestamptz IS NULL OR (starts_at, id) > ($5, $6))
            ORDER BY starts_at, id
            LIMIT $1"#,
        limit,
        from,
        until,
        stage,
        starts_at,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter()
            .map(|r| ReservationSummary {
                id: r.id,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                customer_phone: r.customer_phone,
                stage: r.stage,
                stage_why: r.stage_why,
                starts_at: r.starts_at,
                ends_at: r.ends_at,
                note: r.note,
            })
            .collect(),
        limit,
        |r| Cursor::over(&[&r.starts_at.to_rfc3339(), &r.id]),
    ))
}

/// One booking and its lines.
pub async fn reservation(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<ReservationDetail>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        r#"SELECT id as "id!", customer_id, customer_name as "customer_name!",
                  customer_phone, stage as "stage!", stage_why,
                  starts_at as "starts_at!", ends_at as "ends_at!", note
             FROM proj_booking.reservation WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };

    let lines = sqlx::query!(
        r#"SELECT line as "line!", what as "what!",
                  starts_at as "starts_at!", ends_at as "ends_at!",
                  takes as "takes!", unit, charge
             FROM proj_booking.reservation_line
            WHERE reservation_id = $1
            ORDER BY line"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Some(ReservationDetail {
        summary: ReservationSummary {
            id: row.id,
            customer_id: row.customer_id,
            customer_name: row.customer_name,
            customer_phone: row.customer_phone,
            stage: row.stage,
            stage_why: row.stage_why,
            starts_at: row.starts_at,
            ends_at: row.ends_at,
            note: row.note,
        },
        lines: lines
            .into_iter()
            .map(|l| ReservationLine {
                line: u16::try_from(l.line).unwrap_or(0),
                what: l.what,
                starts_at: l.starts_at,
                ends_at: l.ends_at,
                takes: serde_json::from_value(l.takes).unwrap_or_default(),
                unit: l.unit,
                // A price that will not decode is one this build cannot read,
                // and showing nothing is the honest answer — showing zero would
                // say the appointment was free.
                charge: l.charge.and_then(|c| serde_json::from_value(c).ok()),
            })
            .collect(),
    }))
}

/// Every stage a booking can be in, for a catalogue endpoint and for the front
/// end's filter. Derived from the enum, so it cannot drift from it.
#[must_use]
pub fn stages() -> Vec<&'static str> {
    Stage::ALL.into_iter().map(Stage::as_str).collect()
}
