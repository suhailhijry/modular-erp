//! Destroys demo tenants whose time is up.
//!
//! ```text
//! CONTROL_DATABASE_URL=… PRIMARY_CLUSTER_URL=… cargo run --bin reaper
//! ```
//!
//! # Why a one-shot rather than a job in the worker
//!
//! The worker's unit of work is *a tenant it holds a lease on*, and this deletes
//! the tenant — including the database the lease lives beside. It is fleet-level
//! work with a different shape, and giving the worker a second shape to support
//! one caller would be inventing structure.
//!
//! One-shot also means it can simply not be scheduled. A deployment with no
//! demos never runs it, and one that wants to look before it deletes runs it by
//! hand.
//!
//! Exits non-zero if the sweep itself failed. An individual tenant that could
//! not be destroyed is logged and retried on the next run — one unreachable
//! cluster must not keep every other expired demo alive.

use std::sync::Arc;

use spa_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};

/// Most a single run will destroy.
///
/// A cap rather than "everything", so a bug that marks the fleet as expired
/// costs one batch and an alarm rather than every tenant. Raise it when a real
/// backlog exists.
const PER_RUN: i64 = 100;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    let control = Arc::new(ControlPlane::new(
        pool,
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    let reaped = control.reap_expired_demos(PER_RUN).await?;
    tracing::info!(reaped, "demo sweep finished");

    Ok(())
}
