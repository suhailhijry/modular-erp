//! The worker process.
//!
//! # What it is not yet
//!
//! A worker with no jobs registered. Projection groups belong to modules, and
//! there are no modules yet (D11 keeps business domain out of the kernel), so
//! the composition below has nothing to compose. It still starts, claims
//! tenants, finds nothing to do, and shuts down cleanly — which is exactly what
//! is worth being able to run today, because every one of those steps is a place
//! deployment goes wrong.
//!
//! When `modules/ledger` lands in Phase 3b, its groups and effect handlers are
//! registered here and nothing else changes.

use std::sync::Arc;
use std::time::Duration;

use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use spa_eventlog::{Dispatcher, RetryPolicy};
use spa_worker::{OutboxJob, Worker, WorkerConfig, shutdown_signal};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url = std::env::var("CONTROL_DATABASE_URL")
        .map_err(|_| "CONTROL_DATABASE_URL is not set; the worker has nothing to connect to")?;

    // Tenant clusters are named in the control plane's `cluster` table, and
    // their credentials come from environment variables named there — never
    // from the table itself (architecture D13).
    let primary_url = std::env::var("PRIMARY_CLUSTER_URL")
        .map_err(|_| "PRIMARY_CLUSTER_URL is not set; no tenant database is reachable")?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&control_url)
        .await?;

    let clusters = ClusterRegistry::new().with_url("primary", &primary_url)?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    // Effect handlers come from modules too. An empty dispatcher claims nothing
    // — which is the same behaviour as a worker rolled out before a module's
    // handler exists, and is deliberately not an error.
    let dispatcher = Arc::new(Dispatcher::new(RetryPolicy::default()));

    let config = WorkerConfig {
        name: std::env::var("WORKER_NAME")
            .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned())),
        drain_timeout: Duration::from_secs(20),
        ..WorkerConfig::default()
    };

    let worker = Worker::new(control, config).with_job(Arc::new(OutboxJob::new(dispatcher, 64)));

    let shutdown = worker.run(shutdown_signal()).await;

    if !shutdown.drained {
        // Non-zero, because an orchestrator watching exit codes should be able
        // to tell a clean stop from one that ran out of time.
        tracing::error!("shut down without completing the drain");
        std::process::exit(1);
    }

    Ok(())
}
