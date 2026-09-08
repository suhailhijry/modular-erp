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
use erp_types::{AggregateId, CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::bars::BarEvent;
use crate::pricing::Charged;
use crate::reservation::{Held, Line, ReservationEvent, Stage};
use crate::resource::ResourceEvent;
use erp_recurrence::Availability;

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

/// A resource as it was declared, for [`declared`].
struct Declared<'a> {
    name: &'a str,
    name_latin: Option<&'a str>,
    kind: crate::Kind,
    capacity: u16,
    rate: Option<Money>,
    branch: Option<&'a str>,
    employee: Option<&'a str>,
    at: Timestamp,
}

/// The row a declaration writes.
async fn declared(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    row: Declared<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO resource
             (id, name, name_latin, kind, capacity, branch, employee,
              declared_on, recorded_at, position, rate_minor, rate_currency)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(id)
    .bind(row.name)
    .bind(row.name_latin)
    .bind(row.kind.as_str())
    .bind(i32::from(row.capacity))
    .bind(row.branch)
    .bind(row.employee)
    .bind(row.at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .bind(row.rate.map(Money::minor))
    .bind(row.rate.map(|r| r.currency().to_string()))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The two facts a reservation is *told* by another module, each an opaque id
/// and the moment it arrived.
#[derive(Debug, Clone, Copy)]
enum Stamp {
    /// A deposit settled: `secured_by`, `secured_at`.
    Secured,
    /// The work was billed: `billed_by`, `billed_at`.
    Billed,
}

/// Writes one of them. Two statements rather than one interpolation, because
/// the column is a literal from this file and never a caller's string.
async fn stamped(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    stamp: Stamp,
    by: &str,
    at: Timestamp,
) -> Result<(), sqlx::Error> {
    let sql = match stamp {
        Stamp::Secured => {
            "UPDATE reservation
                SET secured_by = $2, secured_at = $3, recorded_at = $4, position = $5
              WHERE id = $1"
        }
        Stamp::Billed => {
            "UPDATE reservation
                SET billed_by = $2, billed_at = $3, recorded_at = $4, position = $5
              WHERE id = $1"
        }
    };
    sqlx::query(sql)
        .bind(id)
        .bind(by)
        .bind(at)
        .bind(ctx.event_time())
        .bind(ctx.position().get())
        .execute(&mut *conn)
        .await?;
    Ok(())
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
                rate,
                branch,
                employee,
                at,
            } => {
                declared(
                    ctx,
                    conn,
                    id,
                    Declared {
                        name: &name,
                        name_latin: name_latin.as_deref(),
                        kind,
                        capacity,
                        rate,
                        branch: branch.as_ref().map(erp_types::AggregateId::as_str),
                        employee: employee.as_ref().map(erp_types::AggregateId::as_str),
                        at,
                    },
                )
                .await?;
            }
            ResourceEvent::Amended {
                name,
                name_latin,
                capacity,
                rate,
                ..
            } => {
                sqlx::query(
                    "UPDATE resource
                        SET name = $2, name_latin = $3, capacity = $4,
                            recorded_at = $5, position = $6,
                            rate_minor = $7, rate_currency = $8
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&name)
                .bind(&name_latin)
                .bind(i32::from(capacity))
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .bind(rate.map(Money::minor))
                .bind(rate.map(|r| r.currency().to_string()))
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
                deposit,
                note,
                at,
            } => {
                let (starts_at, ends_at) = envelope_of(&lines);
                sqlx::query(
                    "INSERT INTO reservation
                         (id, customer_id, customer_name, customer_phone, stage,
                          starts_at, ends_at, deposit_net, deposit_currency,
                          deposit_due_by, note, reserved_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,'reserved',$5,$6,$7,$8,$9,$10,$11,$12,$13)",
                )
                .bind(id)
                .bind(customer.id.as_ref().map(erp_types::AggregateId::as_str))
                .bind(&customer.name)
                .bind(&customer.phone)
                .bind(starts_at)
                .bind(ends_at)
                .bind(deposit.as_ref().map(|d| d.net.minor()))
                .bind(deposit.as_ref().map(|d| d.net.currency().to_string()))
                .bind(deposit.as_ref().map(|d| d.due_by))
                .bind(none_if_blank(&note))
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
                write_lines(conn, id, &lines).await?;
            }
            // **The slot is paid for.** What paid it is opaque here, and kept
            // so a person can follow the money out of the diary.
            ReservationEvent::Secured { payment, at } => {
                stamped(ctx, conn, id, Stamp::Secured, payment.as_str(), at).await?;
            }
            ReservationEvent::Billed { invoice, at } => {
                stamped(ctx, conn, id, Stamp::Billed, invoice.as_str(), at).await?;
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

/// Which resources each customer must not be booked with.
///
/// **Nothing reads this to decide a booking.** The decision is made against the
/// log in `commands::check_not_barred`, because a bar raised a minute ago has
/// to stop the next booking and a projection lags. This is for the screen that
/// lists them.
///
/// Named for the state rather than the events, because `Bars` is the aggregate
/// the decision is actually made against and two things with one name is how
/// somebody comes to query the wrong one.
#[derive(Debug)]
pub struct Barred;

#[async_trait::async_trait]
impl Projection for Barred {
    type Group = Booking;

    fn name(&self) -> &'static str {
        "bars"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !BarEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let customer = envelope.stream.id.as_str();

        match decode::<BarEvent>(ctx, envelope)? {
            BarEvent::Raised { resource, why, at } => {
                sqlx::query(
                    "INSERT INTO bar (customer_id, resource_id, why, raised_at)
                     VALUES ($1,$2,$3,$4)
                     ON CONFLICT (customer_id, resource_id) DO NOTHING",
                )
                .bind(customer)
                .bind(resource.as_str())
                .bind(&why)
                .bind(at)
                .execute(&mut *conn)
                .await?;
            }
            BarEvent::Lifted { resource, .. } => {
                sqlx::query("DELETE FROM bar WHERE customer_id = $1 AND resource_id = $2")
                    .bind(customer)
                    .bind(resource.as_str())
                    .execute(&mut *conn)
                    .await?;
            }
        }
        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Booking>>> {
    vec![
        std::sync::Arc::new(Resources),
        std::sync::Arc::new(Reservations),
        std::sync::Arc::new(Barred),
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
    /// The published price, before tax, when there is one.
    pub rate: Option<Money>,
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
                  branch, employee, rate_minor, rate_currency,
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
                rate: rate_of(r.rate_minor, r.rate_currency.as_deref()),
                withdrawn: r.withdrawn,
                withdrawn_why: r.withdrawn_why,
            })
            .collect(),
        limit,
        |r| Cursor::over(&[&r.kind, &r.name, &r.id]),
    ))
}

/// One resource, with the timetable it is offered on.
/// The two rate columns as one price, or none.
fn rate_of(minor: Option<i64>, currency: Option<&str>) -> Option<Money> {
    Some(Money::from_minor(
        minor?,
        CurrencyCode::new(currency?).ok()?,
    ))
}

pub async fn resource(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<ResourceDetail>, sqlx::Error> {
    let Some(row) = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, kind as "kind!",
                  capacity as "capacity!", availability as "availability!",
                  branch, employee, rate_minor, rate_currency,
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
            rate: rate_of(row.rate_minor, row.rate_currency.as_deref()),
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
    /// The payment that secured the slot, when a deposit was paid.
    pub secured_by: Option<String>,
    /// The invoice raised for the work, once one has been.
    pub billed_by: Option<String>,
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

/// What one person performed, and what it came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Performed {
    /// The `hr` employee the resource named. Opaque here.
    pub employee: String,
    /// The bookable resource they were assigned as.
    pub resource: String,
    pub jobs: i64,
    /// What those lines came to, net.
    pub net: Money,
}

/// Who performed what over a window, for a commission report.
///
/// # Only completed work, and only what was priced
///
/// **Completed**, because a booking that was cancelled or never turned up is
/// not work somebody did — and a commission paid on a no-show is money the
/// business gives away twice.
///
/// **Priced**, because a line with no charge is a business that bills elsewhere
/// rather than one that charged zero, and summing it as nothing would quietly
/// under-pay whoever performed it. Those lines are excluded and the count says
/// so.
///
/// # Why this lives here and the rate does not
///
/// `booking` knows who did the work and what it was worth. What fraction of it
/// somebody earns is a term of their employment, so it is on their salary in
/// `hr` — and neither module has to learn the other's business.
pub async fn performed(
    conn: &mut PgConnection,
    from: Timestamp,
    until: Timestamp,
) -> Result<Vec<Performed>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT r.employee as "employee!", l.unit as "resource!",
                  count(*) as "jobs!", sum(l.net)::BIGINT as "net!",
                  l.currency as "currency!"
             FROM proj_booking.reservation_line l
             JOIN proj_booking.reservation v ON v.id = l.reservation_id
             JOIN proj_booking.resource r ON r.id = l.unit
            WHERE l.unit IS NOT NULL
              AND r.employee IS NOT NULL
              AND l.net IS NOT NULL
              AND v.stage = 'completed'
              AND l.starts_at < $2 AND l.ends_at > $1
            GROUP BY r.employee, l.unit, l.currency
            ORDER BY r.employee, l.unit"#,
        from,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let currency =
            CurrencyCode::new(&r.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        out.push(Performed {
            employee: r.employee,
            resource: r.resource,
            jobs: r.jobs,
            net: Money::from_minor(r.net, currency),
        });
    }
    Ok(out)
}

/// The diary: bookings that overlap a window, earliest first.
///
/// The window is half-open and matches the way a claim overlaps — `ends_at >
/// from AND starts_at < until` — so a booking that straddles midnight shows up
/// on both days rather than on whichever one it happens to start in.
///
/// Both ends are optional, so the same read serves "everything from now on"
/// and "this week".
/// Bookings **made** since an instant, oldest first.
///
/// # Why when it was made and not when it starts
///
/// This is what an announcer sweeps, and a window on `starts_at` would announce
/// every booking in the diary on the first tick after a tenant switches
/// notifications on. A window on when it was *made* announces what is new,
/// which is what somebody at a counter wants to be told about.
///
/// `reserved_on` is the event's own instant, so this is stable under a rebuild
/// — unlike `recorded_at`, which moves.
pub async fn reserved_since(
    conn: &mut PgConnection,
    since: Timestamp,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!"
             FROM proj_booking.reservation
            WHERE reserved_on > $1
            ORDER BY reserved_on, id
            LIMIT $2"#,
        since,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

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
                  starts_at as "starts_at!", ends_at as "ends_at!", note,
                  secured_by, billed_by
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
                secured_by: r.secured_by,
                billed_by: r.billed_by,
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
                  starts_at as "starts_at!", ends_at as "ends_at!", note,
                  secured_by, billed_by
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
            secured_by: row.secured_by,
            billed_by: row.billed_by,
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

/// A held slot nobody has paid for, past the time it was to be paid by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lapsed {
    pub id: AggregateId,
    /// What was asked for, before tax. For a message, not for a decision.
    pub deposit: Money,
    pub due_by: Timestamp,
}

/// **Held slots nobody paid for.** The worklist the hold-expiry job works.
///
/// Oldest deadline first, so the slot that has been blocked longest is the one
/// released first — and so the batch is a queue rather than a lottery.
///
/// **Answerable inside this group alone.** Whether the money arrived is a fact
/// this module was *told* — `secured_by` — rather than one it reads out of
/// `proj_payments`, which is another projection group on its own checkpoint
/// (L3). That is what a `Secured` event is for.
pub async fn lapsed_holds(
    conn: &mut sqlx::PgConnection,
    now: Timestamp,
    limit: i64,
) -> Result<Vec<Lapsed>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", deposit_net as "deposit_net!",
                  deposit_currency as "deposit_currency!", deposit_due_by as "deposit_due_by!"
             FROM proj_booking.reservation
            WHERE stage = 'reserved'
              AND deposit_due_by IS NOT NULL
              AND secured_by IS NULL
              AND deposit_due_by < $1
            ORDER BY deposit_due_by ASC LIMIT $2"#,
        now,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Lapsed {
                id: AggregateId::new(&row.id).ok()?,
                deposit: Money::from_minor(
                    row.deposit_net,
                    CurrencyCode::new(&row.deposit_currency).ok()?,
                ),
                due_by: row.deposit_due_by,
            })
        })
        .collect())
}

