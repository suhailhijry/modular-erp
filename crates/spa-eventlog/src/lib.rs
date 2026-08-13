//! The tenant event log.
//!
//! One log per tenant database, so nothing here carries a tenant id — the
//! database is the tenant.
//!
//! # The invariant everything else rests on (L1)
//!
//! **Positions are gapless and their order is commit order.**
//!
//! That is stronger than it sounds, and the obvious implementation does not
//! provide it. See `migrations/tenant/0001_event_log.sql` for why a sequence
//! silently loses events under contention, and why a counter row does not.
//!
//! What the property buys: a tailer reading `position > checkpoint ORDER BY
//! position` always sees an unbroken prefix of the log. It can never observe
//! position 101 before 100 and advance past an event that had not committed yet.
//! Every projection, every replay, and the whole reproducibility argument (L2,
//! L5) depends on this and nothing else.
//!
//! `tests/append.rs` proves it under concurrency rather than asserting it.

mod aggregate;
mod append;
mod envelope;
pub mod messages;
mod outbox;
mod read;
mod upcast;

pub use aggregate::{
    Aggregate, Committed, Decision, DomainEvent, ExecuteError, LoadError, Loaded, MAX_ATTEMPTS,
    append_events, execute, load, load_since, try_execute,
};
pub use append::{AppendError, NewEvent, append};
pub use envelope::{Envelope, Metadata};
pub use outbox::{
    DeliveryError, DispatchError, Dispatched, Dispatcher, Effect, EffectHandler, EnqueueError,
    OutboxHealth, PendingEffect, RetryPolicy, Settlement, enqueue, outbox_health,
};
pub use read::{Integrity, ReadError, integrity, read_since, read_stream, read_stream_since};
pub use upcast::{UpcastError, UpcastStep, Upcasters};

use spa_i18n::StaticCatalog;

/// Migrations for a tenant database.
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations/tenant");

/// This crate's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);
