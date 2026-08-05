/*

aggregates:
    - some domain model, and all of its related constraints and invariants
    - whenever an aggregate is retrieved, it does the following:
       1: search for snapshots, if a recent snapshot exists for this aggregate, go to step 4, if not go to step 2
       2: find all events for this aggregate (by aggregate id), sorted by sequence
       3: apply all events in sequence (fill the data from each event), one event at a time, if an error occurs, stop and return, if not go to step 5
       4: get the snapshot payload, and fill the aggregate
       5: return the constructed aggregate
    - firing events is done by calling a function on the aggregate model, if the function call succeeds, we return self, otherwise, we return error
    - if all events succeeded, then we call "persist", which persists all events to the database, and sends the persisted events to the current saga if any, otherwise send it to the event bus (..with a context?)

events:
    - for a given domain model, all of its events are an enum, with each state representing an event for that aggregate

commands:
    - not needed (simply an API endpoint/function call)

saga:
    - same thing for commands, they're simply a function call that does its things with multiple aggregates or even domains, so its irrelevant

context:
    - this is the core of event sourcing. But basically, a context is a queue with helper functions.
    - at the end of a command, the context either drops all events, or persists them in the order they were registered

 */

use std::sync::Arc;

pub use aggregate::{AggregateMeta, DomainEvent};
pub use projector::ProjectorMeta;
pub use spa_macros::{AggregateMeta, DomainEvent, ProjectorMeta};

pub mod aggregate;
pub mod checkpoint_store;
pub mod composite_projector;
pub mod context;
pub mod event_bus;
pub mod event_store;
pub mod kafka_relay_projector;
pub mod pg_event_store;
pub mod pg_notify_event_bus;
pub mod projector;
pub mod projector_admin;
pub mod reactor_runner;

use chrono::{DateTime, Utc};

use crate::event_sourcing::projector::{AlertSink, RetryPolicy};
pub use crate::event_sourcing::{
    aggregate::Aggregate,
    checkpoint_store::CheckpointStore,
    composite_projector::CompositeEventBus,
    context::{Context, ContextError},
    event_bus::EventBus,
    event_store::{EventEnvelope, EventStore, SnapshotEnvelope, StoreError},
    projector::{Projector, ReplayScope},
};

pub async fn load_aggregate<A: Aggregate>(
    store: &dyn EventStore,
    id: &str,
) -> Result<A, StoreError> {
    let mut aggregate = if let Some(snap) = store.load_snapshot(A::domain_name(), id).await? {
        serde_json::from_value::<A>(snap.payload).map_err(|e| StoreError::Other(e.into()))?
    } else {
        A::default()
    };

    let envelopes = if aggregate.version() > 0 {
        store
            .load_events_for_aggregate_since(A::domain_name(), id, aggregate.version())
            .await?
    } else {
        store.load_events(A::domain_name(), id).await?
    };

    for envelope in envelopes {
        let event: A::Event =
            serde_json::from_value(envelope.payload).map_err(|e| StoreError::Other(e.into()))?;
        aggregate.apply(&event);
    }

    Ok(aggregate)
}

pub async fn maybe_snapshot<A>(
    store: &dyn EventStore,
    aggregate: &A,
    every: u64,
) -> Result<(), StoreError>
where
    A: Aggregate,
{
    if aggregate.version() % every == 0 {
        store
            .save_snapshot(SnapshotEnvelope {
                aggregate_domain: A::domain_name().to_string(),
                aggregate_id: aggregate.id().to_string(),
                version: aggregate.version(),
                payload: serde_json::to_value(aggregate)
                    .map_err(|e| StoreError::Other(e.into()))?,
                created_at: Utc::now(),
            })
            .await?;
    }

    Ok(())
}

