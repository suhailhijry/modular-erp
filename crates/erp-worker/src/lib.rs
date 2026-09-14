//! The background worker.
//!
//! Everything the system does that no request asked for: advancing projections,
//! delivering what the outbox owes, and — later — migrations, reapers and
//! provisioning workflows.
//!
//! # Three problems, three mechanisms
//!
//! | problem | mechanism |
//! |---|---|
//! | Which worker looks at which tenant | a per-visit lease claimed with `FOR UPDATE SKIP LOCKED` |
//! | Not burning connections on idle tenants | `next_visit_at`, pushed out by a visit that found nothing |
//! | Stopping without losing work | [`CancellationToken`](tokio_util::sync::CancellationToken) checked between ticks, then [`TaskTracker`](tokio_util::task::TaskTracker) drain |
//!
//! # The shutdown property, stated precisely
//!
//! On SIGTERM, a batch that has started **commits**, and no batch starts after.
//! Then the worker waits for in-flight visits and releases its leases.
//!
//! The reason to let the batch finish rather than abandoning it is not safety —
//! abandoning is safe, because an unfinished transaction rolls back and the
//! checkpoint stays exactly where it was. It is that abandoning throws away work
//! that was about to commit, on every deploy, for every tenant, forever.
//!
//! `tests/shutdown.rs` proves the result is indistinguishable from never having
//! been interrupted, by rebuilding the projection from the log and diffing.

mod health;
mod job;
mod jobs;
pub mod mail;
mod retention;
mod worker;

pub use health::{Finding, HealthJob, Invariant};
pub use job::{Activity, BoxError, Job, PlatformJob};
pub use jobs::{OutboxJob, PlatformOutboxJob, ProjectionJob, Signals};
pub use retention::{
    DELIVERED_EFFECTS, OCCUPANCY_CLAIMS, Retention, SHORT_LINKS, SweepSessions, WEBHOOK_EVENTS,
};
pub use worker::{Shutdown, Worker, WorkerConfig};

/// The signal this and the API both drain on. It lived here until 2026-09-14,
/// when the API turned out to be listening for Ctrl-C alone.
pub use erp_control::shutdown_signal;
