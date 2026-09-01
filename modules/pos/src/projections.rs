//! What each till took, and what its drawer disagreed by.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{CurrencyCode, Cursor, Money, Page, Timestamp};
use sqlx::PgConnection;

use crate::shift::{ShiftEvent, Tender};

/// One group, two tables.
///
/// **No invoice table.** A till sale is a `sales` invoice and lives in
/// `proj_sales`; the screen that lists a shift's receipts reads that group. This
/// module never copies it, which is what keeps "what did we sell today" a
/// question with one answer.
#[derive(Debug)]
pub struct Pos;

impl ProjectionGroup for Pos {
    const NAME: &'static str = "pos";
    const SCHEMA: &'static str = "proj_pos";
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

/// Tills and their drawers.
#[derive(Debug)]
pub struct Shifts;

#[async_trait::async_trait]
impl Projection for Shifts {
    type Group = Pos;

    fn name(&self) -> &'static str {
        "shifts"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !ShiftEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str();

        match decode::<ShiftEvent>(ctx, envelope)? {
            ShiftEvent::Opened {
                till,
                operator,
                float,
                at,
            } => {
                sqlx::query(
                    "INSERT INTO shift
                         (id, till, operator, float, expected, currency,
                          opened_at, recorded_at, position)
                     VALUES ($1,$2,$3,$4,$4,$5,$6,$7,$8)",
                )
                .bind(id)
                .bind(&till)
                .bind(&operator)
                .bind(float.minor())
                .bind(float.currency().as_str())
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ShiftEvent::Sold { tenders, .. } => {
                bump(ctx, conn, id, &tenders, 1, false).await?;
            }
            ShiftEvent::Refunded { tenders, .. } => {
                bump(ctx, conn, id, &tenders, 0, true).await?;
            }
            ShiftEvent::PaidOut { amount, .. } => {
                // Cash only, and it comes straight off what the drawer should
                // hold. It is not a taking, so no `taking` row moves.
                sqlx::query(
                    "UPDATE shift SET expected = expected - $2, recorded_at = $3, position = $4
                      WHERE id = $1",
                )
                .bind(id)
                .bind(amount.minor())
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
            ShiftEvent::Closed {
                expected,
                declared,
                variance,
                at,
            } => {
                // `expected` is written from the event rather than left as the
                // running total: the event is what was decided (L5), and if the
                // two ever disagreed the event is right.
                sqlx::query(
                    "UPDATE shift
                        SET expected = $2, declared = $3, variance = $4, closed_at = $5,
                            recorded_at = $6, position = $7
                      WHERE id = $1",
                )
                .bind(id)
                .bind(expected.minor())
                .bind(declared.minor())
                .bind(variance.minor())
                .bind(at)
                .bind(ctx.event_time())
                .bind(ctx.position().get())
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Moves the takings and the expected drawer for a set of tenders.
///
/// `sold` counts a sale; `giving_back` flips the direction. Only cash touches
/// `expected`, which is [`Method::is_in_the_drawer`] applied in SQL.
async fn bump(
    ctx: &ProjectionCtx<'_>,
    conn: &mut PgConnection,
    id: &str,
    tenders: &[Tender],
    sold: i32,
    giving_back: bool,
) -> Result<(), ProjectionError> {
    for tender in tenders {
        sqlx::query(
            "INSERT INTO taking (shift, method, taken, refunded, currency, recorded_at, position)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (shift, method) DO UPDATE
                SET taken = taking.taken + EXCLUDED.taken,
                    refunded = taking.refunded + EXCLUDED.refunded,
                    recorded_at = EXCLUDED.recorded_at,
                    position = EXCLUDED.position",
        )
        .bind(id)
        .bind(tender.method.as_str())
        .bind(if giving_back {
            0
        } else {
            tender.amount.minor()
        })
        .bind(if giving_back {
            tender.amount.minor()
        } else {
            0
        })
        .bind(tender.amount.currency().as_str())
        .bind(ctx.event_time())
        .bind(ctx.position().get())
        .execute(&mut *conn)
        .await?;
    }

    let cash: i64 = tenders
        .iter()
        .filter(|tender| tender.method.is_in_the_drawer())
        .map(|tender| tender.amount.minor())
        .sum();
    let movement = if giving_back { -cash } else { cash };

    sqlx::query(
        "UPDATE shift
            SET expected = expected + $2, sales_count = sales_count + $3,
                recorded_at = $4, position = $5
          WHERE id = $1",
    )
    .bind(id)
    .bind(movement)
    .bind(sold)
    .bind(ctx.event_time())
    .bind(ctx.position().get())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Pos>>> {
    vec![std::sync::Arc::new(Shifts)]
}

/// The one place a row becomes a summary, so the list and the single read
/// cannot drift apart.
///
/// A macro and not a function because `sqlx::query!` gives each query its own
/// anonymous row type, and the two here are structurally identical without
/// sharing a name anything could be written against.
macro_rules! summarise {
    ($r:expr) => {{
        let r = $r;
        let currency = currency_of(&r.currency);
        ShiftSummary {
            id: r.id,
            till: r.till,
            operator: r.operator,
            float: Money::from_minor(r.float, currency),
            expected: Money::from_minor(r.expected, currency),
            declared: r.declared.map(|m| Money::from_minor(m, currency)),
            variance: r.variance.map(|m| Money::from_minor(m, currency)),
            sales_count: u32::try_from(r.sales_count).unwrap_or(0),
            opened_at: r.opened_at,
            closed_at: r.closed_at,
        }
    }};
}

// -------------------------------------------------------------------- reads

/// A till, as the report shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShiftSummary {
    pub id: String,
    pub till: String,
    pub operator: String,
    pub float: Money,
    /// What the drawer should hold. A running total while the shift is open,
    /// and what the count was measured against once it is shut.
    pub expected: Money,
    /// What was counted. Absent while it is still open.
    pub declared: Option<Money>,
    /// **The number a manager reads.** Negative is short.
    pub variance: Option<Money>,
    pub sales_count: u32,
    pub opened_at: Timestamp,
    pub closed_at: Option<Timestamp>,
}

/// What one shift took, by how it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakingRow {
    /// `cash`, `card` or `transfer`.
    pub method: String,
    pub taken: Money,
    pub refunded: Money,
}

/// Shifts, newest first.
///
/// `till` is optional so the same read serves one counter's day and the whole
/// shop's.
pub async fn shifts(
    conn: &mut PgConnection,
    till: Option<&str>,
    open_only: bool,
    limit: i64,
    after: Option<&Cursor>,
) -> Result<Page<ShiftSummary>, sqlx::Error> {
    let (opened_at, id) = resume(after);
    let rows = sqlx::query!(
        r#"SELECT id as "id!", till as "till!", operator as "operator!",
                  float as "float!", expected as "expected!", declared, variance,
                  currency as "currency!", sales_count as "sales_count!",
                  opened_at as "opened_at!", closed_at
             FROM proj_pos.shift
            WHERE ($4::text IS NULL OR till = $4)
              AND (NOT $5 OR closed_at IS NULL)
              AND ($2::timestamptz IS NULL OR (opened_at, id) < ($2, $3))
            ORDER BY opened_at DESC, id DESC
            LIMIT $1"#,
        limit,
        opened_at,
        id,
        till,
        open_only,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(Page::of(
        rows.into_iter().map(|r| summarise!(r)).collect(),
        limit,
        |s| Cursor::over(&[&s.opened_at.to_rfc3339(), &s.id]),
    ))
}

/// One of them.
pub async fn shift(conn: &mut PgConnection, id: &str) -> Result<Option<ShiftSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", till as "till!", operator as "operator!",
                  float as "float!", expected as "expected!", declared, variance,
                  currency as "currency!", sales_count as "sales_count!",
                  opened_at as "opened_at!", closed_at
             FROM proj_pos.shift WHERE id = $1"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|r| summarise!(r)))
}

/// What one shift took, by method, in a stable order.
pub async fn takings(conn: &mut PgConnection, shift: &str) -> Result<Vec<TakingRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT method as "method!", taken as "taken!", refunded as "refunded!",
                  currency as "currency!"
             FROM proj_pos.taking WHERE shift = $1 ORDER BY method"#,
        shift
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let currency = currency_of(&r.currency);
            TakingRow {
                method: r.method,
                taken: Money::from_minor(r.taken, currency),
                refunded: Money::from_minor(r.refunded, currency),
            }
        })
        .collect())
}

fn resume(after: Option<&Cursor>) -> (Option<Timestamp>, String) {
    match after.map(Cursor::parts) {
        Some(parts) if parts.len() == 2 => (parts[0].parse().ok(), parts[1].clone()),
        _ => (None, String::new()),
    }
}

/// A currency code this module wrote and which is therefore valid.
///
/// Falls back to the tenant's own rather than failing the read, for the reason
/// `prepaid` does: a row with an unreadable currency is corrupt, and the number
/// beside it is still worth showing while somebody works out how it got there.
fn currency_of(code: &str) -> CurrencyCode {
    CurrencyCode::new(code).unwrap_or_else(|_| {
        CurrencyCode::new("SAR").unwrap_or_else(|_| unreachable!("SAR is a real code"))
    })
}
