use async_trait::async_trait;

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn load(&self, projector: &str) -> anyhow::Result<Option<u64>>;
    async fn save(&self, projector: &str, position: u64) -> anyhow::Result<()>;
}
