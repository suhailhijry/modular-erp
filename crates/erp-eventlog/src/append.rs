//! Appending to the log.

use erp_types::{EventName, LogPosition, SchemaVersion, Sequence, StreamId};
use sqlx::PgConnection;

use crate::envelope::{Envelope, Metadata};
use crate::messages;

/// Postgres unique-violation.
const UNIQUE_VIOLATION: &str = "23505";

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    /// Someone else wrote to this stream first.
    ///
    /// The normal outcome of two writers who both loaded the aggregate at the
    /// same version — not a failure, a retry. The caller reloads and reapplies.
    #[error("{stream} was modified concurrently (expected version {expected})")]
    Conflict {
        stream: StreamId,
        expected: Sequence,
    },
    #[error("cannot append an empty batch")]
    Empty,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for AppendError {
    fn message(&self) -> erp_i18n::Message {
        match self {
            // A conflict is retryable and worth saying so — the user changed
            // something someone else had already changed.
            Self::Conflict { .. } => erp_i18n::Message::new(messages::CONCURRENT_MODIFICATION),
            Self::Empty | Self::Database(_) => erp_i18n::Message::new(messages::INTERNAL),
        }
    }
}

/// An event on its way in.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_name: EventName,
    pub schema_version: SchemaVersion,
    pub payload: serde_json::Value,
}

impl NewEvent {
    pub fn new(
        event_name: EventName,
        schema_version: SchemaVersion,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_name,
            schema_version,
            payload,
        }
    }
}

/// Appends a batch to one stream.
///
/// `expected` is the version the caller believes the aggregate is at. The events
/// are written at `expected + 1 ..= expected + n`, and the unique constraint on
/// `(stream, sequence)` rejects the batch if anyone got there first — which is
/// the optimistic-concurrency check, enforced by the database rather than by a
/// read-then-write race.
///
/// # Ordering and locking
///
/// Positions come from the counter row, whose row lock is what makes position
/// order equal commit order (L1). The lock is taken here and released when the
/// caller's transaction ends, so **append as late in the transaction as
/// possible**: everything the caller does afterwards blocks other appends to
/// this tenant.
///
/// Reserving and inserting happen in one statement, so the log's own invariant
/// holds even if a caller forgets to open a transaction. Callers should still
/// use one — the events and their side effects have to commit together (D9) —
/// but the log does not depend on it for correctness.
pub async fn append(
    conn: &mut PgConnection,
    stream: &StreamId,
    expected: Sequence,
    events: &[NewEvent],
    metadata: &Metadata,
) -> Result<Vec<Envelope>, AppendError> {
    if events.is_empty() {
        return Err(AppendError::Empty);
    }

    let names: Vec<String> = events
        .iter()
        .map(|e| e.event_name.as_str().to_owned())
        .collect();
    let versions: Vec<i16> = events
        .iter()
        .map(|e| i16::try_from(e.schema_version.get()).unwrap_or(i16::MAX))
        .collect();
    let payloads: Vec<serde_json::Value> = events.iter().map(|e| e.payload.clone()).collect();
    let metadata_json = serde_json::to_value(metadata).unwrap_or_else(|_| serde_json::json!({}));

    // One statement: reserve a contiguous block of positions and write the
    // events into it. `WITH ORDINALITY` preserves input order, so event i gets
    // position base + i and sequence expected + 1 + i.
    let rows = sqlx::query!(
        r#"
        WITH reserved AS (
            UPDATE event_log_position
               SET next_position = next_position + array_length($4::text[], 1)
             WHERE id
            RETURNING next_position - array_length($4::text[], 1) AS base
        ),
        incoming AS (
            SELECT *
              FROM UNNEST($4::text[], $5::smallint[], $6::jsonb[])
                   WITH ORDINALITY AS t(event_name, schema_version, payload, idx)
        )
        INSERT INTO event (
            position, stream_domain, stream_id, sequence,
            event_name, schema_version, payload, metadata
        )
        SELECT r.base + i.idx - 1,
               $1, $2, $3 + i.idx,
               i.event_name, i.schema_version, i.payload, $7
          FROM incoming i CROSS JOIN reserved r
         ORDER BY i.idx
        RETURNING position, sequence, recorded_at
        "#,
        stream.domain.as_str(),
        stream.id.as_str(),
        expected.get(),
        &names,
        &versions,
        &payloads,
        metadata_json,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            AppendError::Conflict {
                stream: stream.clone(),
                expected,
            }
        }
        _ => AppendError::Database(e),
    })?;

    // Sort by position rather than trusting RETURNING order. Postgres yields
    // rows in insertion order here, but relying on that would be a silent
    // mis-mapping of events to positions if it ever changed.
    let mut assigned: Vec<_> = rows
        .into_iter()
        .map(|r| (r.position, r.sequence, r.recorded_at))
        .collect();
    assigned.sort_unstable_by_key(|(position, _, _)| *position);

    let envelopes = assigned
        .into_iter()
        .zip(events)
        .map(|((position, sequence, recorded_at), event)| {
            Ok(Envelope {
                position: LogPosition::new(position)
                    .map_err(|e| AppendError::Database(sqlx::Error::Protocol(e.to_string())))?,
                stream: stream.clone(),
                sequence: Sequence::new(sequence)
                    .map_err(|e| AppendError::Database(sqlx::Error::Protocol(e.to_string())))?,
                event_name: event.event_name.clone(),
                schema_version: event.schema_version,
                payload: event.payload.clone(),
                metadata: metadata.clone(),
                recorded_at,
            })
        })
        .collect::<Result<Vec<_>, AppendError>>()?;

    Ok(envelopes)
}