/// **Which of these bookings have not been told their deposit arrived.**
///
/// The other half of `payments::settled_advances`: the worker asks `payments`
/// what settled and asks this which of those the diary has not heard about,
/// and tells it. Reads the projection, because the question is asked for a
/// hundred bookings at a time and `secure_in` re-checks the log before it
/// writes anyway — a stale answer here costs one idempotent no-op.
/// **Completed bookings nobody has billed**, oldest first — the worklist of
/// the pass that raises invoices when the business asks for that on
/// completion. Only bookings with a priced line: one with none is a business
/// that bills elsewhere, and there is nothing to raise a document from.
pub async fn unbilled_completions(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<AggregateId>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"SELECT r.id as "id!" FROM proj_booking.reservation r
            WHERE r.stage = 'completed' AND r.billed_by IS NULL
              AND EXISTS (SELECT 1 FROM proj_booking.reservation_line l
                           WHERE l.reservation_id = r.id AND l.net IS NOT NULL)
            ORDER BY r.ends_at ASC LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|id| AggregateId::new(&id).ok())
        .collect())
}

pub async fn unsecured_among(
    conn: &mut sqlx::PgConnection,
    reservations: &[String],
) -> Result<Vec<AggregateId>, sqlx::Error> {
    let rows = sqlx::query_scalar!(
        r#"SELECT id as "id!" FROM proj_booking.reservation
            WHERE id = ANY($1) AND secured_by IS NULL"#,
        reservations,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|id| AggregateId::new(&id).ok())
        .collect())
}

