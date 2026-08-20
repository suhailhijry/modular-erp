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
mod worker;

pub use health::{Finding, HealthJob, Invariant};
pub use job::{Activity, BoxError, Job, PlatformJob};
pub use jobs::{OutboxJob, PlatformOutboxJob, ProjectionJob};
pub use worker::{Shutdown, Worker, WorkerConfig};

use tokio_util::sync::CancellationToken;

/// A token cancelled by SIGTERM or SIGINT.
///
/// SIGTERM is what an orchestrator sends; SIGINT is Ctrl-C. Both mean the same
/// thing here, and treating them differently is how a local run behaves unlike
/// production.
///
/// A **second** signal aborts immediately. An operator pressing Ctrl-C twice
/// means it, and a drain that will not finish must not be the only way out.
#[must_use]
pub fn shutdown_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let listener = token.clone();

    tokio::spawn(async move {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::error!(error = %e, "could not install the SIGTERM handler");
                    return;
                }
            };

        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received; draining"),
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => tracing::info!("interrupt received; draining"),
                Err(e) => {
                    tracing::error!(error = %e, "could not listen for interrupts");
                    return;
                }
            },
        }
        listener.cancel();

        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tracing::warn!("second signal; exiting without finishing the drain");
        std::process::exit(130);
    });

    token
}
