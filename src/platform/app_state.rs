use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    event_sourcing::{CheckpointStore, EventBus, EventStore},
    platform::CommandQueue,
};

#[derive(Clone)]
pub struct AppState {
    pub write_pool: PgPool,
    pub read_pool: PgPool,
    pub event_store: Arc<dyn EventStore>,
    pub checkpoint_store: Arc<dyn CheckpointStore>,
    pub event_bus: Option<Arc<dyn EventBus>>,
    pub queue: Arc<CommandQueue>,
}
