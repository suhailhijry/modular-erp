use async_trait::async_trait;

use crate::event_sourcing::event_store::EventEnvelope;

pub trait ProjectorMeta {
    fn name(&self) -> &'static str;
}

#[async_trait]
pub trait Projector: ProjectorMeta + Send + Sync {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()>;
}

pub enum ReplayScope<'a> {
    Everything,
    Domain(&'a str),
    Aggregate { domain: &'a str, id: &'a str },
}

pub enum HandleOutcome {
    Processed,
    WillRetry { attempt: i32 },
    DeadLettered,
}

#[async_trait]
pub trait AlertSink: Send + Sync {
    async fn alert(&self, message: &str);
}

pub struct RetryPolicy {
    pub max_attempts: i32,
}

pub async fn handle_with_dead_letter(
    projector: &dyn Projector,
    envelope: &EventEnvelope,
    pool: &sqlx::PgPool,
    alerts: &dyn AlertSink,
    policy: &RetryPolicy,
) -> anyhow::Result<HandleOutcome> {
    match projector.handle(envelope).await {
        Ok(()) => {
            // Clear any prior failure record - this position is fine
            // now, don't let stale attempt history linger.
            sqlx::query!(
                "DELETE FROM retry_attempts WHERE projector = $1 AND global_position = $2",
                projector.name(),
                envelope.sequence as i64,
            )
            .execute(pool)
            .await?;
            Ok(HandleOutcome::Processed)
        }
        Err(e) => {
            let row = sqlx::query!(
                "INSERT INTO retry_attempts (projector, global_position, attempt_count, last_error)
                                 VALUES ($1, $2, 1, $3)
                                 ON CONFLICT (projector, global_position)
                                 DO UPDATE SET attempt_count = retry_attempts.attempt_count + 1,
                                               last_error = EXCLUDED.last_error,
                                               last_attempted_at = now()
                                 RETURNING attempt_count, first_attempted_at",
                projector.name(),
                envelope.sequence as i64,
                e.to_string(),
            )
            .fetch_one(pool)
            .await?;

            let attempt = row.attempt_count;
            let first_failed_at = row.first_attempted_at;

            if attempt >= policy.max_attempts {
                sqlx::query!(
                    "INSERT INTO projector_dead_letters
                        (projector, global_position, aggregate_domain, aggregate_id, event_name, payload, error, attempt_count, first_failed_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                     projector.name(),
                     envelope.sequence as i64,
                     envelope.aggregate_domain,
                     envelope.aggregate_id,
                     envelope.event_name,
                        envelope.payload,
                        e.to_string(),
                        attempt,
                        first_failed_at,

                )
                .execute(pool)
                .await?;

                // Logs get missed; someone has to be actively pinged.
                let message = format!(
                    "[{}] permanently failed after {attempt} attempts on {}/{} ({}): {e}",
                    projector.name(),
                    envelope.aggregate_domain,
                    envelope.aggregate_id,
                    envelope.event_name,
                );
                tracing::error!(
                    projector = projector.name(),
                    position = envelope.sequence,
                    aggregate_domain = envelope.aggregate_domain,
                    aggregate_id = %envelope.aggregate_id,
                    event_name = envelope.event_name,
                    error = %e,
                    attempt,
                    "event permanently failed - quarantined for manual review"
                );
                alerts.alert(&message).await;

                Ok(HandleOutcome::DeadLettered)
            } else {
                tracing::warn!(
                    projector = projector.name(),
                    position = envelope.sequence,
                    error = %e,
                    attempt,
                    max_attempts = policy.max_attempts,
                    "handler failed, will retry"
                );
                Ok(HandleOutcome::WillRetry { attempt: attempt })
            }
        }
    }
}
