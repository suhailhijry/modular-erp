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
        }
    }
}

#[async_trait::async_trait]
impl<G: ProjectionGroup> Job for ProjectionJob<G> {
    fn name(&self) -> &'static str {
        self.name
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
