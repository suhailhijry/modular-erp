use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::PgPool;

use crate::{
    event_sourcing::{CheckpointStore, EventBus, EventStore},
    platform::CommandQueue,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub event_store: Arc<dyn EventStore>,
    pub checkpoint_store: Arc<dyn CheckpointStore>,
    pub event_bus: Arc<dyn EventBus>,
    pub queue: Arc<CommandQueue>,
}

impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}
