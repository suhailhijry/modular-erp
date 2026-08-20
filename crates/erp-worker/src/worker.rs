//! Claiming tenants, working them, and stopping without losing anything.

use std::sync::Arc;
use std::time::Duration;

use erp_control::{ControlPlane, TenantDb, WorkSchedule};
use erp_types::TenantId;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::job::{Activity, Job};

/// How the worker paces itself.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Identifies this worker in the lease column. A hostname or pod name.
    pub name: String,
    pub schedule: WorkSchedule,
    /// Tenants claimed per round.
    pub tenants_per_claim: i64,
    /// Tenants worked at once.
    ///
    /// The number that sets this worker's share of the connection budget:
    /// measured, open connections track `active_tenants × per_tenant_pool`, and
    /// this *is* `active_tenants` for this process.
    pub concurrency: usize,
    /// How many rounds of every job one visit gets before the tenant yields its
    /// slot.
    ///
    /// Bounds how long a tenant with a large backlog can hold a slot. It does
    /// not bound how much work gets done — the tenant is immediately due again,
    /// so it goes to the back of the queue rather than to the back of the day.
    pub max_ticks_per_visit: usize,
    /// How long to wait before claiming again when nothing was due.
    pub empty_claim_pause: Duration,
    /// How long in-flight visits get to finish after cancellation.
    pub drain_timeout: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            name: "worker".to_owned(),
            schedule: WorkSchedule::default(),
            tenants_per_claim: 32,
            concurrency: 8,
            max_ticks_per_visit: 16,
            empty_claim_pause: Duration::from_millis(250),
            // Longer than one batch, shorter than any sensible orchestrator's
            // SIGKILL delay. Kubernetes defaults to thirty seconds, so this
            // leaves room for the process to exit afterwards.
            drain_timeout: Duration::from_secs(20),
        }
    }
}

/// What a run did, and how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shutdown {
    pub visits: usize,
    pub failed_visits: usize,
    /// Whether every in-flight visit finished inside the drain timeout.
    ///
    /// **False is not data loss.** An abandoned visit rolls its transaction
    /// back, so the tenant is exactly where it was; it is a signal that the
    /// drain timeout is too short for the batch size.
    pub drained: bool,
    pub leases_released: u64,
}

