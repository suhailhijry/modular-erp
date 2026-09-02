//! Runs and payslips, for a screen.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::run::{Payslip, RunEvent};

/// One group: a run and its payslips are read together and never apart.
#[derive(Debug)]
pub struct Payroll;

impl ProjectionGroup for Payroll {
    const NAME: &'static str = "payroll";
    const SCHEMA: &'static str = "proj_payroll";
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

/// Runs, as a screen lists them.
#[derive(Debug)]
pub struct RunList;

#[async_trait::async_trait]
impl Projection for RunList {
    type Group = Payroll;

    fn name(&self) -> &'static str {
        "runs"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !RunEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<RunEvent>(ctx, envelope)? {
            RunEvent::Drafted {
                period,
                payslips,
                gross,
                deductions,
                net,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO run
                         (id, period, gross, deductions, net, currency, people,
                          drafted_at, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                     ON CONFLICT (id) DO UPDATE
                         SET period = EXCLUDED.period, gross = EXCLUDED.gross,
                             deductions = EXCLUDED.deductions, net = EXCLUDED.net,
                             currency = EXCLUDED.currency, people = EXCLUDED.people,
                             drafted_at = EXCLUDED.drafted_at,
                             recorded_at = EXCLUDED.recorded_at,
                             position = EXCLUDED.position",
                )
                .bind(id)
                .bind(period.to_string())
                .bind(gross.minor())
                .bind(deductions.minor())
                .bind(net.minor())
                .bind(gross.currency().to_string())
                .bind(i32::try_from(payslips.len()).unwrap_or(i32::MAX))
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;

                // **Replaced, not accumulated.** A redraft is the whole run
                // again, and payslips left behind from a previous attempt would
                // be people paid twice on a report.
                sqlx::query("DELETE FROM payslip WHERE run = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                for slip in &payslips {
                    write_payslip(ctx, conn, id, slip).await?;
                }
            }
            RunEvent::Approved { entry, at } => {
                sqlx::query(
                    "UPDATE run SET approved_at = $2, entry = $3,
                            recorded_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(id)
                .bind(at)
                .bind(entry.as_str())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

async fn write_payslip(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    run: &str,
    slip: &Payslip,
) -> Result<(), ProjectionError> {
    sqlx::query(
        "INSERT INTO payslip
             (run, employee, name, basic, gross, deductions, net, currency,
              recorded_at, position)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(run)
    .bind(slip.employee.as_str())
    .bind(&slip.name)
    .bind(slip.basic.minor())
    .bind(slip.gross.minor())
    .bind(slip.deductions.minor())
    .bind(slip.net.minor())
    .bind(slip.gross.currency().to_string())
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Payroll>>> {
    vec![std::sync::Arc::new(RunList)]
}

// -------------------------------------------------------------------- reads

/// A run, as a screen lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub id: String,
    /// `YYYY-MM`.
    pub period: String,
    pub gross: Money,
    pub deductions: Money,
    pub net: Money,
    pub people: i32,
    pub drafted_at: Timestamp,
    /// Set when it posted.
    pub approved_at: Option<Timestamp>,
    /// The journal entry it made.
    pub entry: Option<String>,
}

/// One person's pay in one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayslipRow {
    pub employee: String,
    /// As it was when the run was made.
    pub name: String,
    pub basic: Money,
    pub gross: Money,
    pub deductions: Money,
    pub net: Money,
}

fn currency_of(code: &str) -> Result<CurrencyCode, sqlx::Error> {
    CurrencyCode::new(code).map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

/// Runs, newest period first.
pub async fn runs(
    conn: &mut PgConnection,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<RunSummary>, sqlx::Error> {
    let (period, id) = match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].clone(), parts[1].clone()),
        _ => (String::new(), String::new()),
    };

    let rows = sqlx::query!(
        r#"SELECT id as "id!", period as "period!", gross as "gross!",
                  deductions as "deductions!", net as "net!", currency as "currency!",
                  people as "people!", drafted_at as "drafted_at!", approved_at, entry
             FROM proj_payroll.run
            WHERE ($2::text = '' OR (period, id) < ($2, $3))
            ORDER BY period DESC, id DESC
            LIMIT $1"#,
        limit,
        period,
        id,
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let currency = currency_of(&r.currency)?;
        items.push(RunSummary {
            id: r.id,
            period: r.period,
            gross: Money::from_minor(r.gross, currency),
            deductions: Money::from_minor(r.deductions, currency),
            net: Money::from_minor(r.net, currency),
            people: r.people,
            drafted_at: r.drafted_at,
            approved_at: r.approved_at,
            entry: r.entry,
        });
    }

    Ok(Page::of(items, limit, |r| {
        Cursor::over(&[&r.period, &r.id])
    }))
}

/// One of them.
pub async fn run(conn: &mut PgConnection, id: &str) -> Result<Option<RunSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", period as "period!", gross as "gross!",
                  deductions as "deductions!", net as "net!", currency as "currency!",
                  people as "people!", drafted_at as "drafted_at!", approved_at, entry
             FROM proj_payroll.run WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(r) = row else { return Ok(None) };
    let currency = currency_of(&r.currency)?;
    Ok(Some(RunSummary {
        id: r.id,
        period: r.period,
        gross: Money::from_minor(r.gross, currency),
        deductions: Money::from_minor(r.deductions, currency),
        net: Money::from_minor(r.net, currency),
        people: r.people,
        drafted_at: r.drafted_at,
        approved_at: r.approved_at,
        entry: r.entry,
    }))
}

/// The payslips in a run, by name.
pub async fn payslips(conn: &mut PgConnection, run: &str) -> Result<Vec<PayslipRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT employee as "employee!", name as "name!", basic as "basic!",
                  gross as "gross!", deductions as "deductions!", net as "net!",
                  currency as "currency!"
             FROM proj_payroll.payslip WHERE run = $1 ORDER BY name, employee"#,
        run
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let currency = currency_of(&r.currency)?;
        out.push(PayslipRow {
            employee: r.employee,
            name: r.name,
            basic: Money::from_minor(r.basic, currency),
            gross: Money::from_minor(r.gross, currency),
            deductions: Money::from_minor(r.deductions, currency),
            net: Money::from_minor(r.net, currency),
        });
    }
    Ok(out)
}
