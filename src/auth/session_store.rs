use async_trait::async_trait;

use crate::auth::audience::Audience;

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub identity_id: String,
    pub audience: Audience,
    pub created_at: i64,
    pub last_seen_at: i64,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, identity_id: &str, audience: Audience) -> anyhow::Result<String>;
    async fn resolve(&self, session_id: &str) -> anyhow::Result<Option<SessionRecord>>;
    async fn revoke(&self, session_id: &str) -> anyhow::Result<()>;
    async fn sessions_for_identity(&self, user_id: &str) -> anyhow::Result<Vec<SessionRecord>>;
    async fn revoke_all_for_identity(&self, user_id: &str) -> anyhow::Result<u64>;
}