/// What a booking is waiting to be paid, if anything.
///
/// **`None` once it is secured**, because the question this answers is "does
/// somebody still owe for this slot" and the answer is then no.
pub async fn awaiting_deposit(
    conn: &mut sqlx::PgConnection,
    reservation: &str,
) -> Result<Option<Lapsed>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", deposit_net, deposit_currency, deposit_due_by
             FROM proj_booking.reservation
            WHERE id = $1 AND secured_by IS NULL AND stage = 'reserved'"#,
        reservation,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|row| {
        Some(Lapsed {
            id: AggregateId::new(&row.id).ok()?,
            deposit: Money::from_minor(
                row.deposit_net?,
                CurrencyCode::new(&row.deposit_currency?).ok()?,
            ),
            due_by: row.deposit_due_by?,
        })
    }))
}

/// One bar, as a screen lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    pub resource: String,
    /// What the resource is called, when it is still declared. `None` for one
    /// that has since been removed from the log's reach — the bar is still
    /// real, and hiding it would be the wrong answer.
    pub name: Option<String>,
    /// **For staff.** Never put in front of the customer it is about, and never
    /// in the refusal a booking gets — see `messages::BARRED`.
    pub why: String,
    pub raised_at: Timestamp,
}

/// Every resource one customer must not be booked with.
///
/// Not paged: this is a handful of rows for one person, and a bar list long
/// enough to need paging is a conversation the business needs to have rather
/// than a screen.
pub async fn bars(conn: &mut sqlx::PgConnection, customer: &str) -> Result<Vec<Bar>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT b.resource_id as "resource!", r.name as "name?", b.why as "why!",
                  b.raised_at as "raised_at!"
             FROM proj_booking.bar b
             LEFT JOIN proj_booking.resource r ON r.id = b.resource_id
            WHERE b.customer_id = $1
            ORDER BY b.raised_at DESC, b.resource_id"#,
        customer,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Bar {
            resource: row.resource,
            name: row.name,
            why: row.why,
            raised_at: row.raised_at,
        })
        .collect())
}
