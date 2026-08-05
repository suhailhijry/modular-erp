use std::time::Duration;

use async_trait::async_trait;

use crate::auth::{
    audience::Audience,
    crypto::{generate_token, sha256_hex},
    session_store::{SessionRecord, SessionStore},
};

pub struct PgSessionStore {
    pub pool: sqlx::PgPool,
    pub session_ttl: Duration,
    pub sliding: bool,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

#[async_trait]
impl SessionStore for PgSessionStore {
    async fn create(&self, identity_id: &str, audience: Audience) -> anyhow::Result<String> {
        let session_id = generate_token();
        let hash = sha256_hex(&session_id);
        let now = now_ts();
        sqlx::query(
            "INSERT INTO sessions (session_hash, identity_id, audience, created_at, last_seen_at, expires_at)
             VALUES ($1, $2, $3, $4, $4, $5)",
        )
        .bind(&hash)
        .bind(identity_id)
        .bind(audience.to_string())
        .bind(now)
        .bind(now + self.session_ttl.as_secs() as i64)
        .execute(&self.pool)
        .await?;
        Ok(session_id)
    }

    async fn resolve(&self, session_id: &str) -> anyhow::Result<Option<SessionRecord>> {
        let hash = sha256_hex(session_id);
        let now = now_ts();

        let row: Option<(String, String, i64, i64, i64)> = sqlx::query_as(
            "SELECT identity_id, audience, created_at, last_seen_at, expires_at
             FROM sessions WHERE session_hash = $1",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;

        let Some((identity_id, audience, created_at, last_seen_at, expires_at)) = row else {
            return Ok(None);
        };
        if expires_at <= now {
            // Lazy cleanup: expired rows die on first touch; a periodic
            // DELETE WHERE expires_at < now sweeps the untouched rest.
            let _ = sqlx::query("DELETE FROM sessions WHERE session_hash = $1")
                .bind(&hash)
                .execute(&self.pool)
                .await;
            return Ok(None);
        }
        let Ok(audience) = Audience::try_from(audience) else {
            return Ok(None); // malformed row = invalid session, fail closed
        };

        // Throttled bookkeeping, best-effort - same policy as Redis impl.
        if now - last_seen_at >= 60 {
            let new_expiry = if self.sliding {
                now + self.session_ttl.as_secs() as i64
            } else {
                expires_at
            };
            let _ = sqlx::query(
                "UPDATE sessions SET last_seen_at = $2, expires_at = $3 WHERE session_hash = $1",
            )
            .bind(&hash)
            .bind(now)
            .bind(new_expiry)
            .execute(&self.pool)
            .await;
        }

        Ok(Some(SessionRecord {
            identity_id,
            audience,
            created_at,
            last_seen_at,
        }))
    }

    async fn revoke(&self, session_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM sessions WHERE session_hash = $1")
            .bind(sha256_hex(session_id))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn sessions_for_identity(&self, identity_id: &str) -> anyhow::Result<Vec<SessionRecord>> {
        let now = now_ts();
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT identity_id, audience, created_at, last_seen_at
             FROM sessions WHERE identity_id = $1 AND expires_at > $2",
        )
        .bind(identity_id)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(identity_id, audience, created_at, last_seen_at)| {
                let audience = if let Ok(audience) = Audience::try_from(audience) {
                    audience
                } else {
                    return None;
                };
                Some(SessionRecord {
                    identity_id,
                    audience,
                    created_at,
                    last_seen_at,
                })
            })
            .collect())
    }

    async fn revoke_all_for_identity(&self, identity_id: &str) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE identity_id = $1")
            .bind(identity_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
