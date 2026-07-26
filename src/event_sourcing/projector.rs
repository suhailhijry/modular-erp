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
