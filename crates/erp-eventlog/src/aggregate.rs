//! The write model: rebuilding an aggregate from its events, and appending new
//! ones under optimistic concurrency.
//!
//! Nothing here is domain-specific — architecture decision D11 keeps business
//! domain out of the framework. These are the mechanics every module's
//! aggregates are built on.

use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::{PgConnection, PgPool};

use crate::append::{AppendError, NewEvent, append};
use crate::envelope::{Envelope, Metadata};
use crate::outbox::{Effect, EnqueueError, enqueue};
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
    Enqueue(#[from] EnqueueError),
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

impl<E> ExecuteError<E> {
    /// Whether retrying the whole command is worth doing.
    ///
    /// Only a lost optimistic-concurrency race. A rejection is the aggregate's
    /// rules saying no, and re-asking gets the same answer.
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Append(AppendError::Conflict { .. }))
    }
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

/// What a command decided: facts to record, and promises to keep.
///
/// Both halves of D9 in one value. A decision function returns this instead of
/// performing I/O, which is what makes a command handler testable without a
/// network and replayable without side effects.
///
/// ```ignore
/// |loaded| Ok(Decision::one(Opened { .. }))
///
/// |loaded| Ok(Decision::one(Posted { .. })
///             .with_effect(Effect::new(kind("email.send"), payload)))
///
/// |loaded| Ok(Decision::nothing())
/// ```
///
/// [`execute`] takes this type directly rather than anything convertible into
/// it. An earlier version accepted `impl Into<Decision<_>>` so a command with no
/// effects could return a bare `Vec`; that made the return type unnameable in a
/// closure with no `Ok` branch — a rejection-only command handler — and the
/// resulting inference error pointed at `Result`, not at the real problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision<E> {
    pub events: Vec<E>,
    pub effects: Vec<Effect>,
}

impl<E> Default for Decision<E> {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            effects: Vec::new(),
        }
    }
}

impl<E> Decision<E> {
    /// The command looked, and there is nothing to do.
    ///
    /// A success, not a failure: re-issuing a command that has already taken
    /// effect should be quiet, not an error.
    #[must_use]
    pub fn nothing() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn record(events: Vec<E>) -> Self {
        Self {
            events,
            effects: Vec::new(),
        }
    }

    #[must_use]
    pub fn one(event: E) -> Self {
        Self::record(vec![event])
    }

    #[must_use]
    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.effects.push(effect);
        self
    }

    #[must_use]
    pub fn with_effects(mut self, effects: impl IntoIterator<Item = Effect>) -> Self {
        self.effects.extend(effects);
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.effects.is_empty()
    }
}

impl<E> From<Vec<E>> for Decision<E> {
    fn from(events: Vec<E>) -> Self {
        Self::record(events)
    }
}

/// What a command actually committed.
///
/// `version` is the aggregate's version afterwards, which is what an `ETag`
/// carries and what the next `If-Match` is checked against — so it is returned
/// even when nothing happened, because "nothing happened, and here is where you
/// are" is a useful answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed<E> {
    pub events: Vec<E>,
    /// Position of the first event written. `None` when none were.
    pub at: Option<LogPosition>,
    /// The aggregate's version after this command.
    pub version: Sequence,
    /// Effects newly recorded. Lower than the number promised when a pinned
    /// idempotency key was already present — which is the deduplication working.
    pub effects_enqueued: usize,
}

