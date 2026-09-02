//! The org chart, for a screen.
//!
//! **Not what any authorization check reads.** The effective claim set is
//! write-side state in the tenant migration chain, for the reason §9c gives: a
//! command deciding whether somebody may approve something cannot read a table
//! that may be a second behind. This is for drawing the chart.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::employee::EmployeeEvent;

/// One table, one group.
#[derive(Debug)]
pub struct Hr;

impl ProjectionGroup for Hr {
    const NAME: &'static str = "hr";
    const SCHEMA: &'static str = "proj_hr";
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

/// People, as a chart shows them.
#[derive(Debug)]
pub struct EmployeeList;

#[async_trait::async_trait]
impl Projection for EmployeeList {
    type Group = Hr;

    fn name(&self) -> &'static str {
        "employees"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !EmployeeEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<EmployeeEvent>(ctx, envelope)? {
            EmployeeEvent::Hired {
                name,
                name_latin,
                national_id,
                email,
                phone,
                reports_to,
                branch,
                at,
            } => {
                let details = crate::Details {
                    name,
                    name_latin,
                    national_id,
                    email,
                    phone,
                };
                hired(
                    ctx,
                    conn,
                    id,
                    &details,
                    (reports_to.as_ref(), branch.as_ref()),
                    at,
                )
                .await?;
            }
            EmployeeEvent::Amended {
                name,
                name_latin,
                national_id,
                email,
                phone,
                ..
            } => {
                let details = crate::Details {
                    name,
                    name_latin,
                    national_id,
                    email,
                    phone,
                };
                amended(ctx, conn, id, &details).await?;
            }
            EmployeeEvent::Reparented { reports_to, .. } => {
                reparented(ctx, conn, id, reports_to.as_ref()).await?;
            }
            EmployeeEvent::Transferred { branch, .. } => {
                transferred(ctx, conn, id, branch.as_ref()).await?;
            }
            EmployeeEvent::Left { why, at } => left(ctx, conn, id, &why, at).await?,
            EmployeeEvent::Rehired { .. } => rehired(ctx, conn, id).await?,
            EmployeeEvent::Contracted { salary, .. } => {
                contracted(ctx, conn, id, &salary).await?;
            }
            EmployeeEvent::Rostered { shifts, .. } => {
                rostered(ctx, conn, id, &shifts).await?;
            }
            EmployeeEvent::Attended {
                on, minutes, note, ..
            } => attended(ctx, conn, id, on, minutes, &note).await?,
            EmployeeEvent::Absent {
                kind,
                from,
                until,
                why,
                ..
            } => absent(ctx, conn, id, kind, (from, until), &why).await?,
            EmployeeEvent::Skilled { skills, .. } => {
                skilled(ctx, conn, id, &skills).await?;
            }
            EmployeeEvent::DocumentRecorded {
                kind,
                number,
                expires_on,
                ..
            } => {
                recorded(ctx, conn, id, kind, &number, expires_on).await?;
            }
        }
        Ok(())
    }
}

