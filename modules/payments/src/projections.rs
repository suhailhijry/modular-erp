//! What was collected, as a table.

use erp_eventlog::Envelope;
use erp_projection::{Projection, ProjectionCtx, ProjectionError, ProjectionGroup};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use sqlx::PgConnection;

use crate::payment::PaymentEvent;

/// This module's projection group.
///
/// One group and one table: everything a person asks about a payment is about
/// **that payment**, so there is nothing here that would need a second group to
/// stay consistent with (L3).
#[derive(Debug)]
pub struct Payments;

impl ProjectionGroup for Payments {
    const NAME: &'static str = "payments";
    const SCHEMA: &'static str = "proj_payments";
}

/// Every projection this module runs.
#[must_use]
pub fn projections() -> Vec<std::sync::Arc<dyn Projection<Group = Payments>>> {
    vec![std::sync::Arc::new(Collected)]
}

#[derive(Debug)]
pub struct Collected;

#[async_trait::async_trait]
impl Projection for Collected {
    type Group = Payments;

    fn name(&self) -> &'static str {
        "collected"
    }

    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError> {
        if !PaymentEvent::NAMES.contains(&envelope.event_name.as_str()) {
            return Ok(());
        }
        let id = envelope.stream.id.as_str().to_owned();
        let position = envelope.position;
        let event: PaymentEvent =
            ctx.decode(envelope)
                .map_err(|source| ProjectionError::Decode {
                    event_name: envelope.event_name.as_str().to_owned(),
                    position: envelope.position,
                    source,
                })?;

        match event {
            PaymentEvent::Started {
                provider,
                gateway_id,
                invoice,
                amount,
                started_at,
            } => {
                sqlx::query(
                    "INSERT INTO payment
                        (id, provider, gateway_id, invoice, amount_minor, currency,
                         stage, started_at, position)
                     VALUES ($1, $2, $3, $4, $5, $6, 'pending', $7, $8)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(&id)
                .bind(&provider)
                .bind(&gateway_id)
                .bind(invoice.as_str())
                .bind(amount.minor())
                .bind(amount.currency().to_string())
                .bind(started_at)
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
            PaymentEvent::Settled {
                amount,
                fee,
                settled_at,
                ..
            } => {
                sqlx::query(
                    "UPDATE payment
                        SET stage = 'settled', amount_minor = $2, fee_minor = $3,
                            settled_at = $4, position = $5
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(amount.minor())
                .bind(fee.map(Money::minor))
                .bind(settled_at)
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
            PaymentEvent::Failed { why, .. } => {
                sqlx::query(
                    "UPDATE payment SET stage = 'failed', failed_why = $2, position = $3
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(&why)
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
            PaymentEvent::Refunded { amount, .. } => {
                // **The stage follows the arithmetic**, in SQL, so a replay
                // reaches the same answer as the aggregate did.
                sqlx::query(
                    "UPDATE payment
                        SET refunded_minor = refunded_minor + $2,
                            stage = CASE
                                WHEN refunded_minor + $2 >= amount_minor THEN 'refunded'
                                ELSE stage
                            END,
                            position = $3
                      WHERE id = $1",
                )
                .bind(&id)
                .bind(amount.minor())
                .bind(position)
                .execute(&mut *conn)
                .await?;
            }
            PaymentEvent::Voided { .. } => {
                sqlx::query("UPDATE payment SET stage = 'voided', position = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(position)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        Ok(())
    }
}

/// One attempt, as somebody reading a screen sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRow {
    pub id: String,
    pub provider: String,
    pub gateway_id: String,
    pub invoice: String,
    pub amount: Money,
    pub stage: String,
    pub fee: Option<Money>,
    pub refunded: Money,
    pub failed_why: Option<String>,
    pub started_at: Timestamp,
    pub settled_at: Option<Timestamp>,
}

/// **The lookup a callback makes.** A gateway names its own id and nothing
/// else, so this is what turns one into a payment this system knows about.
pub async fn by_gateway_id(
    conn: &mut sqlx::PgConnection,
    provider: &str,
    gateway_id: &str,
) -> Result<Option<AggregateId>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!" FROM proj_payments.payment
            WHERE provider = $1 AND gateway_id = $2"#,
        provider,
        gateway_id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|r| AggregateId::new(&r.id).ok()))
}

/// One payment.
pub async fn payment(
    conn: &mut sqlx::PgConnection,
    id: &str,
) -> Result<Option<PaymentRow>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", provider as "provider!", gateway_id as "gateway_id!",
                  invoice as "invoice!", amount_minor as "amount_minor!",
                  currency as "currency!", stage as "stage!", fee_minor,
                  refunded_minor as "refunded_minor!", failed_why,
                  started_at as "started_at!", settled_at
             FROM proj_payments.payment WHERE id = $1"#,
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.and_then(|r| {
        let currency = CurrencyCode::new(&r.currency).ok()?;
        Some(PaymentRow {
            id: r.id,
            provider: r.provider,
            gateway_id: r.gateway_id,
            invoice: r.invoice,
            amount: Money::from_minor(r.amount_minor, currency),
            stage: r.stage,
            fee: r.fee_minor.map(|m| Money::from_minor(m, currency)),
            refunded: Money::from_minor(r.refunded_minor, currency),
            failed_why: r.failed_why,
            started_at: r.started_at,
            settled_at: r.settled_at,
        })
    }))
}

/// What has been tried against one invoice, newest first.
pub async fn against(
    conn: &mut sqlx::PgConnection,
    invoice: &str,
    limit: i64,
) -> Result<Vec<PaymentRow>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id as "id!", provider as "provider!", gateway_id as "gateway_id!",
                  invoice as "invoice!", amount_minor as "amount_minor!",
                  currency as "currency!", stage as "stage!", fee_minor,
                  refunded_minor as "refunded_minor!", failed_why,
                  started_at as "started_at!", settled_at
             FROM proj_payments.payment WHERE invoice = $1
            ORDER BY started_at DESC LIMIT $2"#,
        invoice,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let currency = CurrencyCode::new(&r.currency).ok()?;
            Some(PaymentRow {
                id: r.id,
                provider: r.provider,
                gateway_id: r.gateway_id,
                invoice: r.invoice,
                amount: Money::from_minor(r.amount_minor, currency),
                stage: r.stage,
                fee: r.fee_minor.map(|m| Money::from_minor(m, currency)),
                refunded: Money::from_minor(r.refunded_minor, currency),
                failed_why: r.failed_why,
                started_at: r.started_at,
                settled_at: r.settled_at,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    /// Every stage the aggregate can reach is one the table's constraint
    /// allows. A stage the projection writes and the schema refuses is a
    /// rebuild that fails halfway.
    #[test]
    fn every_stage_the_aggregate_reaches_is_one_the_table_takes() {
        use crate::payment::Stage;
        let allowed = include_str!("../schema/install.sql");
        for stage in [
            Stage::Pending,
            Stage::Settled,
            Stage::Failed,
            Stage::Refunded,
            Stage::Voided,
        ] {
            assert!(
                allowed.contains(&format!("'{}'", stage.as_str())),
                "the schema does not allow {}",
                stage.as_str()
            );
        }
    }
}
