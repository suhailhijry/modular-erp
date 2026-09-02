//! Every figure this module offers, built from the log.
//!
//! # The architectural point, which is the whole reason this module exists
//!
//! A dashboard mixing sales, bookings, takings and payroll looks like it must
//! read four projection groups. **L3 forbids that**: a group is the unit of
//! consistency, and four groups on four checkpoints can disagree while somebody
//! is reading a total across them. The system this phase was read against made
//! exactly that mistake — its projectors declare which other projections they
//! read, and it needed a bespoke check to police the rebuild order that
//! created.
//!
//! So this subscribes to the **log**. It decodes `sales::InvoiceEvent`,
//! `booking::ReservationEvent`, `pos::ShiftEvent` and `payroll::RunEvent`, and
//! maintains one group on one checkpoint. Every figure on a screen built from
//! these tables was true at one position in the log, together.
//!
//! # Periods are the month the *event* says, not the month it was recorded
//!
//! An invoice dated the 31st and entered on the 2nd belongs to the month it was
//! dated to, which is the same argument the ledger makes about `occurred_on`.
//! Every `period` here comes from the domain event's own date.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

/// One group, one checkpoint. See the module docs for why that is the point.
#[derive(Debug)]
pub struct Reports;

impl ProjectionGroup for Reports {
    const NAME: &'static str = "reports";
    const SCHEMA: &'static str = "proj_reports";
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

/// `YYYY-MM`, from an instant.
///
/// The month a report is read by, and it sorts correctly as text — which is why
/// it is a string rather than two integers a query would have to reassemble.
fn period(at: Timestamp) -> String {
    at.format("%Y-%m").to_string()
}

/// The branch an event was written under, or the empty string.
///
/// **Empty rather than null**, so it can be part of a primary key. A
/// single-branch business gets one row per period and it is the empty one,
/// which is honest about there being no branch rather than inventing a name.
fn branch_of(envelope: &Envelope) -> String {
    envelope.metadata.branch().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------------------
// Sales
// ---------------------------------------------------------------------------

/// What was sold, by period and place.
#[derive(Debug)]
pub struct Revenue;

#[async_trait::async_trait]
impl Projection for Revenue {
    type Group = Reports;

    fn name(&self) -> &'static str {
        "revenue"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !sales::InvoiceEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        match decode::<sales::InvoiceEvent>(ctx, envelope)? {
            sales::InvoiceEvent::Issued {
                issued_on, totals, ..
            } => {
                let at = (period(issued_on), branch_of(envelope));
                remember(
                    conn,
                    envelope.stream.id.as_str(),
                    &totals,
                    &at,
                    ctx.position().get(),
                )
                .await?;
                add_revenue(ctx, conn, (&at.0, &at.1), (totals.net, totals.tax), (1, 0)).await
            }
            // **A credit note takes its own numbers back out**, so what this
            // says is what the business kept. The alternative — a gross figure
            // with cancellations shown separately — is the one every report
            // reader adds up wrong.
            //
            // It lands in the period the *credit* was dated to, not the
            // invoice's. A December invoice credited in January is December
            // revenue that January took back, and moving it would restate a
            // month somebody has already filed a return against.
            sales::InvoiceEvent::Cancelled {
                on, credit_note, ..
            } => {
                let Some((net, tax, branch)) = invoiced(conn, envelope.stream.id.as_str()).await?
                else {
                    // An invoice this module never saw issued: one from before
                    // it was enabled. Nothing to take back out, and guessing
                    // would be worse than a gap somebody can explain.
                    return Ok(());
                };
                let back = |amount: Money| {
                    amount.checked_neg().map_err(|e| {
                        ProjectionError::Rejected(format!(
                            "crediting {}: {e}",
                            envelope.stream.id.as_str()
                        ))
                    })
                };
                credited(conn, envelope.stream.id.as_str(), &credit_note).await?;
                // **The invoice's own branch**, not the crediting request's: a
                // credit raised from head office still takes revenue out of the
                // branch that earned it.
                add_revenue(
                    ctx,
                    conn,
                    (&period(on), &branch),
                    (back(net)?, back(tax)?),
                    (0, 1),
                )
                .await
            }
            // A payment, a refund and an attached customer move no revenue: the
            // sale was recognised when the document was issued, which is what
            // accrual accounting means.
            _ => Ok(()),
        }
    }
}

/// Remembers what an invoice came to, so a credit can take out exactly that.
async fn remember(
    conn: &mut PgConnection,
    invoice: &str,
    totals: &sales::Totals,
    at: &(String, String),
    position: i64,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO invoiced (id, net, tax, currency, period, branch, entry, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT (id) DO UPDATE
             SET net = EXCLUDED.net, tax = EXCLUDED.tax,
                 currency = EXCLUDED.currency, period = EXCLUDED.period,
                 branch = EXCLUDED.branch, entry = EXCLUDED.entry,
                 position = EXCLUDED.position",
    )
    .bind(invoice)
    .bind(totals.net.minor())
    .bind(totals.tax.minor())
    .bind(totals.net.currency().to_string())
    .bind(&at.0)
    .bind(&at.1)
    // **`sales` names its own entries.** Reimplementing `si.{invoice}` here is
    // the kind of copy that stays right until somebody changes the prefix.
    .bind(sales::issue_entry_of(invoice))
    .bind(position)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What an invoice came to, and where — from this module's own row.
///
/// Reading `proj_sales` instead would be the cross-group read L3 forbids.
async fn invoiced(
    conn: &mut PgConnection,
    invoice: &str,
) -> Result<Option<(Money, Money, String)>, ProjectionError> {
    // projection-read: `invoiced`, written by this projection on `Issued`.
    // `sales.invoice.cancelled` carries the credit note and not the invoice's
    // amounts, so netting a credit off has to remember what the issue put in.
    let row = sqlx::query_as::<_, (i64, i64, String, String)>(
        "SELECT net, tax, currency, branch FROM invoiced WHERE id = $1",
    )
    .bind(invoice)
    .fetch_optional(&mut *conn)
    .await?;

    let Some((net, tax, currency, branch)) = row else {
        return Ok(None);
    };
    let currency = CurrencyCode::new(&currency)
        .map_err(|e| ProjectionError::Rejected(format!("{invoice}: {e}")))?;
    Ok(Some((
        Money::from_minor(net, currency),
        Money::from_minor(tax, currency),
        branch,
    )))
}

/// Adds to a period's revenue, creating the row if it is the first.
async fn add_revenue(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    where_: (&str, &str),
    amounts: (Money, Money),
    counts: (i32, i32),
) -> Result<(), ProjectionError> {
    let (period, branch) = where_;
    let (net, tax) = amounts;
    let (documents, credited) = counts;

    sqlx::query(
        "INSERT INTO revenue
             (period, branch, currency, net, tax, documents, credited,
              recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (period, branch, currency) DO UPDATE
             SET net = revenue.net + EXCLUDED.net,
                 tax = revenue.tax + EXCLUDED.tax,
                 documents = revenue.documents + EXCLUDED.documents,
                 credited = revenue.credited + EXCLUDED.credited,
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(period)
    .bind(branch)
    .bind(net.currency().to_string())
    .bind(net.minor())
    .bind(tax.minor())
    .bind(documents)
    .bind(credited)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every projection this module contributes.
///
/// Five, over four other modules' events plus the ledger's. None of them reads
/// another projection group; that is the point of the module.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Reports>>> {
    vec![
        std::sync::Arc::new(Revenue),
        std::sync::Arc::new(Diary),
        std::sync::Arc::new(Counter),
        std::sync::Arc::new(Wages),
        std::sync::Arc::new(Book),
    ]
}

/// The two minute counts a utilisation row keeps, so [`bump`] does not take two
/// bare `i64`s a caller can transpose.
#[derive(Debug, Clone, Copy)]
struct Minutes {
    /// Diary time the work took. Only ever non-zero on a completion.
    took: i64,
    /// Notice the booking gave. Only ever non-zero when it was taken.
    lead: i64,
}

// ---------------------------------------------------------------------------
// Booking
// ---------------------------------------------------------------------------

/// How well the diary was used.
#[derive(Debug)]
pub struct Diary;

#[async_trait::async_trait]
impl Projection for Diary {
    type Group = Reports;

    fn name(&self) -> &'static str {
        "utilisation"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !booking::ReservationEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<booking::ReservationEvent>(ctx, envelope)? {
            booking::ReservationEvent::Reserved { lines, at, .. } => {
                hold(ctx, conn, id, &lines, Taken::Now(at)).await
            }
            // **Rescheduling replaces what is held.** A booking moved from
            // March to April is April's, and leaving the March row would count
            // it in both months — which is how a utilisation figure comes to
            // add up to more than the diary has hours in it.
            booking::ReservationEvent::Rescheduled { lines, .. } => {
                release(conn, id).await?;
                hold(ctx, conn, id, &lines, Taken::Already).await
            }
            booking::ReservationEvent::Moved { to, .. } => moved_to(ctx, conn, id, to).await,
            // Assigning a unit changes who does the work, not whether it
            // happened. `booking::performed` is where per-person revenue is
            // answered, and it is a different question from utilisation.
            booking::ReservationEvent::Assigned { .. } => Ok(()),
        }
    }
}

/// Whether this is the booking being taken, or one being moved.
///
/// The instant travels with it because **lead time is domain time to domain
/// time**: `ctx.event_time()` is when the append committed, which for anything
/// entered after the fact — an import, a backfill, a booking written up at the
/// end of the day — would say a month's notice was none.
#[derive(Debug, Clone, Copy)]
enum Taken {
    /// Newly booked, at this instant. Counts, and gives its notice.
    Now(Timestamp),
    /// Moved. The counts already happened and must not happen again.
    Already,
}

/// Records what a booking holds, and counts it as booked if it is new.
async fn hold(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    reservation: &str,
    lines: &[booking::Line],
    taken: Taken,
) -> Result<(), ProjectionError> {
    for line in lines {
        let when = period(line.span.from());
        let minutes = (line.span.until() - line.span.from()).num_minutes();

        for held in &line.takes {
            sqlx::query(
                "INSERT INTO held (reservation, resource, period, minutes, stage)
                 VALUES ($1,$2,$3,$4,'reserved')
                 ON CONFLICT (reservation, resource) DO UPDATE
                     SET period = EXCLUDED.period, minutes = EXCLUDED.minutes",
            )
            .bind(reservation)
            .bind(held.resource.as_str())
            .bind(&when)
            .bind(minutes)
            .execute(&mut *conn)
            .await?;

            if let Taken::Now(at) = taken {
                // **How much notice this booking gave**, from the moment it was
                // taken to the moment the work starts. Negative would mean a
                // booking written down after it began — which happens, at a
                // till, and is floored at zero rather than allowed to pull an
                // average backwards.
                let lead = (line.span.from() - at).num_minutes().max(0);
                bump(
                    ctx,
                    conn,
                    (&when, held.resource.as_str()),
                    "booked",
                    1,
                    Minutes { took: 0, lead },
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Forgets what a booking held, without touching the counts.
///
/// **The counts stay.** A booking that moved from March to April was still
/// booked in March, and un-counting it would rewrite a month somebody has
/// already read.
async fn release(conn: &mut PgConnection, reservation: &str) -> Result<(), ProjectionError> {
    sqlx::query("DELETE FROM held WHERE reservation = $1")
        .bind(reservation)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Counts a stage change against every resource the booking holds.
///
/// **Only the first time it reaches a terminal stage.** `reserved → confirmed →
/// completed` is one completion, and a booking marked completed twice by two
/// clicks is still one.
async fn moved_to(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    reservation: &str,
    to: booking::Stage,
) -> Result<(), ProjectionError> {
    let column = match to {
        booking::Stage::Completed => "completed",
        booking::Stage::NoShow => "no_shows",
        booking::Stage::Cancelled => "cancelled",
        // Confirmed, arrived and in-service are on the way to somewhere, and a
        // report that counted them would count the same booking three times.
        _ => return Ok(()),
    };

    // projection-read: `held`, written by this projection on `Reserved`.
    // `booking.reservation.moved` carries a stage and nothing else — the whole
    // lifecycle is one event — so attributing a completion to the resources it
    // took has to remember what they were.
    let rows = sqlx::query_as::<_, (String, String, i64, String)>(
        "SELECT resource, period, minutes, stage FROM held WHERE reservation = $1",
    )
    .bind(reservation)
    .fetch_all(&mut *conn)
    .await?;

    for (resource, when, minutes, stage) in rows {
        if stage != "reserved" {
            // Already counted somewhere terminal. Moving from `no_show` to
            // `completed` is a correction somebody has to make by hand, and
            // silently moving the count would hide that it happened.
            continue;
        }
        // Only completed work took diary time worth reporting: a no-show gave
        // the capacity back, and counting its minutes would make a business
        // that is empty look busy.
        let took = if to == booking::Stage::Completed {
            minutes
        } else {
            0
        };
        bump(
            ctx,
            conn,
            (&when, &resource),
            column,
            1,
            Minutes { took, lead: 0 },
        )
        .await?;

        sqlx::query("UPDATE held SET stage = $3 WHERE reservation = $1 AND resource = $2")
            .bind(reservation)
            .bind(&resource)
            .bind(column)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Adds one to a utilisation counter.
///
/// The column is a literal from this file and never a caller's string, which is
/// what would otherwise make this the dynamic SQL the workspace forbids — so it
/// is four statements rather than one interpolation.
async fn bump(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    at: (&str, &str),
    column: &str,
    by: i32,
    minutes: Minutes,
) -> Result<(), ProjectionError> {
    let (when, resource) = at;
    let (booked, completed, no_shows, cancelled) = match column {
        "booked" => (by, 0, 0, 0),
        "completed" => (0, by, 0, 0),
        "no_shows" => (0, 0, by, 0),
        _ => (0, 0, 0, by),
    };

    sqlx::query(
        "INSERT INTO utilisation
             (period, resource, booked, completed, no_shows, cancelled, minutes,
              lead_minutes, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         ON CONFLICT (period, resource) DO UPDATE
             SET booked = utilisation.booked + EXCLUDED.booked,
                 completed = utilisation.completed + EXCLUDED.completed,
                 no_shows = utilisation.no_shows + EXCLUDED.no_shows,
                 cancelled = utilisation.cancelled + EXCLUDED.cancelled,
                 minutes = utilisation.minutes + EXCLUDED.minutes,
                 lead_minutes = utilisation.lead_minutes + EXCLUDED.lead_minutes,
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(when)
    .bind(resource)
    .bind(booked)
    .bind(completed)
    .bind(no_shows)
    .bind(cancelled)
    .bind(minutes.took)
    .bind(minutes.lead)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Records which entry credited a document, so the reconciliation can check it.
async fn credited(
    conn: &mut PgConnection,
    invoice: &str,
    credit_note: &str,
) -> Result<(), ProjectionError> {
    sqlx::query("UPDATE invoiced SET credit_entry = $2 WHERE id = $1")
        .bind(invoice)
        .bind(sales::credit_entry_of(invoice, credit_note))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Point of sale
// ---------------------------------------------------------------------------

/// What the tills took, by method and by person.
#[derive(Debug)]
pub struct Counter;

#[async_trait::async_trait]
impl Projection for Counter {
    type Group = Reports;

    fn name(&self) -> &'static str {
        "takings"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !pos::ShiftEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let shift = envelope.stream.id.as_str();

        match decode::<pos::ShiftEvent>(ctx, envelope)? {
            // The one event that says who is on the counter. Everything after
            // it carries tenders and no operator, rightly — the operator has
            // not changed — so this is where the attribution comes from.
            pos::ShiftEvent::Opened { operator, .. } => {
                sqlx::query(
                    "INSERT INTO till (shift, operator) VALUES ($1,$2)
                     ON CONFLICT (shift) DO UPDATE SET operator = EXCLUDED.operator",
                )
                .bind(shift)
                .bind(&operator)
                .execute(&mut *conn)
                .await?;
                Ok(())
            }
            pos::ShiftEvent::Sold { tenders, at, .. } => {
                let Some(operator) = on_the_counter(conn, shift).await? else {
                    return Ok(());
                };
                for tender in tenders {
                    add_takings(
                        ctx,
                        conn,
                        (&period(at), &operator, tender.method),
                        Took::taken(tender.amount),
                    )
                    .await?;
                }
                Ok(())
            }
            pos::ShiftEvent::Refunded { tenders, at, .. } => {
                let Some(operator) = on_the_counter(conn, shift).await? else {
                    return Ok(());
                };
                for tender in tenders {
                    add_takings(
                        ctx,
                        conn,
                        (&period(at), &operator, tender.method),
                        Took::refunded(tender.amount),
                    )
                    .await?;
                }
                Ok(())
            }
            // **Cash only**, because only cash is in the box. A card sale
            // settles to the bank and never leaves a drawer.
            pos::ShiftEvent::PaidOut { amount, at, .. } => {
                let Some(operator) = on_the_counter(conn, shift).await? else {
                    return Ok(());
                };
                add_takings(
                    ctx,
                    conn,
                    (&period(at), &operator, pos::Method::Cash),
                    Took::paid_out(amount),
                )
                .await
            }
            // **The number a manager reads.** Counted at close, so `shifts` is
            // how many drawers were reconciled rather than how many were
            // opened — a till still taking money has no variance yet.
            pos::ShiftEvent::Closed { variance, at, .. } => {
                let Some(operator) = on_the_counter(conn, shift).await? else {
                    return Ok(());
                };
                add_takings(
                    ctx,
                    conn,
                    (&period(at), &operator, pos::Method::Cash),
                    Took::closed(variance),
                )
                .await
            }
        }
    }
}

/// Who has this till open, from this module's own row.
async fn on_the_counter(
    conn: &mut PgConnection,
    shift: &str,
) -> Result<Option<String>, ProjectionError> {
    // `None` on a shift opened before this module was enabled. Attributing its
    // takings to nobody would be inventing a person; leaving them out is a gap
    // somebody can explain.
    Ok(
        // projection-read: `till`, written by this projection on `Opened`.
        // `pos.shift.sold` carries tenders and not the operator, because the
        // operator has not changed since the shift was opened.
        sqlx::query_scalar::<_, String>("SELECT operator FROM till WHERE shift = $1")
            .bind(shift)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// One movement on a takings row. Named rather than four positional amounts,
/// because `taken` and `refunded` are the same type and transposing them is
/// silent.
#[derive(Debug, Clone, Copy)]
struct Took {
    amount: Money,
    taken: i64,
    refunded: i64,
    paid_out: i64,
    variance: i64,
    shifts: i32,
}

impl Took {
    const fn nothing(amount: Money) -> Self {
        Self {
            amount,
            taken: 0,
            refunded: 0,
            paid_out: 0,
            variance: 0,
            shifts: 0,
        }
    }

    fn taken(amount: Money) -> Self {
        Self {
            taken: amount.minor(),
            ..Self::nothing(amount)
        }
    }

    fn refunded(amount: Money) -> Self {
        Self {
            refunded: amount.minor(),
            ..Self::nothing(amount)
        }
    }

    fn paid_out(amount: Money) -> Self {
        Self {
            paid_out: amount.minor(),
            ..Self::nothing(amount)
        }
    }

    fn closed(variance: Money) -> Self {
        Self {
            variance: variance.minor(),
            shifts: 1,
            ..Self::nothing(variance)
        }
    }
}

async fn add_takings(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    at: (&str, &str, pos::Method),
    took: Took,
) -> Result<(), ProjectionError> {
    let (when, operator, method) = at;

    sqlx::query(
        "INSERT INTO takings
             (period, operator, method, currency, taken, refunded, variance,
              paid_out, shifts, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
         ON CONFLICT (period, operator, method, currency) DO UPDATE
             SET taken = takings.taken + EXCLUDED.taken,
                 refunded = takings.refunded + EXCLUDED.refunded,
                 variance = takings.variance + EXCLUDED.variance,
                 paid_out = takings.paid_out + EXCLUDED.paid_out,
                 shifts = takings.shifts + EXCLUDED.shifts,
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(when)
    .bind(operator)
    .bind(method.as_str())
    .bind(took.amount.currency().to_string())
    .bind(took.taken)
    .bind(took.refunded)
    .bind(took.variance)
    .bind(took.paid_out)
    .bind(took.shifts)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Payroll
// ---------------------------------------------------------------------------

/// What people cost, from approved runs.
#[derive(Debug)]
pub struct Wages;

#[async_trait::async_trait]
impl Projection for Wages {
    type Group = Reports;

    fn name(&self) -> &'static str {
        "people_cost"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !payroll::RunEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let run = envelope.stream.id.as_str();

        match decode::<payroll::RunEvent>(ctx, envelope)? {
            // Held, not counted. **A draft is not a cost** — counting one would
            // make a report move when somebody opened a screen, and drafting
            // again replaces rather than accumulates.
            payroll::RunEvent::Drafted {
                period,
                payslips,
                gross,
                deductions,
                net,
                ..
            } => {
                let commission = Money::checked_sum(
                    payslips.iter().map(|slip| slip.commission),
                    gross.currency(),
                )
                .map_err(|e| ProjectionError::Rejected(format!("{run}: {e}")))?;

                sqlx::query(
                    "INSERT INTO drafted
                         (run, period, currency, gross, commission, deductions, net, people)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
                     ON CONFLICT (run) DO UPDATE
                         SET period = EXCLUDED.period, currency = EXCLUDED.currency,
                             gross = EXCLUDED.gross, commission = EXCLUDED.commission,
                             deductions = EXCLUDED.deductions, net = EXCLUDED.net,
                             people = EXCLUDED.people",
                )
                .bind(run)
                .bind(period.to_string())
                .bind(gross.currency().to_string())
                .bind(gross.minor())
                .bind(commission.minor())
                .bind(deductions.minor())
                .bind(net.minor())
                .bind(i32::try_from(payslips.len()).unwrap_or(i32::MAX))
                .execute(&mut *conn)
                .await?;
                Ok(())
            }
            // **Now it is a cost.** The figures come from this module's own
            // record of the draft: `payroll.run.approved` carries the journal
            // entry and the time, because approving does not change what
            // anybody is paid.
            payroll::RunEvent::Approved { .. } => {
                // projection-read: `drafted`, written by this projection on
                // `Drafted`. `payroll.run.approved` carries the journal entry
                // and the time, because approving does not change what anybody
                // is paid — so the figures come from where they were held.
                let row = sqlx::query_as::<_, (String, String, i64, i64, i64, i64, i32)>(
                    "SELECT period, currency, gross, commission, deductions, net, people
                       FROM drafted WHERE run = $1",
                )
                .bind(run)
                .fetch_optional(&mut *conn)
                .await?;

                // A run approved whose draft this module never saw: one from
                // before it was enabled. Nothing to count.
                let Some((when, currency, gross, commission, deductions, net, people)) = row else {
                    return Ok(());
                };

                sqlx::query(
                    "INSERT INTO people_cost
                         (period, currency, gross, commission, deductions, net, people,
                          recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                     ON CONFLICT (period, currency) DO UPDATE
                         SET gross = people_cost.gross + EXCLUDED.gross,
                             commission = people_cost.commission + EXCLUDED.commission,
                             deductions = people_cost.deductions + EXCLUDED.deductions,
                             net = people_cost.net + EXCLUDED.net,
                             people = people_cost.people + EXCLUDED.people,
                             recorded_at = EXCLUDED.recorded_at,
                             position = EXCLUDED.position",
                )
                .bind(&when)
                .bind(&currency)
                .bind(gross)
                .bind(commission)
                .bind(deductions)
                .bind(net)
                .bind(people)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The books
// ---------------------------------------------------------------------------

/// What the ledger recorded, kept here so §10b is not a cross-group read.
///
/// See `schema/install.sql` on `entry` for why this module keeps its own copy
/// rather than asking `proj_ledger`: two groups sit on two checkpoints, and an
/// invariant that fires because one of them is behind is an invariant somebody
/// switches off.
#[derive(Debug)]
pub struct Book;

#[async_trait::async_trait]
impl Projection for Book {
    type Group = Reports;

    fn name(&self) -> &'static str {
        "entry"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !ledger::JournalEntryEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        // A reversal is its own posted entry, so there is nothing to record
        // here: `Reversed` says an entry was undone, and the entry that undid
        // it arrives as a `Posted` of its own.
        let ledger::JournalEntryEvent::Posted {
            occurred_on, lines, ..
        } = decode::<ledger::JournalEntryEvent>(ctx, envelope)?
        else {
            return Ok(());
        };

        let Some(currency) = lines.as_slice().first().map(|line| line.amount.currency()) else {
            return Ok(());
        };

        // Summed side by side rather than taken from `total_debits` and
        // negated. `BalancedLines` carries a proof that they cancel, so
        // deriving one from the other would make the reconciliation's
        // per-currency check vacuous — it would be asserting arithmetic this
        // function had just done.
        let (debits, credits) = lines
            .as_slice()
            .iter()
            .fold((0_i64, 0_i64), |(d, c), line| {
                let amount = line.amount.minor();
                if amount > 0 {
                    (d.saturating_add(amount), c)
                } else {
                    (d, c.saturating_add(amount))
                }
            });

        sqlx::query(
            "INSERT INTO entry (id, currency, debits, credits, occurred_on)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (id) DO UPDATE
                 SET currency = EXCLUDED.currency, debits = EXCLUDED.debits,
                     credits = EXCLUDED.credits, occurred_on = EXCLUDED.occurred_on",
        )
        .bind(envelope.stream.id.as_str())
        .bind(currency.to_string())
        .bind(debits)
        .bind(credits)
        .bind(occurred_on)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// What a dashboard reads
// ---------------------------------------------------------------------------
//
// Every one of these takes a period range and returns rows, with no cursor.
// **A report is small by construction**: one row per month per branch per
// currency, so five years of a three-branch business is a hundred and eighty
// rows. Paging a hundred and eighty rows is machinery for its own sake — and a
// chart wants the whole series anyway, so a cursor would only mean the caller
// reassembling it.
//
// `MONTHS` caps what one request can ask for, so a client that sends
// `1900-01`..`2999-12` gets a bounded answer rather than a scan.

/// The longest range one request may ask for. Ten years, which is longer than
/// this system will have been running and longer than any chart shows.
pub const MONTHS: i64 = 120;

/// One period's sales.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevenueRow {
    pub period: String,
    /// Empty on a business with no branches. See `schema/install.sql`.
    pub branch: String,
    /// **Net of credit notes.** What the business kept.
    pub net: Money,
    pub tax: Money,
    pub documents: i32,
    pub credited: i32,
}

/// Sales by period and branch, oldest first.
pub async fn revenue(
    conn: &mut PgConnection,
    from: &str,
    until: &str,
) -> Result<Vec<RevenueRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT period as "period!", branch as "branch!", currency as "currency!",
                  net as "net!", tax as "tax!",
                  documents as "documents!", credited as "credited!"
             FROM proj_reports.revenue
            WHERE period >= $1 AND period <= $2
            ORDER BY period, branch, currency
            LIMIT $3"#,
        from,
        until,
        MONTHS,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = money(&row.currency)?;
            Ok(RevenueRow {
                period: row.period,
                branch: row.branch,
                net: Money::from_minor(row.net, currency),
                tax: Money::from_minor(row.tax, currency),
                documents: row.documents,
                credited: row.credited,
            })
        })
        .collect()
}

/// One period's use of one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilisationRow {
    pub period: String,
    pub resource: String,
    pub booked: i32,
    pub completed: i32,
    pub no_shows: i32,
    pub cancelled: i32,
    /// Diary time the completed work took.
    pub minutes: i64,
    /// Notice the bookings gave, summed. Divided by `booked` it is the average.
    pub lead_minutes: i64,
}

impl UtilisationRow {
    /// No-shows as a share of what was booked, in basis points.
    ///
    /// **Integer arithmetic**, like every rate in this workspace: floating
    /// point is forbidden here, and a percentage that is off in the fourth
    /// decimal is a percentage somebody eventually reconciles against.
    #[must_use]
    pub const fn no_show_rate_bp(&self) -> i32 {
        if self.booked == 0 {
            return 0;
        }
        // Cannot overflow: `no_shows` never exceeds `booked`, and both are
        // counts of events in one month.
        (self.no_shows * 10_000) / self.booked
    }

    /// Average notice, in minutes. Zero when nothing was booked.
    #[must_use]
    pub const fn average_lead_minutes(&self) -> i64 {
        if self.booked == 0 {
            return 0;
        }
        self.lead_minutes / self.booked as i64
    }
}

/// How the diary was used, oldest first.
pub async fn utilisation(
    conn: &mut PgConnection,
    from: &str,
    until: &str,
) -> Result<Vec<UtilisationRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT period as "period!", resource as "resource!",
                  booked as "booked!", completed as "completed!",
                  no_shows as "no_shows!", cancelled as "cancelled!",
                  minutes as "minutes!", lead_minutes as "lead_minutes!"
             FROM proj_reports.utilisation
            WHERE period >= $1 AND period <= $2
            ORDER BY period, resource
            LIMIT $3"#,
        from,
        until,
        MONTHS * 100,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| UtilisationRow {
            period: row.period,
            resource: row.resource,
            booked: row.booked,
            completed: row.completed,
            no_shows: row.no_shows,
            cancelled: row.cancelled,
            minutes: row.minutes,
            lead_minutes: row.lead_minutes,
        })
        .collect())
}

/// What one person's till took, one period, one method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakingsRow {
    pub period: String,
    pub operator: String,
    pub method: String,
    pub taken: Money,
    pub refunded: Money,
    /// **What the drawer disagreed by.** Negative is short.
    pub variance: Money,
    /// Cash out that was not a refund — the closest the log comes to banked.
    pub paid_out: Money,
    pub shifts: i32,
}

/// Till takings, oldest first.
pub async fn takings(
    conn: &mut PgConnection,
    from: &str,
    until: &str,
) -> Result<Vec<TakingsRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT period as "period!", operator as "operator!", method as "method!",
                  currency as "currency!", taken as "taken!", refunded as "refunded!",
                  variance as "variance!", paid_out as "paid_out!", shifts as "shifts!"
             FROM proj_reports.takings
            WHERE period >= $1 AND period <= $2
            ORDER BY period, operator, method
            LIMIT $3"#,
        from,
        until,
        MONTHS * 100,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = money(&row.currency)?;
            Ok(TakingsRow {
                period: row.period,
                operator: row.operator,
                method: row.method,
                taken: Money::from_minor(row.taken, currency),
                refunded: Money::from_minor(row.refunded, currency),
                variance: Money::from_minor(row.variance, currency),
                paid_out: Money::from_minor(row.paid_out, currency),
                shifts: row.shifts,
            })
        })
        .collect()
}

/// One period's wage bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeopleCostRow {
    pub period: String,
    pub gross: Money,
    /// Part of `gross`, not on top of it — see `payroll::Payslip::commission`.
    pub commission: Money,
    pub deductions: Money,
    pub net: Money,
    pub people: i32,
}

/// What people cost, oldest first. **Approved runs only.**
pub async fn people_cost(
    conn: &mut PgConnection,
    from: &str,
    until: &str,
) -> Result<Vec<PeopleCostRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT period as "period!", currency as "currency!",
                  gross as "gross!", commission as "commission!",
                  deductions as "deductions!", net as "net!", people as "people!"
             FROM proj_reports.people_cost
            WHERE period >= $1 AND period <= $2
            ORDER BY period, currency
            LIMIT $3"#,
        from,
        until,
        MONTHS,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency = money(&row.currency)?;
            Ok(PeopleCostRow {
                period: row.period,
                gross: Money::from_minor(row.gross, currency),
                commission: Money::from_minor(row.commission, currency),
                deductions: Money::from_minor(row.deductions, currency),
                net: Money::from_minor(row.net, currency),
                people: row.people,
            })
        })
        .collect()
}

/// A currency code out of a stored row.
///
/// A stored code that is not one means these tables were written by something
/// that is not this code, which is a decode failure and not a zero (L6).
fn money(code: &str) -> Result<CurrencyCode, sqlx::Error> {
    CurrencyCode::new(code).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}
