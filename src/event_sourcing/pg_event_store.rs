use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::event_sourcing::{
    CheckpointStore, EventEnvelope, EventStore, SnapshotEnvelope, StoreError,
};

const UNIQUE_VIOLATION: &str = "23505";

#[derive(Clone)]
pub struct PgEventStore {
    write_pool: sqlx::PgPool,
    read_pool: sqlx::PgPool,
}

impl PgEventStore {
    pub fn new(write_pool: sqlx::PgPool, read_pool: sqlx::PgPool) -> Self {
        Self {
            write_pool,
            read_pool,
        }
    }
}

#[async_trait]
impl EventStore for PgEventStore {
    async fn load_events(
        &self,
        aggregate_domain: &str,
        aggregate_id: &str,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        const PAGE: i64 = 1000;

        let mut all = Vec::new();
        let mut after_seq: i64 = 0;
        loop {
            let rows = sqlx::query!(
                "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at
                 FROM events
                 WHERE aggregate_domain = $1 AND aggregate_id = $2 AND sequence > $3
                 ORDER BY sequence ASC
                 LIMIT $4",
                 aggregate_domain,
                 aggregate_id,
                 after_seq,
                 PAGE,
            )
            .fetch_all(&self.write_pool)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

            let page_len = rows.len();

            for record in &rows {
                let envelope = EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: record.aggregate_domain.to_string(),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: record.event_name.to_string(),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                };
                after_seq = envelope.sequence as i64;
                all.push(envelope);
            }

            if page_len < PAGE as usize {
                break;
            }
        }
        Ok(all)
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
            .fetch_optional(&self.write_pool)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        let Some(row) = row else { return Ok(None) };
        Ok(Some(SnapshotEnvelope {
            aggregate_domain: row.aggregate_domain,
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

        // Single transaction AND single statement: one multi-row insert
        // via UNNEST instead of one round-trip per event. RETURNING is
        // ordered by the input arrays (Postgres UNNEST preserves element
        // order and INSERT...RETURNING yields rows in insertion order
        // for a plain VALUES/UNNEST source), so positions map back to
        // envelopes by index.
        let mut tx = self
            .write_pool
            .begin()
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        let domains: Vec<String> = events
            .iter()
            .map(|e| e.aggregate_domain.to_string())
            .collect();
        let ids: Vec<String> = events.iter().map(|e| e.aggregate_id.to_string()).collect();
        let seqs: Vec<i64> = events.iter().map(|e| e.sequence as i64).collect();
        let names: Vec<String> = events.iter().map(|e| e.event_name.to_string()).collect();
        let payloads: Vec<serde_json::Value> = events.iter().map(|e| e.payload.clone()).collect();
        let metadatas: Vec<serde_json::Value> = events.iter().map(|e| e.metadata.clone()).collect();
        let created: Vec<chrono::DateTime<chrono::Utc>> =
            events.iter().map(|e| e.created_at).collect();

        let result = sqlx::query!("
            INSERT INTO events (aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at)
                         SELECT * FROM UNNEST($1::text[], $2::text[], $3::bigint[], $4::text[], $5::jsonb[], $6::jsonb[], $7::timestamptz[])
                         RETURNING id
            ", &domains, &ids, &seqs, &names, &payloads, &metadatas, &created,).fetch_all(&mut *tx).await;

        match result {
            Ok(rows) => {
                for (event, row) in events.iter_mut().zip(rows.iter()) {
                    let position: i64 = row.id;
                    event.id = position as u64;
                }
            }
            Err(sqlx::Error::Database(db_err))
                if db_err.code().as_deref() == Some(UNIQUE_VIOLATION) =>
            {
                let first = &events[0];
                return Err(StoreError::Conflict {
                    aggregate_domain: first.aggregate_domain.clone(),
                    aggregate_id: first.aggregate_id.clone(),
                    sequence: first.sequence,
                });
            }
            Err(e) => return Err(StoreError::Other(e.into())),
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
        .execute(&self.write_pool)
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
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at
             FROM events
             WHERE aggregate_domain = $1 AND aggregate_id = $2 AND sequence > $3
             ORDER BY sequence ASC",
             aggregate_domain,
             aggregate_id,
             after_sequence as i64,
        )
        .fetch_all(&self.write_pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: record.aggregate_domain.to_string(),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: record.event_name.to_string(),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
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
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at
             FROM events
             WHERE id > $1
             ORDER BY id ASC
             LIMIT $2",
             after_position as i64,
             limit as i64
        )
        .fetch_all(&self.read_pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: record.aggregate_domain.to_string(),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: record.event_name.to_string(),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
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
        .fetch_one(&self.read_pool)
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
            "SELECT id, aggregate_domain, aggregate_id, sequence, event_name, payload, metadata, created_at
             FROM events
             WHERE aggregate_domain = $1 AND id > $2
             ORDER BY id ASC
             LIMIT $3",
             aggregate_domain,
             after_position as i64,
             limit as i64,
        )
        .fetch_all(&self.read_pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        rows.iter()
            .map(|record| {
                Ok(EventEnvelope {
                    id: record.id as u64,
                    aggregate_domain: record.aggregate_domain.to_string(),
                    aggregate_id: record.aggregate_id.to_string(),
                    event_name: record.event_name.to_string(),
                    sequence: record.sequence as u64,
                    payload: record.payload.clone(),
                    metadata: record.metadata.clone(),
                    created_at: record.created_at,
                })
            })
            .collect()
    }
}

#[async_trait]
impl CheckpointStore for PgEventStore {
    async fn load(&self, projector: &str) -> anyhow::Result<Option<u64>> {
        let row = sqlx::query!(
            "SELECT global_position FROM projector_checkpoints WHERE projector = $1",
            projector,
        )
        .fetch_optional(&self.write_pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;

        Ok(row.map(|v| v.global_position as u64))
    }

    async fn save(&self, projector: &str, position: u64) -> anyhow::Result<()> {
        sqlx::query!(
            "INSERT INTO projector_checkpoints (projector, global_position)
             VALUES ($1, $2)
             ON CONFLICT (projector)
             DO UPDATE SET global_position = EXCLUDED.global_position
             WHERE projector_checkpoints.global_position < EXCLUDED.global_position", // never regress a snapshot
            projector,
            position as i64,
        )
        .execute(&self.write_pool)
        .await
        .map_err(|e| StoreError::Other(e.into()))?;
        Ok(())
    }
}
