//! **What this system forgets, and when.**
//!
//! # Why
//!
//! Every table that records something done — a delivered effect, a provider's
//! callback, a claim on a chair last spring, a short link in a text nobody will
//! tap again — is a receipt. Receipts are kept for a while and not for ever:
//! the first version had no sweep for any of them, so every tenant database
//! grew with every message sent and every booking made, was cloned and backed
//! up at that size, and would have kept growing for the life of the deployment.
//!
//! # What is never swept
//!
//! The event log, which is the truth; anything pending or dead in the outbox,
//! which is a promise still open; any link that is neither expired nor spent.
//! The windows below are generous, because a row costs an index entry and a
//! question that cannot be answered costs an argument with a provider.

use async_trait::async_trait;
use erp_control::{ControlPlane, TenantDb};
use erp_types::Timestamp;

use crate::{Activity, BoxError, Job, PlatformJob};

/// A delivered effect is kept this long, in either plane. Long enough to answer
/// "did we send the reminder" for last month's bookings.
pub const DELIVERED_EFFECTS: chrono::Duration = chrono::Duration::days(30);
/// A provider's callback payload is kept this long — a quarter, which is how
/// long an argument about a settlement takes.
pub const WEBHOOK_EVENTS: chrono::Duration = chrono::Duration::days(90);
/// A past occupancy claim is kept this long.
pub const OCCUPANCY_CLAIMS: chrono::Duration = chrono::Duration::days(180);
/// A dead short link is kept this long past its death.
pub const SHORT_LINKS: chrono::Duration = chrono::Duration::days(30);

/// The tenant-plane sweeps, as one kernel job every tenant gets — and
/// [`Retention::sweep_control`], the control plane's one, for the reaper.
#[derive(Debug, Clone, Copy, Default)]
pub struct Retention;

impl Retention {
    /// Runs every sweep once, as of `now`. What one tick of the job does, with
    /// the clock as a parameter so a test can hold it still.
    pub async fn sweep(db: &TenantDb, now: Timestamp) -> Result<u64, BoxError> {
        let mut conn = db.acquire().await?;
        let mut gone = 0;
        gone += erp_eventlog::sweep_delivered(&mut conn, now - DELIVERED_EFFECTS).await?;
        gone += erp_eventlog::sweep_webhook_events(&mut conn, now - WEBHOOK_EVENTS).await?;
        gone += erp_occupancy::sweep_ended_before(&mut conn, now - OCCUPANCY_CLAIMS).await?;
        gone += erp_links::sweep(&mut conn, now - SHORT_LINKS).await?;
        Ok(gone)
    }

    /// Forgets the control plane's delivered effects — signup, invitation and
    /// reset emails, sign-in texts — after the same [`DELIVERED_EFFECTS`], as
    /// of `now`. Pending and dead ones stay, as they do in a tenant's.
    ///
    /// **Called by the reaper, not run as a [`PlatformJob`].** Platform jobs
    /// run every claim cycle, a quarter of a second apart on an idle fleet, and
    /// `outbox` has no index on `delivered_at`, so this would scan the table
    /// four times a second on every worker. It is a sweep that is cheap once
    /// and pointless often, which is the reaper's shape.
    pub async fn sweep_control(control: &ControlPlane, now: Timestamp) -> Result<u64, BoxError> {
        let mut conn = control.pool().acquire().await?;
        Ok(erp_eventlog::sweep_delivered(&mut conn, now - DELIVERED_EFFECTS).await?)
    }
}

#[async_trait]
impl Job for Retention {
    fn name(&self) -> &'static str {
        "kernel.retention"
    }

    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError> {
        let gone = Self::sweep(db, Timestamp::from(chrono::Utc::now())).await?;
        Ok(if gone > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Removes sessions that have expired.** Control-plane, because identities
/// are; a session row is an index entry that grows with every sign-in.
#[derive(Debug, Clone, Copy, Default)]
pub struct SweepSessions;

#[async_trait]
impl PlatformJob for SweepSessions {
    fn name(&self) -> &'static str {
        "control.sweep_sessions"
    }

    async fn tick(&self, control: &ControlPlane) -> Result<Activity, BoxError> {
        let gone = control.sweep_sessions().await?;
        Ok(if gone > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}
