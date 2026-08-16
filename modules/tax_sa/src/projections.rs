//! What has been filed.

use spa_eventlog::Envelope;
use spa_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use spa_types::{CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::filing::FilingEvent;

/// Filed returns — one group, and a small one.
///
/// It reads nothing from `proj_sales` or `proj_purchases` (architecture L3). The
/// numbers it holds were computed once, in the transaction that filed them, and
/// are in the event; recomputing them here would defeat the point of recording
/// what went to ZATCA.
#[derive(Debug)]
pub struct TaxSa;

impl ProjectionGroup for TaxSa {
    const NAME: &'static str = "tax_sa";
    const SCHEMA: &'static str = "proj_tax_sa";
}

#[derive(Debug)]
pub struct FiledReturns;

#[async_trait::async_trait]
impl Projection for FiledReturns {
    type Group = TaxSa;

    fn name(&self) -> &'static str {
        "filed_returns"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !FilingEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }

        let FilingEvent::Filed {
            from,
            until,
            currency,
            output_tax,
            input_tax,
            payable,
            filed_on,
            reference,
        } = ctx
            .decode::<FilingEvent>(envelope)
            .map_err(|source| ProjectionError::Decode {
                event_name: envelope.event_name.as_str().to_owned(),
                position: envelope.position,
                source,
            })?;

        sqlx::query(
            "INSERT INTO filed_return
                 (id, period_from, period_until, currency, output_tax, input_tax,
                  payable, filed_on, reference, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(envelope.stream.id.as_str())
        .bind(from)
        .bind(until)
        .bind(currency.as_str())
        .bind(output_tax.minor())
        .bind(input_tax.minor())
        .bind(payable.minor())
        .bind(filed_on)
        .bind(reference.as_deref())
        // The event's time, never the wall clock (L2).
        .bind(ctx.event_time())
        .execute(&mut *conn)
        .await?;

        Ok(())
    }
}

/// Every projection this module contributes.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = TaxSa>>> {
    vec![std::sync::Arc::new(FiledReturns)]
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A return as it was filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiledReturn {
    pub period: String,
    pub from: Timestamp,
    pub until: Timestamp,
    pub output_tax: Money,
    pub input_tax: Money,
    pub payable: Money,
    pub filed_on: Timestamp,
    /// ZATCA's acknowledgement, once clearance produces one.
    pub reference: Option<String>,
}

/// Everything filed, most recent period first.
pub async fn filed(conn: &mut PgConnection, limit: i64) -> Result<Vec<FiledReturn>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", period_from as "period_from!", period_until as "period_until!",
                  currency as "currency!", output_tax as "output_tax!",
                  input_tax as "input_tax!", payable as "payable!",
                  filed_on as "filed_on!", reference
             FROM proj_tax_sa.filed_return
            ORDER BY period_from DESC, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(FiledReturn {
                period: row.id,
                from: row.period_from,
                until: row.period_until,
                output_tax: Money::from_minor(row.output_tax, currency),
                input_tax: Money::from_minor(row.input_tax, currency),
                payable: Money::from_minor(row.payable, currency),
                filed_on: row.filed_on,
                reference: row.reference,
            })
        })
        .collect()
}
