use std::sync::Arc;

use async_trait::async_trait;

use crate::event_sourcing::{EventBus, EventEnvelope, Projector, ProjectorMeta};

#[derive(Default)]
pub struct ReactorRegistry {
    reactors: Vec<Arc<dyn Projector>>,
}

impl ReactorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, reactor: Arc<dyn Projector>) -> Self {
        self.reactors.push(reactor);
        self
    }

    pub fn build(self) -> CompositeEventBus {
        CompositeEventBus {
            reactors: self.reactors,
        }
    }
}

pub struct CompositeEventBus {
    reactors: Vec<Arc<dyn Projector>>,
}

impl CompositeEventBus {
    pub fn new(reactors: Vec<Arc<dyn Projector>>) -> Self {
        Self { reactors }
    }
}

#[async_trait]
impl EventBus for CompositeEventBus {
    async fn publish(&self, event: EventEnvelope) -> anyhow::Result<()> {
        for reactor in &self.reactors {
            reactor.handle(&event).await?;
        }
        Ok(())
    }
}

pub struct CompositeProjector {
    checkpoint_name: &'static str,
    reactors: Vec<Arc<dyn Projector>>,
}

impl CompositeProjector {
    pub fn new(checkpoint_name: &'static str, reactors: Vec<Arc<dyn Projector>>) -> Self {
        Self {
            checkpoint_name,
            reactors,
        }
    }
}

impl ProjectorMeta for CompositeProjector {
    fn name(&self) -> &'static str {
        self.checkpoint_name
    }
}

#[async_trait]
impl Projector for CompositeProjector {
    async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        for reactor in &self.reactors {
            reactor.handle(envelope).await?;
        }
        Ok(())
    }
}