/// Somebody was put on the books.
///
/// Its own function for the reason the two `moved` ones are: `apply` reads
/// better as one line per event than as one function over the limit.
///
/// The details arrive as [`crate::Details`] rather than five string parameters,
/// because five strings in a row is a call somebody transposes — the same
/// argument `sales::Draft` makes about an invoice.
async fn hired(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    details: &crate::Details,
    chart: (
        Option<&erp_types::AggregateId>,
        Option<&erp_types::AggregateId>,
    ),
    at: Timestamp,
) -> Result<(), ProjectionError> {
    let (reports_to, branch) = chart;
    sqlx::query(
        "INSERT INTO employee
             (id, name, name_latin, national_id, email, phone,
              reports_to, branch, hired_on, recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(id)
    .bind(&details.name)
    .bind(details.name_latin.as_deref())
    .bind(details.national_id.as_deref())
    .bind(details.email.as_deref())
    .bind(details.phone.as_deref())
    .bind(reports_to.map(erp_types::AggregateId::as_str))
    .bind(branch.map(erp_types::AggregateId::as_str))
    .bind(at)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What somebody is paid.
///
/// `gross` and `net` are stored, not derived on read: they are what GOSI and
/// end-of-service are computed from, and a report recomputing them from a JSON
/// blob would be a second implementation of the rule.
///
/// A total that will not fit **stops the group** rather than storing a wrong
/// one (L6). It means the log holds a salary this build cannot represent, which
/// is a corruption somebody has to look at — silently clamping it would put a
/// number nobody chose on a payslip.
async fn contracted(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    salary: &crate::employee::Salary,
) -> Result<(), ProjectionError> {
    let fail = |what: &str| {
        ProjectionError::Rejected(format!(
            "{what} for {id} does not fit in Money, at position {}",
            ctx.position()
        ))
    };
    let gross = salary.gross().map_err(|_| fail("gross pay"))?;
    let net = salary.net().map_err(|_| fail("net pay"))?;

    sqlx::query(
        "INSERT INTO employee_salary
             (employee, basic, gross, net, currency, allowances, deductions,
              recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (employee) DO UPDATE
             SET basic = EXCLUDED.basic, gross = EXCLUDED.gross, net = EXCLUDED.net,
                 currency = EXCLUDED.currency, allowances = EXCLUDED.allowances,
                 deductions = EXCLUDED.deductions,
                 recorded_at = EXCLUDED.recorded_at, position = EXCLUDED.position",
    )
    .bind(id)
    .bind(salary.basic.minor())
    .bind(gross.minor())
    .bind(net.minor())
    .bind(salary.basic.currency().to_string())
    .bind(serde_json::to_value(&salary.allowances).unwrap_or_default())
    .bind(serde_json::to_value(&salary.deductions).unwrap_or_default())
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// When somebody works.
///
/// The rules go in as JSON for the reason a resource's opening hours do:
/// nothing queries inside them. A rota screen draws them, and the rule that
/// *decides* is evaluated on the write side from the aggregate.
async fn rostered(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    shifts: &[erp_recurrence::Availability],
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO employee_shift (employee, shifts, recorded_at, position)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (employee) DO UPDATE
             SET shifts = EXCLUDED.shifts,
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(id)
    .bind(serde_json::to_value(shifts).unwrap_or_default())
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Somebody left. **The record stays** — they are on last year's payroll and
/// whatever they approved.
async fn left(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    why: &str,
    at: Timestamp,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "UPDATE employee SET left_at = $2, left_why = $3, recorded_at = $4, position = $5
          WHERE id = $1",
    )
    .bind(id)
    .bind(at)
    .bind(why)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// They came back.
async fn rehired(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "UPDATE employee SET left_at = NULL, left_why = NULL, recorded_at = $2, position = $3
          WHERE id = $1",
    )
    .bind(id)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A day somebody worked.
async fn attended(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    on: chrono::NaiveDate,
    minutes: u16,
    note: &str,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO employee_day (employee, on_date, minutes, note, recorded_at, position)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (employee, on_date) DO UPDATE
             SET minutes = EXCLUDED.minutes, note = EXCLUDED.note,
                 recorded_at = EXCLUDED.recorded_at, position = EXCLUDED.position",
    )
    .bind(id)
    .bind(on)
    .bind(i32::from(minutes))
    .bind(note)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Leave taken, or booked.
///
/// `days` is stored because it is what a balance is drawn down by, and a report
/// recomputing it from two dates would be a second implementation of an
/// inclusive range — which is exactly the arithmetic somebody gets wrong by one.
async fn absent(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    kind: crate::Leave,
    dates: (chrono::NaiveDate, chrono::NaiveDate),
    why: &str,
) -> Result<(), ProjectionError> {
    let (from, until) = dates;
    let days = (until - from).num_days() + 1;

    sqlx::query(
        "INSERT INTO employee_leave
             (employee, kind, from_date, until_date, days, why, recorded_at, position)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (employee, from_date, kind) DO UPDATE
             SET until_date = EXCLUDED.until_date, days = EXCLUDED.days,
                 why = EXCLUDED.why, recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(id)
    .bind(kind.as_str())
    .bind(from)
    .bind(until)
    .bind(i32::try_from(days).unwrap_or(i32::MAX))
    .bind(why)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What somebody is qualified to do, as a set.
///
/// Delete-then-insert, because the event carries the whole set: a skill removed
/// has to disappear, and an upsert alone would leave it there for ever.
async fn skilled(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    skills: &[erp_types::AggregateId],
) -> Result<(), ProjectionError> {
    sqlx::query("DELETE FROM employee_skill WHERE employee = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;

    for service in skills {
        sqlx::query(
            "INSERT INTO employee_skill (employee, service, recorded_at, position)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (employee, service) DO NOTHING",
        )
        .bind(id)
        .bind(service.as_str())
        .bind(ctx.event_time())
        .bind(ctx.position().get())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A document was recorded, or renewed.
///
/// One row per (person, kind): a renewal replaces, because nothing here asks
/// what an expired document used to say and the log keeps that history anyway.
async fn recorded(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    kind: crate::DocumentKind,
    number: &str,
    expires_on: chrono::NaiveDate,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO employee_document
             (employee, kind, number, expires_on, recorded_at, position)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (employee, kind) DO UPDATE
             SET number = EXCLUDED.number,
                 expires_on = EXCLUDED.expires_on,
                 recorded_at = EXCLUDED.recorded_at,
                 position = EXCLUDED.position",
    )
    .bind(id)
    .bind(kind.as_str())
    .bind(number)
    .bind(expires_on)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Their details changed. **Never their reporting line** — that is `reparented`
/// below, and the separation is what makes a move readable to an auditor.
async fn amended(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    details: &crate::Details,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "UPDATE employee
            SET name = $2, name_latin = $3, national_id = $4,
                email = $5, phone = $6, recorded_at = $7, position = $8
          WHERE id = $1",
    )
    .bind(id)
    .bind(&details.name)
    .bind(details.name_latin.as_deref())
    .bind(details.national_id.as_deref())
    .bind(details.email.as_deref())
    .bind(details.phone.as_deref())
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Somebody moved in the chart.
///
/// Its own function rather than an arm, because `apply` reads better as six
/// one-line arms than as one function over the line limit — and the workspace
/// refuses the shortcut of one statement with the column name interpolated,
/// which is a lint worth having even where the value is a literal.
async fn reparented(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    to: Option<&erp_types::AggregateId>,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "UPDATE employee SET reports_to = $2, recorded_at = $3, position = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(to.map(erp_types::AggregateId::as_str))
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Somebody moved branch.
async fn transferred(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    to: Option<&erp_types::AggregateId>,
) -> Result<(), ProjectionError> {
    sqlx::query("UPDATE employee SET branch = $2, recorded_at = $3, position = $4 WHERE id = $1")
        .bind(id)
        .bind(to.map(erp_types::AggregateId::as_str))
        .bind(ctx.event_time())
        .bind(ctx.position().get())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Hr>>> {
    vec![std::sync::Arc::new(EmployeeList)]
}

// -------------------------------------------------------------------- reads

/// A person, as a chart shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmployeeSummary {
    pub id: String,
    pub name: String,
    pub name_latin: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// Who they report to. `None` for the root.
    pub reports_to: Option<String>,
    /// Where they work. `None` for a company-wide role.
    pub branch: Option<String>,
    pub hired_on: Timestamp,
    pub left_at: Option<Timestamp>,
}

macro_rules! summarise {
    ($r:expr) => {{
        let r = $r;
        EmployeeSummary {
            id: r.id,
            name: r.name,
            name_latin: r.name_latin,
            email: r.email,
            phone: r.phone,
            reports_to: r.reports_to,
            branch: r.branch,
            hired_on: r.hired_on,
            left_at: r.left_at,
        }
    }};
}

/// People, by name.
///
/// **`branch` is a filter and not a wall**, which is the distinction §9d draws:
/// `ledger::post_entry_in` *refuses* a document dated to a branch that is not
/// open, and this *narrows* to a branch the caller can widen. Payroll and an
/// org chart are company-wide by nature, and a boundary that refused them would
/// make the module unusable in its first month.
pub async fn employees(
    conn: &mut PgConnection,
    branch: Option<&str>,
    include_leavers: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<EmployeeSummary>, sqlx::Error> {
    let (name, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, email, phone,
                  reports_to, branch, hired_on as "hired_on!", left_at
             FROM proj_hr.employee
            WHERE ($4 OR left_at IS NULL)
              AND ($5::text IS NULL OR branch = $5)
              AND ($2::text = '' OR (name, id) > ($2, $3))
            ORDER BY name, id
            LIMIT $1"#,
        limit,
        name,
        id,
        include_leavers,
        branch,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter().map(|r| summarise!(r)).collect(),
        limit,
        |e| Cursor::over(&[&e.name, &e.id]),
    ))
}

/// One of them.
pub async fn employee(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<EmployeeSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, email, phone,
                  reports_to, branch, hired_on as "hired_on!", left_at
             FROM proj_hr.employee WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| summarise!(r)))
}

/// A document that has lapsed, or is about to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expiring {
    pub employee: String,
    pub name: String,
    pub branch: Option<String>,
    /// `identity`, `work_permit`, `medical` or `licence`.
    pub kind: String,
    pub number: String,
    pub expires_on: chrono::NaiveDate,
    /// Negative once it has gone. **Signed on purpose**: a screen that showed
    /// "0 days left" for both tomorrow and last March would be the screen
    /// somebody stops reading.
    pub days_left: i32,
}

/// Documents lapsing within `within_days`, soonest first.
///
/// What the expiry screen shows and what the health check reads. Includes ones
/// that have already gone — those are not warnings that were ignored, they are
/// people who may not be rostered, and burying them below the upcoming ones is
/// how they stay buried.
pub async fn expiring(
    conn: &mut PgConnection,
    within_days: i32,
    limit: i64,
) -> Result<Vec<Expiring>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT d.employee as "employee!", e.name as "name!", e.branch,
                  d.kind as "kind!", d.number as "number!",
                  d.expires_on as "expires_on!",
                  (d.expires_on - CURRENT_DATE)::int as "days_left!"
             FROM proj_hr.employee_document d
             JOIN proj_hr.employee e ON e.id = d.employee
            WHERE e.left_at IS NULL
              AND d.expires_on <= CURRENT_DATE + ($1::int)
            ORDER BY d.expires_on, d.employee
            LIMIT $2"#,
        within_days,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Expiring {
            employee: r.employee,
            name: r.name,
            branch: r.branch,
            kind: r.kind,
            number: r.number,
            expires_on: r.expires_on,
            days_left: r.days_left,
        })
        .collect())
}

/// What one person is paid, and the dates that bound their service.
///
/// Everything an end-of-service calculation needs, in one read. Together rather
/// than two calls because the two are always wanted together and a caller who
/// had to make both would eventually make one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayDetails {
    /// Basic plus allowances, which is the wage the Labour Law computes the
    /// award on.
    pub gross: Money,
    pub basic: Money,
    pub hired_on: Timestamp,
    /// `None` while they are still employed, which is the "what would we owe
    /// her" case.
    pub left_at: Option<Timestamp>,
}

/// What one person is paid, with their dates.
///
/// `None` when there is no salary recorded — which is different from a salary
/// of zero, and the caller has to be able to tell them apart.
pub async fn pay_details(
    conn: &mut PgConnection,
    employee: &str,
) -> Result<Option<PayDetails>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT s.basic as "basic!", s.gross as "gross!", s.currency as "currency!",
                  e.hired_on as "hired_on!", e.left_at
             FROM proj_hr.employee_salary s
             JOIN proj_hr.employee e ON e.id = s.employee
            WHERE s.employee = $1"#,
        employee,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(r) = row else { return Ok(None) };
    let currency =
        erp_types::CurrencyCode::new(&r.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(Some(PayDetails {
        gross: Money::from_minor(r.gross, currency),
        basic: Money::from_minor(r.basic, currency),
        hired_on: r.hired_on,
        left_at: r.left_at,
    }))
}

/// What one person is qualified to do.
///
/// **Empty means anything**, not nothing — the same rule the write side applies
/// in `Employee::can_perform`, and a caller rendering this has to say so. See
/// `Skills::restricted` on the HTTP surface.
pub async fn skills(conn: &mut PgConnection, employee: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT service as "service!" FROM proj_hr.employee_skill
            WHERE employee = $1 ORDER BY service"#,
        employee,
    )
    .fetch_all(&mut *conn)
    .await
}

/// Who can perform a service, for a rota screen picking somebody.
///
/// **Includes everybody with no skills recorded**, because an empty skill list
/// means no restriction — the same rule the write side applies, and stating it
/// once in each place is why they are asserted to agree in `hr/tests`.
pub async fn who_can_perform(
    conn: &mut PgConnection,
    service: &str,
    limit: i64,
) -> Result<Vec<EmployeeSummary>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT e.id as "id!", e.name as "name!", e.name_latin, e.email, e.phone,
                  e.reports_to, e.branch, e.hired_on as "hired_on!", e.left_at
             FROM proj_hr.employee e
            WHERE e.left_at IS NULL
              AND (EXISTS (SELECT 1 FROM proj_hr.employee_skill s
                            WHERE s.employee = e.id AND s.service = $1)
               OR NOT EXISTS (SELECT 1 FROM proj_hr.employee_skill s
                               WHERE s.employee = e.id))
            ORDER BY e.name, e.id
            LIMIT $2"#,
        service,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows.into_iter().map(|r| summarise!(r)).collect())
}

/// When somebody works, for a rota screen.
///
/// **Empty means no pattern recorded**, not "never" — the same rule the write
/// side applies in `Employee::is_working_at`, and the reason the HTTP shape
/// answers `rostered` alongside the list.
pub async fn shifts(
    conn: &mut PgConnection,
    employee: &str,
) -> Result<Vec<erp_recurrence::Availability>, sqlx::Error> {
    let row: Option<serde_json::Value> = sqlx::query_scalar!(
        r#"SELECT shifts as "shifts!" FROM proj_hr.employee_shift WHERE employee = $1"#,
        employee,
    )
    .fetch_optional(&mut *conn)
    .await?;

    // A rule that will not decode is a rule this build cannot read. Empty
    // rather than a failure, and the same call `booking` makes about a
    // resource's timetable — except that here empty means *no restriction*, so
    // the failure mode is a rota that shows everybody rather than nobody.
    Ok(row
        .map(|v| serde_json::from_value(v).unwrap_or_default())
        .unwrap_or_default())
}

/// A day somebody worked, as a timesheet shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkedDay {
    pub on: chrono::NaiveDate,
    pub minutes: i32,
    pub note: String,
}

/// Leave taken or booked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveTaken {
    /// `annual`, `sick`, `unpaid` or `statutory`.
    pub kind: String,
    pub from: chrono::NaiveDate,
    /// **Inclusive**, so the 3rd to the 5th is three days.
    pub until: chrono::NaiveDate,
    pub days: i32,
    pub why: String,
}

