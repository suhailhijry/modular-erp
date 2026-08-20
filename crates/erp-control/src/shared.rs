//! State that has to be the same on every node.
//!
//! # What this is for, and what it is not for
//!
//! It is **not** a second copy of [`TtlCache`](crate::cache::TtlCache). That
//! cache answers the entry path from process memory in nanoseconds at a 99.9%
//! hit rate; putting those lookups in Redis would replace a memory read with a
//! network round trip and make the hot path slower. It stays exactly where it
//! is.
//!
//! What Redis adds is the thing a per-process cache structurally cannot do:
//! **agreement between nodes.**
//!
//! ## 1. Sessions
//!
//! `ControlPlane::session` runs on every authenticated request and was
//! deliberately uncached, for a reason `cache.rs` states plainly: *a stale
//! membership for five seconds is survivable, a stale logout is not*. So the
//! busiest lookup in the system was the one query that always went to the
//! control database — the database that cannot be sharded.
//!
//! A **shared** cache resolves that, because a logout deletes the entry for
//! every node at once rather than for the node that happened to serve it.
//!
//! ## 2. Invalidation
//!
//! The entry caches invalidate locally on write. With one API process that is
//! complete; with three it means a role change reaches the other two only when
//! their TTL lapses. `cache.rs` named the answer when it was written — "out-of-
//! band invalidation" — and this is it: a write publishes what it changed, and
//! every node drops that key.
//!
//! # What happens when Redis is not there
//!
//! Every path degrades to **exactly the behaviour of the build before this
//! module existed**, and says so in the log:
//!
//! - a session read falls through to Postgres — correct, just slower;
//! - a session write is skipped — the next request pays for it again;
//! - an invalidation that cannot be published still happened locally, so the
//!   other nodes fall back to the TTL window that has always been documented.
//!
//! That is not law L6 being bent. L6 is about not degrading a *guarantee*;
//! nothing here is a guarantee that did not already have a documented bound.
//!
//! **The exception, stated plainly:** if a logout cannot reach Redis, that token
//! keeps working until the cached entry expires. [`SESSION_TTL`] is what bounds
//! it, and it is deliberately short for that reason and no other.

use std::time::Duration;

use redis::AsyncCommands as _;
use erp_types::{IdentityId, TenantId};

/// How long a session stays readable from Redis without being re-checked.
///
/// **This is the blast radius of a failed logout**, not a performance knob.
/// Sixty seconds, matching the order of the entry cache's own TTL: a revoked
/// credential that survives a minute is the bound this system already documents
/// for a revoked membership, and having one number to reason about is worth more
/// than shaving a few queries.
pub const SESSION_TTL: Duration = Duration::from_mins(1);

/// The channel invalidations are published on.
const CHANNEL: &str = "erp:invalidate";

/// What one node changed, so the others can forget it.
///
/// Serialized rather than sent as a bare string so a new variant is a
/// deserialization failure on old nodes rather than a silently ignored message —
/// during a rolling deploy the two versions are both live, and "the old pods
/// quietly stopped invalidating" is precisely the bug that would never be found.
///
/// Adjacently tagged — `{"what":"identity","which":"…"}` — rather than
/// internally, because serde cannot put an internal tag on a newtype variant
/// wrapping a string, and half of these are exactly that.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "what", content = "which", rename_all = "snake_case")]
pub enum Invalidate {
    Identity(IdentityId),
    Tenant(TenantId),
    Membership {
        identity: IdentityId,
        tenant: TenantId,
    },
    Platform(IdentityId),
    Entitlements(TenantId),
}

/// A connection to the shared cache, or nothing.
///
/// `None` is a supported deployment, not a broken one: a single API process has
/// nobody to agree with.
#[derive(Clone)]
pub struct Shared {
    conn: redis::aio::ConnectionManager,
    client: redis::Client,
}

