use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::event_sourcing::{EventEnvelope, EventStore, SnapshotEnvelope, StoreError};

const UNIQUE_VIOLATION: &str = "23505";

fn intern(s: &str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    if let Some(existing) = map.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    map.insert(s.to_owned(), leaked);
    leaked
}

#[derive(Clone)]
pub struct PgEventStore {
    pool: sqlx::PgPool,
}

impl PgEventStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventStore for PgEventStore {
    async fn load_events(
        &self,
        aggregate_domain: &str,
        aggregate_id: &str,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at, published_at
             FROM events
             WHERE aggregate_domain = $1 AND aggregate_id = $2
             ORDER BY sequence ASC",
             aggregate_domain,
             aggregate_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: intern(&record.aggregate_domain),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: intern(&record.event_name),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                    published_at: record.published_at,
                })
            })
            .collect()
    }

    async fn load_snapshot(
        &self,
        aggregate_domain: &str,
        aggregate_id: &str,
    ) -> Result<Option<SnapshotEnvelope>, StoreError> {
        let row = sqlx::query!("SELECT aggregate_domain, aggregate_id, version, payload, created_at FROM snapshots WHERE aggregate_domain = $1 AND aggregate_id = $2",
            aggregate_domain,
            aggregate_id,
        )
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        let Some(row) = row else { return Ok(None) };
        Ok(Some(SnapshotEnvelope {
            aggregate_domain: intern(&row.aggregate_domain),
            aggregate_id: row.aggregate_id,
            version: row.version as u64,
            payload: row.payload,
            created_at: row.created_at,
        }))
    }

    async fn save_events(&self, events: &mut [EventEnvelope]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }

        // Single transaction: either every envelope in this batch lands,
        // or none do - this is what makes a saga's multi-aggregate
        // commit atomic.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        for event in events.iter_mut() {
            let result = sqlx::query!(
                "INSERT INTO events (aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 RETURNING id",
                 event.aggregate_domain,
                 event.aggregate_id,
                 event.sequence as i64,
                 event.event_name,
                 event.payload,
                 event.metadata,
                 event.created_at,
            )
            .fetch_one(&mut *tx)
            .await;

            match result {
                Ok(row) => {
                    event.id = row.id as u64;
                }
                Err(sqlx::Error::Database(db_err))
                    if db_err.code().as_deref() == Some(UNIQUE_VIOLATION) =>
                {
                    // Whole transaction is abandoned (tx drops without
                    // commit below) - nothing partially written.
                    return Err(StoreError::Conflict {
                        aggregate_domain: event.aggregate_domain,
                        aggregate_id: event.aggregate_id.clone(),
                        sequence: event.sequence,
                    });
                }
                Err(e) => return Err(StoreError::Other(e.into())),
            }
        }

        tx.commit().await.map_err(|e| StoreError::Other(e.into()))?;
        Ok(())
    }

    async fn save_snapshot(&self, snapshot: SnapshotEnvelope) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO snapshots (aggregate_domain, aggregate_id, version, payload)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (aggregate_domain, aggregate_id)
             DO UPDATE SET version = EXCLUDED.version, payload = EXCLUDED.payload
             WHERE snapshots.version < EXCLUDED.version", // never regress a snapshot
            snapshot.aggregate_domain,
            snapshot.aggregate_id,
            snapshot.version as i64,
            snapshot.payload,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;
        Ok(())
    }

    async fn load_events_for_aggregate_since(
        &self,
        aggregate_domain: &str,
        aggregate_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at, published_at
             FROM events
             WHERE aggregate_domain = $1 AND aggregate_id = $2 AND sequence > $3
             ORDER BY sequence ASC",
             aggregate_domain,
             aggregate_id,
             after_sequence as i64,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: intern(&record.aggregate_domain),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: intern(&record.event_name),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                    published_at: record.published_at,
                })
            })
            .collect()
    }

    async fn load_all_events_since(
        &self,
        after_position: u64,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at, published_at
             FROM events
             WHERE id > $1
             ORDER BY id ASC
             LIMIT $2",
             after_position as i64,
             limit as i64
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: intern(&record.aggregate_domain),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: intern(&record.event_name),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                    published_at: record.published_at,
                })
            })
            .collect()
    }

    async fn position_at_or_after(&self, timestamp: DateTime<Utc>) -> Result<u64, StoreError> {
        // First event at/after the timestamp; if none exists yet (the
        // timestamp is in the future relative to the log), fall back to
        // "current head + 1" so paging from there yields nothing until
        // new events actually arrive.
        let row = sqlx::query!(
            "SELECT COALESCE(
                (SELECT MIN(id) FROM events WHERE created_at >= $1),
                (SELECT COALESCE(MAX(id), 0) + 1 FROM events)
             ) AS position",
            timestamp,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        if let Some(position) = row.position {
            Ok(position as u64)
        } else {
            Err(StoreError::Other(anyhow::format_err!("Not found")))
        }
    }

    async fn load_events_by_domain_since(
        &self,
        aggregate_domain: &str,
        after_position: u64,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at, published_at
             FROM events
             WHERE aggregate_domain = $1 AND id > $2
             ORDER BY id ASC
             LIMIT $3",
             aggregate_domain,
             after_position as i64,
             limit as i64,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: intern(&record.aggregate_domain),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: intern(&record.event_name),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                    published_at: record.published_at,
                })
            })
            .collect()
    }

    async fn load_all_unpublished_events(
        &self,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at
             FROM events
             WHERE published_at IS NULL
             ORDER BY id ASC
             LIMIT $1",
             limit as i64,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: intern(&record.aggregate_domain),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: intern(&record.event_name),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                    published_at: None,
                })
            })
            .collect()
    }

    async fn publish_event(&self, sequence: u64) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE events SET published_at = now()
             WHERE id = $1", // never regress a snapshot
            sequence as i64,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        Ok(())
    }
}
