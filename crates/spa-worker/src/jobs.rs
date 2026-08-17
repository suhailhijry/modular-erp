//! The two jobs the kernel itself provides.
//!
//! Both are deliberately thin. Their whole reason to exist is that they take
//! their connections from [`TenantDb`], so background work is metered by the
//! same lane budget as everything else and no pool escapes the boundary that
//! makes cross-tenant access a type error.

use std::sync::Arc;

use spa_control::TenantDb;
use spa_eventlog::{Dispatcher, Upcasters};
use spa_projection::{Progress, Projection, ProjectionGroup, run_once_in};
use spa_types::ModuleId;

use crate::job::{Activity, BoxError, Job};

/// Advances one projection group.
///
/// # Where the transaction comes from
///
/// `TenantDb::begin` — so the connection is counted against the background lane
/// for exactly as long as the batch takes, and released the moment it commits.
/// [`run_once_in`] does the lease, the batch and the checkpoint inside it, which
/// is law L4; committing here is what makes it hold.
pub struct ProjectionJob<G: ProjectionGroup> {
    name: &'static str,
    projections: Vec<Arc<dyn Projection<Group = G>>>,
    upcasters: Arc<Upcasters>,
    batch_size: i64,
    module: Option<ModuleId>,
}

impl<G: ProjectionGroup> std::fmt::Debug for ProjectionJob<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionJob")
            .field("group", &G::NAME)
            .field("projections", &self.projections.len())
            .field("batch_size", &self.batch_size)
            .finish_non_exhaustive()
    }
}

impl<G: ProjectionGroup> ProjectionJob<G> {
    pub fn new(
        projections: Vec<Arc<dyn Projection<Group = G>>>,
        upcasters: Arc<Upcasters>,
        batch_size: i64,
    ) -> Self {
        Self {
            name: G::NAME,
            projections,
            upcasters,
            batch_size,
            module: None,
        }
    }

    /// Marks this group as belonging to a module, so tenants without it are
    /// skipped.
    #[must_use]
    pub fn for_module(mut self, module: ModuleId) -> Self {
        self.module = Some(module);
        self
    }
}

#[async_trait::async_trait]
impl<G: ProjectionGroup> Job for ProjectionJob<G> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn module(&self) -> Option<ModuleId> {
        self.module.clone()
    }

    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError> {
        let refs: Vec<&dyn Projection<Group = G>> =
            self.projections.iter().map(AsRef::as_ref).collect();

        let mut tx = db.begin().await?;
        let progress =
            match run_once_in::<G>(&mut tx, &refs, &self.upcasters, self.batch_size).await {
                Ok(progress) => progress,
                Err(e) => {
                    // Explicit rather than relying on the drop: an error here means
                    // a projection refused an event, and leaving the rollback
                    // implicit is how a future edit accidentally commits it.
                    tx.rollback().await?;
                    return Err(e.into());
                }
            };

        match progress {
            Progress::Advanced { .. } => {
                tx.commit().await?;
                Ok(Activity::Worked)
            }
            // Nothing was applied, so there is nothing to commit. `Busy` means
            // another worker holds the group — not an error, and not work.
            Progress::UpToDate { .. } | Progress::Busy => {
                tx.rollback().await?;
                Ok(Activity::Idle)
            }
        }
    }
}

/// Delivers what the outbox owes.
///
/// # Three connections, not one
///
/// Claim, deliver, settle — and the delivery in the middle holds **no**
/// connection, because it is network I/O with a timeout measured in seconds.
/// The two database moments take a permit each and give it straight back.
pub struct OutboxJob {
    dispatcher: Arc<Dispatcher>,
    batch_size: i64,
}

impl std::fmt::Debug for OutboxJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxJob")
            .field("kinds", &self.dispatcher.kinds())
            .field("batch_size", &self.batch_size)
            .finish_non_exhaustive()
    }
}

impl OutboxJob {
    #[must_use]
    pub const fn new(dispatcher: Arc<Dispatcher>, batch_size: i64) -> Self {
        Self {
            dispatcher,
            batch_size,
        }
    }
}

#[async_trait::async_trait]
impl Job for OutboxJob {
    fn name(&self) -> &'static str {
        "outbox"
    }

    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError> {
        let claimed = {
            let mut conn = db.acquire().await?;
            self.dispatcher.claim(&mut conn, self.batch_size).await?
        };

        if claimed.is_empty() {
            return Ok(Activity::Idle);
        }

        for effect in &claimed {
            let settlement = self.dispatcher.deliver(effect).await;

            let mut conn = db.acquire().await?;
            self.dispatcher
                .settle(&mut conn, effect, &settlement)
                .await?;
        }

        Ok(Activity::Worked)
    }
}

/// Delivers what the **control plane's** outbox owes.
///
/// # Why this exists beside [`OutboxJob`]
///
/// Same dispatcher, same table shape, different plane. An invitation is a
/// control-plane row, so the promise to email it is one too — there is no tenant
/// database it could live in, and putting it in one would mean writing across
/// two databases and losing the single transaction that makes the promise worth
/// anything.
///
/// It is a [`PlatformJob`] rather than a [`Job`] because it is not per-tenant.
/// See that trait for why running it per-tenant would be safe and still wrong.
pub struct PlatformOutboxJob {
    dispatcher: Arc<Dispatcher>,
    batch_size: i64,
}

impl std::fmt::Debug for PlatformOutboxJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformOutboxJob")
            .field("kinds", &self.dispatcher.kinds())
            .field("batch_size", &self.batch_size)
            .finish_non_exhaustive()
    }
}

impl PlatformOutboxJob {
    #[must_use]
    pub const fn new(dispatcher: Arc<Dispatcher>, batch_size: i64) -> Self {
        Self {
            dispatcher,
            batch_size,
        }
    }
}

#[async_trait::async_trait]
impl crate::PlatformJob for PlatformOutboxJob {
    fn name(&self) -> &'static str {
        "platform-outbox"
    }

    async fn tick(&self, control: &spa_control::ControlPlane) -> Result<Activity, BoxError> {
        // `dispatch_once` rather than the claim/deliver/settle dance `OutboxJob`
        // does by hand: that one exists to take a *permit* per database moment
        // out of a tenant's bulkhead. The control pool has no per-tenant budget
        // to protect, so the simpler call is the honest one.
        let pass = self
            .dispatcher
            .dispatch_once(control.pool(), self.batch_size)
            .await?;
        Ok(if pass.claimed == 0 {
            Activity::Idle
        } else {
            Activity::Worked
        })
    }
}
