use std::sync::Arc;

use chrono::Utc;

use crate::event_sourcing::{
    aggregate::{Aggregate, DomainEvent},
    event_bus::EventBus,
    event_store::{EventEnvelope, EventStore, StoreError},
};

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("events persisted but post-commit dispatch failed: {0}")]
    DispatchFailed(anyhow::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Context {
    queue: Vec<EventEnvelope>,
    metadata: serde_json::Value,
}

impl Context {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            metadata: serde_json::json!({}),
        }
    }

    pub fn with_metadata(metadata: serde_json::Value) -> Self {
        Self {
            queue: Vec::new(),
            metadata,
        }
    }

    pub fn queue_events<A: Aggregate>(
        &mut self,
        aggregate_id: &str,
        version: u64,
        events: Vec<A::Event>,
    ) {
        for (i, event) in events.into_iter().enumerate() {
            let payload = serde_json::to_value(&event).expect("event cannot be serialized.");
            let event_name = event.event_name();
            self.queue.push(EventEnvelope {
                id: 0,
                aggregate_domain: A::domain_name().to_string(),
                aggregate_id: aggregate_id.to_string(),
                sequence: version + i as u64,
                event_name: event_name.to_string(),
                payload,
                metadata: self.metadata.clone(),
                created_at: Utc::now(),
            });
        }
    }

    pub fn discard(&mut self) {
        self.queue.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub async fn commit(
        mut self,
        store: &dyn EventStore,
        bus: Option<Arc<dyn EventBus>>,
    ) -> Result<(), ContextError> {
        if self.queue.is_empty() {
            return Ok(());
        }

        store.save_events(&mut self.queue).await?;

        for envelope in self.queue.drain(..) {
            if let Some(bus) = bus.clone() {
                let result = bus.publish(envelope).await;
                if let Err(e) = result {
                    return Err(ContextError::DispatchFailed(e));
                }
            }
        }

        Ok(())
    }
}