pub async fn handle_command<A: Aggregate>(
    store: &dyn EventStore,
    bus: Option<Arc<dyn EventBus>>,
    id: &str,
    command: A::Command,
    metadata: serde_json::Value,
) -> anyhow::Result<A>
where
    A::Error: Into<anyhow::Error>,
{
    const MAX_RETRIES: u32 = 3;
    const SNAPSHOT_EVERY: u64 = 1000;

    for attempt in 0..MAX_RETRIES {
        let mut aggregate = load_aggregate::<A>(store, id).await?;
        let version = aggregate.version() + 1;

        let events = aggregate.handle(command.clone()).map_err(Into::into)?;

        for event in &events {
            aggregate.apply(event);
        }

        let mut ctx = Context::with_metadata(metadata.clone());
        ctx.queue_events::<A>(id, version, events);

        match ctx.commit(store, bus.clone()).await {
            Ok(_) => {
                if let Err(e) = maybe_snapshot(store, &aggregate, SNAPSHOT_EVERY).await {
                    tracing::warn!(error = %e, aggregate_id = id, "snapshot failed (non-fatal)");
                }
                return Ok(aggregate);
            }
            Err(ContextError::Store(StoreError::Conflict { .. })) if attempt + 1 < MAX_RETRIES => {
                continue;
            }
            Err(ContextError::DispatchFailed(e)) => {
                tracing::error!(error = %e, aggregate_id = id, "command persisted but in-process dispatch failed - replay-driven consumers unaffected");
                return Ok(aggregate);
            }
            Err(e) => return Err(e.into()),
        }
    }

    unreachable!("loop always returns or errors")
}

pub async fn replay(
    store: &dyn EventStore,
    checkpoints: &dyn CheckpointStore,
    projector: &dyn Projector,
    scope: ReplayScope<'_>,
    start_at: Option<DateTime<Utc>>,
    batch_size: u64,
) -> anyhow::Result<()> {
    let mut position = match start_at {
        Some(t) => store.position_at_or_after(t).await?,
        None => checkpoints.load(projector.name()).await?.unwrap_or(0),
    };

    loop {
        let batch = match &scope {
            ReplayScope::Everything => store.load_all_events_since(position, batch_size).await?,
            ReplayScope::Domain(domain) => {
                store
                    .load_events_by_domain_since(domain, position, batch_size)
                    .await?
            }
            ReplayScope::Aggregate { domain, id } => {
                store
                    .load_events_for_aggregate_since(domain, id, position)
                    .await?
            }
        };

        if batch.is_empty() {
            break;
        }

        for envelope in &batch {
            projector.handle(envelope).await?;
            position = match &scope {
                ReplayScope::Aggregate { .. } => envelope.sequence,
                _ => envelope.id,
            };
        }

        checkpoints.save(projector.name(), position).await?;
    }

    Ok(())
}

pub async fn replay_with_dead_letter(
    store: &dyn EventStore,
    checkpoints: &dyn CheckpointStore,
    projector: &dyn Projector,
    scope: ReplayScope<'_>,
    batch_size: u64,
    pool: &sqlx::PgPool,
    alerts: &dyn AlertSink,
    policy: &RetryPolicy,
) -> anyhow::Result<()> {
    use crate::event_sourcing::projector::{HandleOutcome, handle_with_dead_letter};

    let mut position = checkpoints.load(projector.name()).await?.unwrap_or(0);

    loop {
        let batch = match &scope {
            ReplayScope::Everything => store.load_all_events_since(position, batch_size).await?,
            ReplayScope::Domain(t) => {
                store
                    .load_events_by_domain_since(t, position, batch_size)
                    .await?
            }
            ReplayScope::Aggregate { domain, id } => {
                store
                    .load_events_for_aggregate_since(domain, id, position)
                    .await?
            }
        };

        if batch.is_empty() {
            break;
        }

        let mut advanced = false;
        for envelope in &batch {
            match handle_with_dead_letter(projector, envelope, pool, alerts, policy).await? {
                HandleOutcome::Processed | HandleOutcome::DeadLettered => {
                    position = match &scope {
                        ReplayScope::Aggregate { .. } => envelope.sequence,
                        _ => envelope.id,
                    };
                    advanced = true;
                }
                HandleOutcome::WillRetry { .. } => {
                    // Persist progress up to the last good event, then
                    // yield - the caller's next tick re-attempts.
                    if advanced {
                        checkpoints.save(projector.name(), position).await?;
                    }
                    return Ok(());
                }
            }
        }
        checkpoints.save(projector.name(), position).await?;
    }
    Ok(())
}
