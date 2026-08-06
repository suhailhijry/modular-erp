//! The write model: rebuilding an aggregate from its events, and appending new
//! ones under optimistic concurrency.
//!
//! Nothing here is domain-specific — architecture decision D11 keeps business
//! domain out of the framework. These are the mechanics every module's
//! aggregates are built on.

use serde::Serialize;
use serde::de::DeserializeOwned;
use spa_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence, StreamId};
use sqlx::{PgConnection, PgPool};

use crate::append::{AppendError, NewEvent, append};
use crate::envelope::Metadata;
use crate::read::{ReadError, read_stream_since};
use crate::upcast::{UpcastError, Upcasters};

/// Something that happened. The unit stored in the log.
pub trait DomainEvent: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// The stable wire name — `journal_entry.posted`.
    ///
    /// **Part of the API contract and of the stored data.** Renaming one orphans
    /// every event already written under the old name, so it is a migration,
    /// not a refactor.
    fn event_name(&self) -> EventName;

    /// The version this build writes.
    ///
    /// Must match what the [`Upcasters`] registry declares for this name, or
    /// events are written that the reader will not recognise.
    fn schema_version(&self) -> SchemaVersion;
}

/// A consistency boundary: state rebuilt from events, and the rules that decide
/// what may happen next.
pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: DomainEvent;

    /// The stream domain this aggregate's events live under.
    fn domain() -> DomainName;

    /// Folds one event into the state.
    ///
    /// **Must be total and must not fail.** By the time an event reaches this
    /// method it is already history — refusing it would mean the aggregate can
    /// no longer be loaded at all. Validation belongs in the decision function,
    /// before the event exists.
    fn apply(&mut self, event: &Self::Event);
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error(transparent)]
    Upcast(#[from] UpcastError),
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError<E> {
    /// The decision function refused. The aggregate's own rules said no.
    #[error("{0}")]
    Rejected(E),
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// Optimistic concurrency lost repeatedly.
    ///
    /// Distinct from a single [`AppendError::Conflict`], which is retried
    /// silently. Reaching this means genuine sustained contention on one
    /// aggregate, and the caller should surface it rather than retry forever.
    #[error("gave up after {attempts} attempts; {stream} is under sustained contention")]
    Contended { stream: StreamId, attempts: u32 },
}

/// An aggregate and the version it was loaded at.
///
/// The version is what the next append is checked against, which is why they
/// travel together — passing an aggregate around without its version is how an
/// optimistic-concurrency check gets made against the wrong number.
#[derive(Debug, Clone)]
pub struct Loaded<A> {
    pub aggregate: A,
    pub version: Sequence,
}

impl<A> Loaded<A> {
    /// Whether this aggregate has any history. A version of zero means nothing
    /// has been written — which is how "does it exist?" is answered, since the
    /// log has no separate notion of creation.
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.version == Sequence::ZERO
    }
}

/// Rebuilds an aggregate from its stream.
pub async fn load<A: Aggregate>(
    conn: &mut PgConnection,
    id: &AggregateId,
    upcasters: &Upcasters,
) -> Result<Loaded<A>, LoadError> {
    load_since(conn, id, upcasters, A::default(), Sequence::ZERO).await
}

/// Rebuilds from a known starting point — the shape a snapshot would use.
///
/// Snapshots themselves are deliberately absent: they are an optimization for
/// aggregates with long histories, and which of those exist is not yet known.
/// Adding them later is additive, and this signature is where they attach.
pub async fn load_since<A: Aggregate>(
    conn: &mut PgConnection,
    id: &AggregateId,
    upcasters: &Upcasters,
    mut aggregate: A,
    from: Sequence,
) -> Result<Loaded<A>, LoadError> {
    let stream = StreamId::new(A::domain(), id.clone());
    let stored = read_stream_since(conn, &stream, from).await?;

    let mut version = from;
    for envelope in stored {
        let event: A::Event = upcasters.decode(
            &envelope.event_name,
            envelope.schema_version,
            envelope.payload,
        )?;
        aggregate.apply(&event);
        version = envelope.sequence;
    }

    Ok(Loaded { aggregate, version })
}

/// Loads, decides, and appends — retrying if someone else got there first.
///
/// `decide` must be a **pure function of the aggregate's state**. It runs again
/// on every retry, so a decision that reads a clock, generates an id, or writes
/// anywhere would produce different results on the second attempt — and the one
/// that got committed would not be the one that was checked.
///
/// Retrying is safe precisely because of that purity: a conflict means the
/// aggregate moved on, so the decision is remade against the state that actually
/// won rather than being forced through.
///
/// This owns its transaction. Commands that need to write *anything else*
/// atomically with their events — an outbox row, a read-model update — need the
/// caller's transaction instead, and use [`load`] plus
/// [`append_events`] directly.
pub async fn execute<A, F, E>(
    pool: &PgPool,
    id: &AggregateId,
    upcasters: &Upcasters,
    metadata: &Metadata,
    decide: F,
) -> Result<Vec<A::Event>, ExecuteError<E>>
where
    A: Aggregate,
    F: Fn(&Loaded<A>) -> Result<Vec<A::Event>, E>,
{
    /// Enough to clear ordinary contention, few enough that a genuinely hot
    /// aggregate surfaces as a retryable error instead of a hung request.
    const MAX_ATTEMPTS: u32 = 5;

    let stream = StreamId::new(A::domain(), id.clone());

    for attempt in 1..=MAX_ATTEMPTS {
        let mut tx = pool.begin().await?;

        let loaded = load::<A>(&mut tx, id, upcasters).await?;
        let events = decide(&loaded).map_err(ExecuteError::Rejected)?;

        // A decision to do nothing is a success, not an empty append.
        if events.is_empty() {
            tx.rollback().await?;
            return Ok(events);
        }

        match append_events::<A>(&mut tx, id, loaded.version, &events, metadata).await {
            Ok(()) => {
                tx.commit().await?;
                return Ok(events);
            }
            Err(AppendError::Conflict { .. }) => {
                // Someone else wrote first. Roll back and decide again against
                // the state that won.
                tx.rollback().await?;
                tracing::debug!(
                    stream = %stream,
                    attempt,
                    "optimistic concurrency conflict, retrying"
                );
            }
            Err(e) => {
                tx.rollback().await?;
                return Err(e.into());
            }
        }
    }

    Err(ExecuteError::Contended {
        stream,
        attempts: MAX_ATTEMPTS,
    })
}

/// Appends typed events to an aggregate's stream.
///
/// The typed counterpart to [`append`](crate::append), for callers who own their
/// own transaction because they are writing something else alongside.
pub async fn append_events<A: Aggregate>(
    conn: &mut PgConnection,
    id: &AggregateId,
    expected: Sequence,
    events: &[A::Event],
    metadata: &Metadata,
) -> Result<(), AppendError> {
    if events.is_empty() {
        return Ok(());
    }

    let stream = StreamId::new(A::domain(), id.clone());
    let encoded: Vec<NewEvent> = events
        .iter()
        .map(|event| {
            NewEvent::new(
                event.event_name(),
                event.schema_version(),
                // An event that cannot be serialized is a programming error in
                // the event type itself, not a runtime condition. Recording
                // `null` would silently store an unreadable event, so store a
                // marker the decoder is guaranteed to reject instead.
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();

    append(conn, &stream, expected, &encoded, metadata).await?;
    Ok(())
}
