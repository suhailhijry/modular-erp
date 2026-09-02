//! The org chart, for a screen.
//!
//! **Not what any authorization check reads.** The effective claim set is
//! write-side state in the tenant migration chain, for the reason §9c gives: a
//! command deciding whether somebody may approve something cannot read a table
//! that may be a second behind. This is for drawing the chart.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{Cursor, Page, Timestamp};
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
                sqlx::query(
                    "INSERT INTO employee
                         (id, name, name_latin, national_id, email, phone,
                          reports_to, branch, hired_on, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
                )
                .bind(id)
                .bind(&name)
                .bind(name_latin.as_deref())
                .bind(national_id.as_deref())
                .bind(email.as_deref())
                .bind(phone.as_deref())
                .bind(reports_to.as_ref().map(erp_types::AggregateId::as_str))
                .bind(branch.as_ref().map(erp_types::AggregateId::as_str))
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
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
                sqlx::query(
                    "UPDATE employee
                        SET name = $2, name_latin = $3, national_id = $4,
                            email = $5, phone = $6, recorded_at = $7, position = $8
                      WHERE id = $1",
                )
                .bind(id)
                .bind(&name)
                .bind(name_latin.as_deref())
                .bind(national_id.as_deref())
                .bind(email.as_deref())
                .bind(phone.as_deref())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            EmployeeEvent::Reparented { reports_to, .. } => {
                reparented(ctx, conn, id, reports_to.as_ref()).await?;
            }
            EmployeeEvent::Transferred { branch, .. } => {
                transferred(ctx, conn, id, branch.as_ref()).await?;
            }
            EmployeeEvent::Left { why, at } => {
                sqlx::query(
                    "UPDATE employee SET left_at = $2, left_why = $3,
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
            EmployeeEvent::Rehired { .. } => {
                sqlx::query(
                    "UPDATE employee SET left_at = NULL, left_why = NULL,
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

/// A cursor over `(name, id)`, ascending — a staff list reads alphabetically,
/// the same one place `branches` does.
fn resume(after: Option<&Cursor>) -> (String, String) {
    match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].clone(), parts[1].clone()),
        _ => (String::new(), String::new()),
    }
}
