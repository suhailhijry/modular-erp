//! Destroys demo tenants whose time is up, abandons signups whose build never
//! finished, and forgets signups nobody answered and mail already delivered.
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
//! It still has to be scheduled, demos or not: a signup whose build died holds
//! its name until this runs, and stale links sit until it does. One-shot means
//! a person who wants to look before it deletes can also run it by hand.
//!
//! # Why the signup sweep rides along
//!
//! Same shape and the same schedule: fleet-level tidying with no tenant behind
//! it, cheap, and pointless to run often. An unanswered signup holds a password
//! hash and an address somebody typed, and neither is worth keeping a day after
//! the link stopped working. The control plane's delivered mail and texts ride
//! along for the same reason, after thirty days — see `Retention::sweep_control`.
//!
//! It runs **first**, and unconditionally: it touches only the control plane,
//! so it cannot be held up by a cluster the demo sweep cannot reach.
//!
//! # Why the stuck-provisioning sweep rides along
//!
//! A signup's build compensates itself when it fails — but only if its process
//! lives to run the compensation. One killed mid-build by a crash or a deploy
//! leaves a tenant `provisioning`, holding its name, for ever. This runs that
//! compensation for it, after `PROVISIONING_GRACE_SECONDS`, so a stuck name is
//! held for up to that plus this binary's schedule.
//!
//! Exits non-zero if a sweep itself failed. An individual tenant that could not
//! be destroyed is logged and retried on the next run — one unreachable cluster
//! must not keep every other expired demo alive.

use std::sync::Arc;

use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};

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

    let forgotten = control.sweep_signups().await?;
    tracing::info!(forgotten, "unanswered signups swept");

    // An hour after it was minted a spent reset link and an unopened one are
    // both rubbish, and both are a row naming somebody who forgot a password.
    let stale = control.sweep_password_resets().await?;
    tracing::info!(stale, "expired reset links swept");

    // The same, for the links mailed after somebody's second factor was reset.
    // **It reopens nothing**: the account stays link-only — that fact is on the
    // identity, not on the row — so a swept link means asking for a fresh one,
    // never a password-only enrolment.
    let enrolments = control.sweep_enrolment_links().await?;
    tracing::info!(enrolments, "expired enrolment links swept");

    // The control plane's delivered mail and texts, after the tenant plane's
    // thirty days. Here rather than in the worker for the reason
    // `Retention::sweep_control` gives.
    let receipts = erp_worker::Retention::sweep_control(
        &control,
        erp_types::Timestamp::from(chrono::Utc::now()),
    )
    .await?;
    tracing::info!(receipts, "delivered control-plane effects swept");

    let reaped = control.reap_expired_demos(PER_RUN).await?;
    tracing::info!(reaped, "demo sweep finished");

    // Before the orphans, though the order does not matter: it drops a
    // database and the row naming it together, so it never makes one.
    let abandoned = control
        .reap_stuck_provisioning(erp_control::PROVISIONING_GRACE_SECONDS, PER_RUN)
        .await?;
    tracing::info!(abandoned, "stuck provisionings abandoned");

    // **Databases no tenant row claims.**
    //
    // Dropped only when the database itself says it holds nothing: no events,
    // and no setting anybody chose. The control plane cannot answer this —
    // `provision` writes the row before it creates the database, so a dead
    // provisioning leaves a row (the sweep above), not an unclaimed database;
    // one of those is usually a control plane that has lost rows. Asking the
    // database is asking the one party that is not in doubt.
    //
    // One occupied or unreadable database refuses the whole cluster's sweep,
    // which is the case `restore.rs` calls dangerous and is right to.
    for cluster in control.cluster_names().await? {
        match control
            .drop_empty_orphans(
                &cluster,
                erp_control::ORPHAN_GRACE_SECONDS,
                usize::try_from(PER_RUN).unwrap_or(usize::MAX),
            )
            .await
        {
            Ok(dropped) if dropped.is_empty() => {}
            Ok(dropped) => tracing::warn!(
                cluster = %cluster,
                dropped = dropped.len(),
                names = ?dropped,
                "empty unclaimed tenant databases dropped"
            ),
            Err(e) => tracing::error!(
                cluster = %cluster,
                error = %e,
                "unclaimed tenant databases on this cluster were not swept"
            ),
        }
    }

    Ok(())
}