/// The background worker.
///
/// # Shutdown
///
/// Cancellation is checked **between units of work, never inside one**:
///
/// 1. The claim loop stops asking for tenants.
/// 2. Visits already in flight keep going, and check the token between job
///    ticks — so the batch that is running commits, and the next one does not
///    start.
/// 3. [`TaskTracker::wait`] blocks until they are done, or the drain timeout
///    passes.
/// 4. Leases are released, so a replacement picks the tenants up in
///    milliseconds rather than waiting for them to lapse.
///
/// Step 2 is the part worth being precise about. Aborting mid-batch would also
/// be *safe* — the transaction rolls back and the checkpoint stays put — but it
/// throws away work that was about to commit, on every deploy, for every tenant.
/// Letting the batch finish costs one batch of latency and loses nothing.
pub struct Worker {
    control: Arc<ControlPlane>,
    jobs: Arc<Vec<Arc<dyn Job>>>,
    platform: Arc<Vec<Arc<dyn crate::PlatformJob>>>,
    config: WorkerConfig,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field(
                "jobs",
                &self.jobs.iter().map(|j| j.name()).collect::<Vec<_>>(),
            )
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Worker {
    #[must_use]
    pub fn new(control: Arc<ControlPlane>, config: WorkerConfig) -> Self {
        Self {
            control,
            jobs: Arc::new(Vec::new()),
            platform: Arc::new(Vec::new()),
            config,
        }
    }

    /// Adds work that is not about any one tenant. See
    /// [`PlatformJob`](crate::PlatformJob).
    #[must_use]
    pub fn with_platform_job(mut self, job: Arc<dyn crate::PlatformJob>) -> Self {
        Arc::make_mut(&mut self.platform).push(job);
        self
    }

    #[must_use]
    pub fn with_job(mut self, job: Arc<dyn Job>) -> Self {
        Arc::make_mut(&mut self.jobs).push(job);
        self
    }

    /// Runs until cancelled.
    pub async fn run(&self, cancel: CancellationToken) -> Shutdown {
        let tracker = TaskTracker::new();
        let slots = Arc::new(Semaphore::new(self.config.concurrency));
        let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut visits = 0usize;

        tracing::info!(
            worker = %self.config.name,
            jobs = ?self.jobs.iter().map(|j| j.name()).collect::<Vec<_>>(),
            concurrency = self.config.concurrency,
            "worker started"
        );

        while !cancel.is_cancelled() {
            // **Before claiming tenants**, so a deployment with nothing due
            // still pumps the platform queue every `empty_claim_pause` rather
            // than only when a tenant happens to need visiting.
            self.run_platform_jobs().await;

            let claimed = match self
                .control
                .claim_tenants(
                    &self.config.name,
                    self.config.tenants_per_claim,
                    self.config.schedule,
                )
                .await
            {
                Ok(tenants) => tenants,
                Err(e) => {
                    // The control plane is unreachable. Nothing this worker can
                    // do is useful until it comes back, so pause rather than
                    // spin — and say so loudly, because every tenant's
                    // projections are stalled while this persists.
                    tracing::error!(error = %e, "could not claim tenants");
                    if sleep_or_cancel(&cancel, self.config.empty_claim_pause).await {
                        break;
                    }
                    continue;
                }
            };

            if claimed.is_empty() {
                if sleep_or_cancel(&cancel, self.config.empty_claim_pause).await {
                    break;
                }
                continue;
            }

            for claim in claimed {
                // Taken before spawning, so the claim loop blocks when every
                // slot is busy instead of queueing an unbounded number of
                // tasks — which would make `concurrency` a lie and the
                // connection estimate with it.
                let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                    break;
                };

                visits += 1;
                let visit = Visit {
                    control: Arc::clone(&self.control),
                    jobs: Arc::clone(&self.jobs),
                    max_ticks: self.config.max_ticks_per_visit,
                    schedule: self.config.schedule,
                    tenant: claim.tenant.id,
                    // How many consecutive visits have already found nothing.
                    // The backoff is computed from it, which is what makes a
                    // dormant tenant nearly free — see `WorkSchedule`.
                    idle_visits: claim.idle_visits,
                    cancel: cancel.clone(),
                };
                let failures = Arc::clone(&failures);
                tracker.spawn(async move {
                    if !visit.run().await {
                        failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(slot);
                });
            }
        }

        tracker.close();
        let drained = tokio::time::timeout(self.config.drain_timeout, tracker.wait())
            .await
            .is_ok();
        if !drained {
            tracing::warn!(
                worker = %self.config.name,
                timeout_ms = self.config.drain_timeout.as_millis(),
                "drain timed out; in-flight visits were abandoned mid-batch. \
                 Nothing is lost — an abandoned batch rolls back — but the \
                 timeout is too short for the batch size."
            );
        }

        let leases_released = self
            .control
            .release_leases(&self.config.name)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not release leases; they will lapse instead");
                0
            });

        tracing::info!(
            worker = %self.config.name,
            visits,
            drained,
            leases_released,
            "worker stopped"
        );

        Shutdown {
            visits,
            // Read after the drain, so every visit that ran has reported.
            failed_visits: failures.load(std::sync::atomic::Ordering::Relaxed),
            drained,
            leases_released,
        }
    }
}

impl Worker {
    /// Runs every platform job once, in order.
    ///
    /// A failure is **logged and stepped over**, not propagated: a platform job
    /// that cannot reach its dependency must not stop the worker from visiting
    /// tenants, whose work has nothing to do with it. That is the same trade
    /// `Visit::work` makes for a tenant job, and for the same reason — the
    /// alternative is one broken relay stalling every projection on the fleet.
    async fn run_platform_jobs(&self) {
        for job in self.platform.iter() {
            match job.tick(&self.control).await {
                Ok(Activity::Worked) => {
                    tracing::debug!(job = job.name(), "platform job did work");
                }
                Ok(Activity::Idle) => {}
                Err(e) => {
                    tracing::error!(job = job.name(), error = %e, "platform job failed");
                }
            }
        }
    }
}