impl<E> Committed<E> {
    /// Whether the command was a no-op.
    #[must_use]
    pub fn did_nothing(&self) -> bool {
        self.events.is_empty() && self.effects_enqueued == 0
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

/// Loads, decides, appends, and records the decision's effects — retrying if
/// someone else got there first.
///
/// `decide` must be a **pure function of the aggregate's state**. It runs again
/// on every retry, so a decision that reads a clock, generates an id, or writes
/// anywhere would produce different results on the second attempt — and the one
/// that got committed would not be the one that was checked.
///
/// Retrying is safe precisely because of that purity: a conflict means the
/// aggregate moved on, so the decision is remade against the state that actually
/// won rather than being forced through. Effects are remade with it, so the
/// abandoned attempt's promises roll back along with its events.
///
/// # Atomicity (D9)
///
/// Events and effects are written in **one transaction**. That is the whole
/// mechanism: after commit, either the facts and the promises are both durable
/// or neither is, so there is no ordering in which a customer is emailed about
/// something that did not happen, and none in which something happens with the
/// promise lost.
///
/// A caller needing to write something *else* atomically — a read model, a
/// module's own table — still owns its transaction and uses [`load`],
/// [`append_events`] and [`enqueue`](crate::enqueue) directly.
pub async fn execute<A, F, E>(
    pool: &PgPool,
    id: &AggregateId,
    upcasters: &Upcasters,
    metadata: &Metadata,
    decide: F,
) -> Result<Committed<A::Event>, ExecuteError<E>>
where
    A: Aggregate,
    F: Fn(&Loaded<A>) -> Result<Decision<A::Event>, E>,
{
    for attempt in 1..=MAX_ATTEMPTS {
        let mut tx = pool.begin().await?;
        match try_execute::<A, _, E>(&mut tx, id, upcasters, metadata, &decide).await {
            Ok(committed) => {
                tx.commit().await?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await?;
                tracing::debug!(attempt, "optimistic concurrency conflict, retrying");
            }
            Err(e) => {
                tx.rollback().await?;
                return Err(e);
            }
        }
    }

    Err(ExecuteError::Contended {
        stream: StreamId::new(A::domain(), id.clone()),
        attempts: MAX_ATTEMPTS,
    })
}

/// Enough to clear ordinary contention, few enough that a genuinely hot
/// aggregate surfaces as a retryable error instead of a hung request.
pub const MAX_ATTEMPTS: u32 = 5;

/// One attempt at [`execute`], inside the caller's transaction.
///
/// # Who retries
///
/// Not this function. It reports a conflict and leaves the caller to roll back
/// and try again, because **the transaction has to come from wherever the
/// connection budget is** — `TenantDb::begin` for anything serving a tenant.
/// `TenantDb::execute` is the loop; this is one turn of it.
///
/// # The caller's obligation
///
/// Commit on `Ok`, roll back on `Err`. Same shape and same failure mode as
/// `run_once_in`: forgetting to commit loses the command, and there is no
/// ordering in which part of it survives.
pub async fn try_execute<A, F, E>(
    conn: &mut PgConnection,
    id: &AggregateId,
    upcasters: &Upcasters,
    metadata: &Metadata,
    decide: F,
) -> Result<Committed<A::Event>, ExecuteError<E>>
where
    A: Aggregate,
    F: Fn(&Loaded<A>) -> Result<Decision<A::Event>, E>,
{
    let loaded = load::<A>(&mut *conn, id, upcasters).await?;
    let decision = decide(&loaded).map_err(ExecuteError::Rejected)?;

    // A decision to do nothing is a success, not an empty append.
    if decision.is_empty() {
        return Ok(Committed {
            events: Vec::new(),
            at: None,
            version: loaded.version,
            effects_enqueued: 0,
        });
    }

    let envelopes = if decision.events.is_empty() {
        Vec::new()
    } else {
        append_events::<A>(&mut *conn, id, loaded.version, &decision.events, metadata).await?
    };

    let at = envelopes.first().map(|e| e.position);
    let version = envelopes.last().map_or(loaded.version, |e| e.sequence);

    // Same transaction as the append above. A failure here — an unkeyed effect
    // from a command that appended nothing — takes the events with it, which is
    // right: the command asked for something the outbox cannot promise, so none
    // of it happened.
    let effects_enqueued = enqueue(&mut *conn, at, &decision.effects).await?;

    Ok(Committed {
        events: decision.events,
        at,
        version,
        effects_enqueued,
    })
}

/// Appends typed events to an aggregate's stream.
///
/// The typed counterpart to [`append`](crate::append), for callers who own their
/// own transaction because they are writing something else alongside. Returns
/// the stored envelopes, which is how the caller learns the positions its
/// effects are keyed on.
pub async fn append_events<A: Aggregate>(
    conn: &mut PgConnection,
    id: &AggregateId,
    expected: Sequence,
    events: &[A::Event],
    metadata: &Metadata,
) -> Result<Vec<Envelope>, AppendError> {
    if events.is_empty() {
        return Ok(Vec::new());
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

    append(conn, &stream, expected, &encoded, metadata).await
}
