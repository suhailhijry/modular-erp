use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{PgPool, postgres::PgListener};

use crate::event_sourcing::{
    CheckpointStore, EventBus, EventEnvelope, EventStore, Projector, ReplayScope, replay,
};

#[derive(Clone)]
pub struct PgNotifyEventBus {
    pool: PgPool,
    channel: &'static str,
}

impl PgNotifyEventBus {
    pub fn new(pool: PgPool, channel: &'static str) -> Self {
        Self { pool, channel }
    }
}

#[async_trait]
impl EventBus for PgNotifyEventBus {
    async fn publish(&self, event: EventEnvelope) -> anyhow::Result<()> {
        // Payload is just the position - a hint for "there's something
        // new at or before here", not the event data itself.
        sqlx::query!(
            "SELECT pg_notify($1, $2)",
            self.channel,
            event.id.to_string()
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

pub async fn run_pg_notify_listener(
    pool: PgPool,
    channel: &str,
    store: Arc<dyn EventStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    projector: Arc<dyn Projector>,
) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(&pool).await?;
    listener.listen(channel).await?;

    let mut fallback = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        replay(
            store.as_ref(),
            checkpoints.as_ref(),
            projector.as_ref(),
            ReplayScope::Everything,
            None,
            500,
        )
        .await?;

        tokio::select! {
            res = listener.recv() => { res?; }     // woke up: a NOTIFY arrived
            _ = fallback.tick() => {}               // woke up: safety-net poll
        }
    }
}
