//! Reading the log.

use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId,
};
use sqlx::PgConnection;

use crate::envelope::Envelope;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// A stored row does not satisfy the invariants its types promise.
    ///
    /// Never guessed past. A row this build cannot interpret means data from a
    /// newer version, or corruption — and both are reasons to stop rather than
    /// to project something plausible (L6).
    #[error("stored event at position {position} is invalid: {reason}")]
    Corrupt { position: i64, reason: String },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// One page of the log, in position order.
///
/// This is the tailer's read. Because positions are gapless and commit-ordered
/// (L1), the result is always an unbroken prefix of everything committed — a
/// caller can advance its checkpoint to the last position it sees without ever
/// stepping over an event that had not committed yet.
pub async fn read_since(
    conn: &mut PgConnection,
    after: LogPosition,
    limit: i64,
) -> Result<Vec<Envelope>, ReadError> {
    let rows = sqlx::query!(
        r#"SELECT position, stream_domain, stream_id, sequence, event_name,
                  schema_version, payload, metadata, recorded_at
             FROM event
            WHERE position > $1
            ORDER BY position
            LIMIT $2"#,
        after.get(),
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            envelope(
                r.position,
                r.stream_domain,
                r.stream_id,
                r.sequence,
                r.event_name,
                r.schema_version,
                r.payload,
                r.metadata,
                r.recorded_at,
            )
        })
        .collect()
}

/// Every event for one aggregate, in order. The read that rebuilds it.
pub async fn read_stream(
    conn: &mut PgConnection,
    stream: &StreamId,
) -> Result<Vec<Envelope>, ReadError> {
    read_stream_since(conn, stream, Sequence::ZERO).await
}

/// Events for one aggregate after a version — the read that resumes from a
/// snapshot.
pub async fn read_stream_since(
    conn: &mut PgConnection,
    stream: &StreamId,
    after: Sequence,
) -> Result<Vec<Envelope>, ReadError> {
    let rows = sqlx::query!(
        r#"SELECT position, stream_domain, stream_id, sequence, event_name,
                  schema_version, payload, metadata, recorded_at
             FROM event
            WHERE stream_domain = $1 AND stream_id = $2 AND sequence > $3
            ORDER BY sequence"#,
        stream.domain.as_str(),
        stream.id.as_str(),
        after.get(),
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            envelope(
                r.position,
                r.stream_domain,
                r.stream_id,
                r.sequence,
                r.event_name,
                r.schema_version,
                r.payload,
                r.metadata,
                r.recorded_at,
            )
        })
        .collect()
}

/// What the log's own integrity check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub event_count: i64,
    pub highest_position: i64,
    /// The counter's next value. Must be `highest_position + 1`.
    pub next_position: i64,
}

impl Integrity {
    /// Whether positions are contiguous from 1.
    ///
    /// True iff `event_count == highest_position` and the counter agrees. Any
    /// deviation means a position was burned, a row was deleted despite the
    /// append-only trigger, or the counter was tampered with — all of which
    /// break the guarantee replay depends on.
    #[must_use]
    pub const fn is_contiguous(&self) -> bool {
        self.event_count == self.highest_position && self.next_position == self.highest_position + 1
    }
}

/// Checks L1 against the stored data.
///
/// Cheap enough to run continuously per tenant (architecture §7), and it is the
/// alarm that says replay can no longer be trusted.
pub async fn integrity(conn: &mut PgConnection) -> Result<Integrity, ReadError> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM event)                     AS "event_count!",
                  (SELECT COALESCE(max(position), 0) FROM event)   AS "highest_position!",
                  (SELECT next_position FROM event_log_position)   AS "next_position!""#
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(Integrity {
        event_count: row.event_count,
        highest_position: row.highest_position,
        next_position: row.next_position,
    })
}

// ---------------------------------------------------------------------------

/// Rebuilds an [`Envelope`], validating everything the types promise.
///
/// Values go through their validating constructors rather than being trusted,
/// because this is where data written by *older versions of this system*
/// arrives — the one place where "it was valid when we wrote it" is not a
/// guarantee about now.
#[expect(clippy::too_many_arguments, reason = "one per stored column")]
fn envelope(
    position: i64,
    stream_domain: String,
    stream_id: String,
    sequence: i64,
    event_name: String,
    schema_version: i16,
    payload: serde_json::Value,
    metadata: serde_json::Value,
    recorded_at: erp_types::Timestamp,
) -> Result<Envelope, ReadError> {
    let corrupt = |reason: String| ReadError::Corrupt { position, reason };

    Ok(Envelope {
        position: LogPosition::new(position).map_err(|e| corrupt(e.to_string()))?,
        stream: StreamId::new(
            DomainName::new(stream_domain).map_err(|e| corrupt(e.to_string()))?,
            AggregateId::new(stream_id).map_err(|e| corrupt(e.to_string()))?,
        ),
        sequence: Sequence::new(sequence).map_err(|e| corrupt(e.to_string()))?,
        event_name: EventName::new(event_name).map_err(|e| corrupt(e.to_string()))?,
        schema_version: SchemaVersion::new(i64::from(schema_version))
            .map_err(|e| corrupt(e.to_string()))?,
        payload,
        metadata: serde_json::from_value(metadata).map_err(|e| corrupt(e.to_string()))?,
        recorded_at,
    })
}