/// One person's timesheet over a window, oldest first.
pub async fn worked(
    conn: &mut PgConnection,
    employee: &str,
    from: chrono::NaiveDate,
    until: chrono::NaiveDate,
) -> Result<Vec<WorkedDay>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT on_date as "on!", minutes as "minutes!", note as "note!"
             FROM proj_hr.employee_day
            WHERE employee = $1 AND on_date BETWEEN $2 AND $3
            ORDER BY on_date"#,
        employee,
        from,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| WorkedDay {
            on: r.on,
            minutes: r.minutes,
            note: r.note,
        })
        .collect())
}

/// One person's leave that touches a window.
///
/// **Touches, not starts in.** A fortnight beginning in March is leave in April
/// too, and a report that only found the ones starting inside the window would
/// show a rota with somebody in it who is on a beach.
pub async fn leave(
    conn: &mut PgConnection,
    employee: &str,
    from: chrono::NaiveDate,
    until: chrono::NaiveDate,
) -> Result<Vec<LeaveTaken>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT kind as "kind!", from_date as "from!", until_date as "until!",
                  days as "days!", why as "why!"
             FROM proj_hr.employee_leave
            WHERE employee = $1 AND from_date <= $3 AND until_date >= $2
            ORDER BY from_date, kind"#,
        employee,
        from,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| LeaveTaken {
            kind: r.kind,
            from: r.from,
            until: r.until,
            days: r.days,
            why: r.why,
        })
        .collect())
}

/// How many days of each kind somebody has taken in a window.
///
/// **What a balance is drawn down by.** How much they are *entitled* to is
/// statute and belongs to the country module — this says what has gone, which
/// is the half that is the same everywhere.
pub async fn leave_taken(
    conn: &mut PgConnection,
    employee: &str,
    from: chrono::NaiveDate,
    until: chrono::NaiveDate,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT kind as "kind!", sum(days)::BIGINT as "days!"
             FROM proj_hr.employee_leave
            WHERE employee = $1 AND from_date <= $3 AND until_date >= $2
            GROUP BY kind ORDER BY kind"#,
        employee,
        from,
        until,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows.into_iter().map(|r| (r.kind, r.days)).collect())
}

/// A cursor over `(name, id)`, ascending — a staff list reads alphabetically,
/// the same one place `branches` does.
fn resume(after: Option<&Cursor>) -> (String, String) {
    match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].clone(), parts[1].clone()),
        _ => (String::new(), String::new()),
    }
}
