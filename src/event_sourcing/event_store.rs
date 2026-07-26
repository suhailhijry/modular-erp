use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[derive(Clone)]
pub struct EventEnvelope {
    pub id: u64,
    pub aggregate_domain: &'static str,
    pub aggregate_id: String,
    pub sequence: u64,
    pub event_name: &'static str,
    pub payload: serde_json::Value,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct SnapshotEnvelope {
    pub aggregate_domain: &'static str,
    pub aggregate_id: String,
    pub version: u64,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "Failed to persist {aggregate_domain}/{aggregate_id}: the last persisted version is higher than the current version {sequence}."
    )]
    Conflict {
        aggregate_domain: &'static str,
        aggregate_id: String,
        sequence: u64,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn load_events(
        &self,
        domain_name: &str,
        aggregate_id: &str,
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    async fn load_snapshot(
        &self,
        domain_name: &str,
        aggregate_id: &str,
    ) -> Result<Option<SnapshotEnvelope>, StoreError>;

    async fn save_events(&self, events: &mut [EventEnvelope]) -> Result<(), StoreError>;
    async fn save_snapshot(&self, snapshot: SnapshotEnvelope) -> Result<(), StoreError>;

    async fn load_events_for_aggregate_since(
        &self,
        domain_name: &str,
        aggregate_id: &str,
        version: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    async fn load_events_by_domain_since(
        &self,
        domain_name: &str,
        sequence: u64,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    async fn load_all_events_since(
        &self,
        sequence: u64,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    async fn load_all_unpublished_events(
        &self,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError>;

    async fn publish_event(&self, sequence: u64) -> Result<(), StoreError>;

    async fn position_at_or_after(&self, timestamp: DateTime<Utc>) -> Result<u64, StoreError>;
}
