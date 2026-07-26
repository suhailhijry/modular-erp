use async_trait::async_trait;

use crate::event_sourcing::event_store::EventEnvelope;

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, event: EventEnvelope) -> anyhow::Result<()>;
}