impl std::fmt::Debug for Shared {
    /// Says nothing about the URL, which carries a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    /// Connects, or explains why not.
    ///
    /// `ConnectionManager` reconnects on its own, so a Redis restart is a few
    /// failed commands rather than a process that has to be bounced.
    pub async fn connect(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client.clone()).await?;
        Ok(Self { conn, client })
    }

    /// From `REDIS_URL`, or `None` when there is none.
    ///
    /// A URL that is set and **unusable** is an error rather than a shrug: an
    /// operator who configured Redis and typed it wrong wants to find out at
    /// start-up, not from a graph two days later.
    pub async fn from_env() -> Result<Option<Self>, redis::RedisError> {
        let Some(url) = std::env::var("REDIS_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
        else {
            tracing::info!(
                "REDIS_URL is not set; sessions are read from the control \
                 database on every request and cache invalidation reaches only \
                 the node that wrote it. Correct for one API process."
            );
            return Ok(None);
        };
        let shared = Self::connect(&url).await?;
        tracing::info!("shared cache connected");
        Ok(Some(shared))
    }

    // -----------------------------------------------------------------------
    // Sessions
    // -----------------------------------------------------------------------

    /// The cached session for a token digest, if there is one.
    ///
    /// A Redis failure is `None` — the caller falls through to Postgres, which
    /// is the source of truth and always was.
    pub async fn session(&self, digest: &[u8]) -> Option<crate::Session> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(session_key(digest)).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "shared session read failed; using the database");
            None
        });
        serde_json::from_str(&raw?).ok()
    }

    /// Remembers a session until [`SESSION_TTL`], and files it under its
    /// identity so `log_out_everywhere` can find it.
    pub async fn remember_session(&self, digest: &[u8], session: &crate::Session) {
        let Ok(encoded) = serde_json::to_string(session) else {
            return;
        };
        let mut conn = self.conn.clone();

        // The set has a TTL of its own so an identity that stops signing in does
        // not leave a key behind for ever. It is refreshed on every write, which
        // is what keeps it alive for as long as any session under it can be.
        let owned = owner_key(session.identity);
        let result: redis::RedisResult<()> = redis::pipe()
            .set_ex(session_key(digest), encoded, SESSION_TTL.as_secs())
            .ignore()
            .sadd(&owned, hex::encode(digest))
            .ignore()
            .expire(&owned, i64::try_from(SESSION_TTL.as_secs()).unwrap_or(60))
            .ignore()
            .query_async(&mut conn)
            .await;

        if let Err(e) = result {
            tracing::warn!(error = %e, "shared session write failed; the next request pays for it");
        }
    }

    /// Forgets one session, everywhere.
    ///
    /// **The failure here is the one that matters.** If this cannot reach Redis
    /// the token keeps working until [`SESSION_TTL`] lapses, on nodes that have
    /// it cached. Logged at `error` for that reason: it is the only place in
    /// this module where a Redis outage widens a security window rather than
    /// costing a query.
    pub async fn forget_session(&self, digest: &[u8]) {
        let mut conn = self.conn.clone();
        if let Err(e) = conn.del::<_, ()>(session_key(digest)).await {
            tracing::error!(
                error = %e,
                ttl_secs = SESSION_TTL.as_secs(),
                "could not clear a logged-out session from the shared cache; \
                 it stays usable until it expires"
            );
        }
    }

    /// Forgets every session an identity has.
    ///
    /// What "log out everywhere" and a suspension both need. The tokens are not
    /// derivable from the identity, so they are filed under it as they are
    /// cached; a member of the set whose key has already expired is a delete
    /// that finds nothing, which costs nothing.
    pub async fn forget_sessions_of(&self, identity: IdentityId) {
        let mut conn = self.conn.clone();
        let owned = owner_key(identity);
        let digests: Vec<String> = conn.smembers(&owned).await.unwrap_or_default();

        let mut pipe = redis::pipe();
        for digest in &digests {
            pipe.del(format!("erp:session:{digest}")).ignore();
        }
        pipe.del(&owned).ignore();

        if let Err(e) = pipe.query_async::<()>(&mut conn).await {
            tracing::error!(
                error = %e,
                ttl_secs = SESSION_TTL.as_secs(),
                "could not clear an identity's sessions from the shared cache; \
                 they stay usable until they expire"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Invalidation
    // -----------------------------------------------------------------------

    /// Tells every node to drop what this one just changed.
    ///
    /// Fire and forget by design: the local invalidation has already happened,
    /// so a failure here costs freshness on *other* nodes and nothing else — the
    /// TTL window that was the only behaviour before this existed.
    pub async fn publish(&self, what: &Invalidate) {
        let Ok(encoded) = serde_json::to_string(what) else {
            return;
        };
        let mut conn = self.conn.clone();
        if let Err(e) = conn.publish::<_, _, ()>(CHANNEL, encoded).await {
            tracing::warn!(
                error = %e, ?what,
                "could not broadcast an invalidation; other nodes will wait out the TTL"
            );
        }
    }

    /// Every invalidation published from now on, as a stream.
    ///
    /// A dedicated connection, because a subscribed Redis connection cannot
    /// serve ordinary commands — which is why this takes the client rather than
    /// borrowing the manager.
    pub async fn subscribe(&self) -> Result<redis::aio::PubSub, redis::RedisError> {
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(CHANNEL).await?;
        Ok(pubsub)
    }
}

/// A session's key.
///
/// Hex rather than the raw digest bytes: Redis keys are binary-safe, but a key
/// somebody can read in `redis-cli` is worth more than four bytes per entry when
/// the question is "why is this token still working". It reveals nothing the
/// `session.token_hash` column does not already hold — it is the same digest.
fn session_key(digest: &[u8]) -> String {
    format!("erp:session:{}", hex::encode(digest))
}

fn owner_key(identity: IdentityId) -> String {
    format!("erp:sessions-of:{identity}")
}

/// Applies invalidations from other nodes until the control plane is dropped.
///
/// # Why a `Weak`
///
/// This runs for the life of the process and holds nothing alive. A strong
/// `Arc<ControlPlane>` here would keep the pools open through shutdown and the
/// drain would never finish — a background task that outlives what it serves is
/// a leak with a heartbeat.
///
/// # Why failures loop rather than return
///
/// Losing the subscription is Redis restarting, and the answer is to resubscribe.
/// While it is down the nodes fall back to the TTL window, which is the only
/// behaviour this system had before — so the loop is a degradation that repairs
/// itself, not an error path.
pub fn apply_invalidations_in_background(
    control: &std::sync::Arc<crate::ControlPlane>,
) -> Option<tokio::task::JoinHandle<()>> {
    let shared = control.shared()?.clone();
    let weak = std::sync::Arc::downgrade(control);

    Some(tokio::spawn(async move {
        use futures_util::StreamExt as _;

        loop {
            if weak.upgrade().is_none() {
                return;
            }
            match shared.subscribe().await {
                Ok(mut pubsub) => {
                    tracing::info!("listening for cache invalidations");
                    let mut stream = pubsub.on_message();
                    while let Some(message) = stream.next().await {
                        let Some(control) = weak.upgrade() else {
                            return;
                        };
                        match message.get_payload::<String>() {
                            Ok(raw) => match serde_json::from_str::<Invalidate>(&raw) {
                                Ok(what) => control.apply_invalidation(&what),
                                // A node running a newer build sent something
                                // this one has no variant for. Loud, because the
                                // consequence is silent staleness for whatever
                                // it named.
                                Err(e) => tracing::error!(
                                    error = %e, %raw,
                                    "unreadable invalidation; this node may be serving stale \
                                     authorization for what it described"
                                ),
                            },
                            Err(e) => tracing::warn!(error = %e, "unreadable invalidation payload"),
                        }
                    }
                    tracing::warn!("invalidation subscription ended; resubscribing");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not subscribe to invalidations; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message shape that survives the wire.
    ///
    /// It has to, and the reason is a rolling deploy: two builds are live at
    /// once, and one that cannot read the other's invalidations stops
    /// invalidating without saying so.
    #[test]
    fn every_invalidation_round_trips() {
        let identity = IdentityId::new();
        let tenant = TenantId::new();
        for what in [
            Invalidate::Identity(identity),
            Invalidate::Tenant(tenant),
            Invalidate::Membership { identity, tenant },
            Invalidate::Platform(identity),
            Invalidate::Entitlements(tenant),
        ] {
            let encoded = serde_json::to_string(&what).expect("serializes");
            let back: Invalidate = serde_json::from_str(&encoded).expect("deserializes");
            assert_eq!(what, back, "{encoded}");
        }
    }

    /// Two different identities cannot collide on one key, and a session key
    /// cannot be mistaken for an owner key.
    #[test]
    fn keys_are_namespaced_and_distinct() {
        let a = IdentityId::new();
        let b = IdentityId::new();
        assert_ne!(owner_key(a), owner_key(b));
        assert_ne!(session_key(b"x"), owner_key(a));
        assert!(session_key(b"x").starts_with("erp:"));
        assert!(owner_key(a).starts_with("erp:"));
    }
}
