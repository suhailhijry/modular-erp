//! Redis-backed sessions. OTP challenges and CSRF stay on Postgres
//! (UNLOGGED tables are plenty for their traffic; sessions are the
//! per-request hot path that justifies Redis).
//!
//! Key layout:
//!   sess:{sha256(session_id)}      -> HASH {identity_id, audience, created_at,
//!                                    last_seen_at}, TTL = session lifetime
//!   identity_sessions:{identity_id}        -> SET of session-id hashes, for
//!                                    list/revoke-all-per-identity
//!
//! Design decisions:
//! - The PLAIN session id exists only in the client's cookie/token and
//!   in this process's memory during a request. Redis stores only its
//!   SHA-256: a dumped RDB/AOF or a compromised replica yields nothing
//!   presentable to the API.
//! - Revocation = DEL. A Redis flush logs everyone out - accepted
//!   tradeoff, stated in the design doc. If admin tooling needs a
//!   durable "sessions issued" audit trail, add an append-only Postgres
//!   log at issue/revoke time; the AUTHORITATIVE liveness check stays
//!   here.
//! - last_seen updates are throttled (once per minute) so the hot path
//!   is one HGETALL, not HGETALL+HSET on every request.
//!
//! Cargo: redis = { version = "0.27", features = ["tokio-comp", "connection-manager"] }
//!        sha2, rand, hex

use async_trait::async_trait;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use std::time::Duration;

use super::audience::Audience;
use super::crypto::{generate_token, sha256_hex};
use super::session_store::{SessionRecord, SessionStore};

#[derive(Clone)]
pub struct RedisSessionStore {
    conn: ConnectionManager,
    session_ttl: Duration,
    /// Sliding expiration: every resolve pushes the TTL back out. Set
    /// equal to session_ttl for fixed-lifetime sessions instead.
    sliding: bool,
}

fn hash_id(session_id: &str) -> String {
    sha256_hex(session_id)
}

fn sess_key(hashed: &str) -> String {
    format!("sess:{hashed}")
}

fn identity_index_key(identity_id: &str) -> String {
    format!("identity_sessions:{identity_id}")
}

impl RedisSessionStore {
    pub async fn connect(
        redis_url: &str,
        session_ttl: Duration,
        sliding: bool,
    ) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            session_ttl,
            sliding,
        })
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn create(&self, identity_id: &str, audience: Audience) -> anyhow::Result<String> {
        let session_id = generate_token();
        let hashed = hash_id(&session_id);
        let now = chrono::Utc::now().timestamp();

        let mut conn = self.conn.clone();
        let ttl_secs = self.session_ttl.as_secs() as i64;

        // Session hash + TTL, and the per-identity index. The index entry
        // gets a slightly LONGER TTL via periodic cleanup semantics:
        // stale members are pruned lazily in `sessions_for_identity`.
        redis::pipe()
            .atomic()
            .hset_multiple(
                sess_key(&hashed),
                &[
                    ("identity_id", identity_id),
                    ("audience", audience.into()),
                    ("created_at", &now.to_string()),
                    ("last_seen_at", &now.to_string()),
                ],
            )
            .expire(sess_key(&hashed), ttl_secs)
            .sadd(identity_index_key(identity_id), &hashed)
            .query_async::<()>(&mut conn)
            .await?;

        Ok(session_id)
    }

    /// The per-request hot path: one HGETALL; TTL refresh and last_seen
    /// write only when due.
    async fn resolve(&self, session_id: &str) -> anyhow::Result<Option<SessionRecord>> {
        let hashed = hash_id(session_id);
        let mut conn = self.conn.clone();

        let map: std::collections::HashMap<String, String> =
            conn.hgetall(sess_key(&hashed)).await?;
        if map.is_empty() {
            return Ok(None); // expired, revoked, or never existed - indistinguishable on purpose
        }

        let malformed = || anyhow::anyhow!("malformed session record in store");
        let record = SessionRecord {
            identity_id: map.get("identity_id").cloned().ok_or_else(malformed)?,
            audience: map
                .get("audience")
                .and_then(|s| {
                    if let Ok(try_from) = Audience::try_from(s.as_str()) {
                        Some(try_from)
                    } else {
                        None
                    }
                })
                .ok_or_else(malformed)?,
            created_at: map
                .get("created_at")
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?,
            last_seen_at: map
                .get("last_seen_at")
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?,
        };

        let now = chrono::Utc::now().timestamp();
        if now - record.last_seen_at >= 60 {
            // Throttled bookkeeping; best-effort (a lost last_seen is
            // cosmetic, never worth failing the request over).
            let mut pipe = redis::pipe();
            pipe.hset(sess_key(&hashed), "last_seen_at", now);
            if self.sliding {
                pipe.expire(sess_key(&hashed), self.session_ttl.as_secs() as i64);
            }
            let _ = pipe.query_async::<()>(&mut conn).await;
        }

        Ok(Some(record))
    }

    async fn revoke(&self, session_id: &str) -> anyhow::Result<()> {
        let hashed = hash_id(session_id);
        let mut conn = self.conn.clone();
        // Fetch identity_id first so the index entry goes too.
        let identity_id: Option<String> = conn.hget(sess_key(&hashed), "identity_id").await?;
        let mut pipe = redis::pipe();
        pipe.del(sess_key(&hashed));
        if let Some(uid) = identity_id {
            pipe.srem(identity_index_key(&uid), &hashed);
        }
        pipe.query_async::<()>(&mut conn).await?;
        Ok(())
    }

    /// Live sessions for a identity (admin tooling / "log out everywhere").
    /// Lazily prunes index members whose session hash has expired.
    async fn sessions_for_identity(&self, identity_id: &str) -> anyhow::Result<Vec<SessionRecord>> {
        let mut conn = self.conn.clone();
        let hashes: Vec<String> = conn.smembers(identity_index_key(identity_id)).await?;
        let mut live = Vec::new();
        for hashed in hashes {
            let map: std::collections::HashMap<String, String> =
                conn.hgetall(sess_key(&hashed)).await?;
            if map.is_empty() {
                let _: () = conn.srem(identity_index_key(identity_id), &hashed).await?; // prune stale
                continue;
            }
            if let (Some(uid), Some(aud), Some(c), Some(l)) = (
                map.get("identity_id"),
                map.get("audience").and_then(|s| {
                    if let Ok(try_from) = Audience::try_from(s.as_str()) {
                        Some(try_from)
                    } else {
                        None
                    }
                }),
                map.get("created_at").and_then(|s| s.parse().ok()),
                map.get("last_seen_at").and_then(|s| s.parse().ok()),
            ) {
                live.push(SessionRecord {
                    identity_id: uid.clone(),
                    audience: aud,
                    created_at: c,
                    last_seen_at: l,
                });
            }
        }
        Ok(live)
    }

    /// "Log out everywhere" - password change, suspected compromise,
    /// identity suspension.
    async fn revoke_all_for_identity(&self, identity_id: &str) -> anyhow::Result<u64> {
        let mut conn = self.conn.clone();
        let hashes: Vec<String> = conn.smembers(identity_index_key(identity_id)).await?;
        if hashes.is_empty() {
            return Ok(0);
        }
        let mut pipe = redis::pipe();
        for hashed in &hashes {
            pipe.del(sess_key(hashed));
        }
        pipe.del(identity_index_key(identity_id));
        pipe.query_async::<()>(&mut conn).await?;
        Ok(hashes.len() as u64)
    }
}