/// Sleeps, or returns early if cancelled. `true` means "stop".
async fn sleep_or_cancel(cancel: &CancellationToken, pause: Duration) -> bool {
    tokio::select! {
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(pause) => false,
    }
}

/// What a visit's job rounds came to.
///
/// `worked` decides when the tenant is next due; `failed` decides whether the
/// visit counts against the worker. They are independent — a visit that
/// projected two batches and then hit a bad event did both.
struct Work {
    worked: bool,
    failed: bool,
}

/// One tenant, one slot, until it runs out of work or its ticks.
struct Visit {
    control: Arc<ControlPlane>,
    jobs: Arc<Vec<Arc<dyn Job>>>,
    max_ticks: usize,
    schedule: WorkSchedule,
    tenant: TenantId,
    idle_visits: i32,
    cancel: CancellationToken,
}

impl Visit {
    /// `false` if the tenant could not be opened or a job failed.
    async fn run(self) -> bool {
        let db = match self.control.enter_for_maintenance(self.tenant).await {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(tenant = %self.tenant, error = %e, "could not open tenant");
                // Backed off like an idle tenant rather than retried at once: a
                // tenant whose database is unreachable would otherwise be
                // reclaimed in a tight loop by every worker in the fleet.
                self.reschedule(false).await;
                return false;
            }
        };

        let Work { worked, failed } = self.work(&db).await;
        self.reschedule(worked).await;
        !failed
    }

    /// Runs every job in turn until a full round finds nothing, ticks run out,
    /// or shutdown starts.
    async fn work(&self, db: &TenantDb) -> Work {
        let mut worked_at_all = false;

        for _ in 0..self.max_ticks {
            let mut worked_this_round = false;

            for job in self.jobs.iter() {
                // The cancellation check. Between ticks, so a batch that has
                // started commits and the next one never begins.
                if self.cancel.is_cancelled() {
                    tracing::debug!(
                        tenant = %self.tenant,
                        "stopping between ticks for shutdown"
                    );
                    return Work {
                        worked: worked_at_all,
                        failed: false,
                    };
                }

                // A module this tenant declined costs it nothing.
                if job.module().is_some_and(|m| !db.has_module(&m)) {
                    continue;
                }

                match job.tick(db).await {
                    Ok(Activity::Worked) => {
                        worked_this_round = true;
                        worked_at_all = true;
                    }
                    Ok(Activity::Idle) => {}
                    Err(e) => {
                        // L6: this tenant's work stops. It does not degrade into
                        // skipping the event and carrying on, and it does not
                        // take the worker down — the other tenants are fine and
                        // the projection-lag health check is what escalates
                        // this one.
                        tracing::error!(
                            tenant = %self.tenant,
                            job = job.name(),
                            error = %e,
                            "job failed; this tenant is stalled until it is fixed"
                        );
                        return Work {
                            worked: worked_at_all,
                            failed: true,
                        };
                    }
                }
            }

            if !worked_this_round {
                break;
            }
        }

        Work {
            worked: worked_at_all,
            failed: false,
        }
    }

    /// Decides when this tenant is next worth looking at, and drops the lease.
    async fn reschedule(&self, worked: bool) {
        // A visit that did work almost certainly has more to do — it may have
        // stopped on `max_ticks`, or on cancellation. A visit interrupted by
        // shutdown is likewise due immediately, so whoever replaces this worker
        // picks it up rather than waiting out an idle interval.
        let delay = if worked || self.cancel.is_cancelled() {
            Duration::ZERO
        } else {
            self.schedule.next_idle_delay(self.tenant, self.idle_visits)
        };

        if let Err(e) = self
            .control
            .schedule_next_visit(self.tenant, delay, worked)
            .await
        {
            // The lease lapses on its own, so this costs latency, not
            // correctness.
            tracing::warn!(
                tenant = %self.tenant,
                error = %e,
                "could not reschedule; the lease will lapse instead"
            );
        }
    }
}
