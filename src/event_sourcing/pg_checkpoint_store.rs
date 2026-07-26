use async_trait::async_trait;
use sqlx::PgPool;

use crate::event_sourcing::{CheckpointStore, StoreError};

#[derive(Clone)]
pub struct PgCheckpointStore {
    pool: PgPool,
}

impl PgCheckpointStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CheckpointStore for PgCheckpointStore {
    async fn load(&self, projector: &str) -> anyhow::Result<Option<u64>> {
        let row = sqlx::query!(
            "SELECT global_position FROM projector_checkpoints WHERE projector_name = $1",
            projector,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        Ok(row.map(|v| v.global_position as u64))
    }

    async fn save(&self, projector: &str, position: u64) -> anyhow::Result<()> {
        sqlx::query!(
            "INSERT INTO projector_checkpoints (projector_name, global_position)
             VALUES ($1, $2)
             ON CONFLICT (projector_name)
             DO UPDATE SET global_position = EXCLUDED.global_position
             WHERE projector_checkpoints.global_position < EXCLUDED.global_position", // never regress a snapshot
            projector,
            position as i64,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;
        Ok(())
    }
}
