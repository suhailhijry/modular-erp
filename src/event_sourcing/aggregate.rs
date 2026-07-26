use std::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};

pub trait DomainEvent:
    Serialize + DeserializeOwned + Clone + Debug + Send + Sync + 'static
{
    fn event_name(&self) -> &'static str;
}

pub trait AggregateMeta {
    fn domain_name() -> &'static str;
    fn id(&self) -> &str;
    fn version(&self) -> u64;
}

pub trait Aggregate:
    AggregateMeta + Default + Clone + Serialize + DeserializeOwned + Send + Sync + 'static
{
    type Event: DomainEvent;
    type Command: Clone;
    type Error: std::error::Error + Send + Sync + 'static;

    fn apply(&mut self, event: &Self::Event);
    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error>;
}
