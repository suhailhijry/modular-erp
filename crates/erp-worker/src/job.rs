//! What a worker actually does to a tenant.

use erp_control::TenantDb;
use erp_types::ModuleId;

/// Whether a tick found anything to do.
///
/// Drives the visit schedule: a tenant that worked is looked at again
/// immediately, one that did not is pushed out by the idle interval. Getting
/// this wrong in the `Worked` direction burns connections on an idle tenant;
/// getting it wrong in the `Idle` direction leaves work sitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Worked,
}

impl Activity {
    #[must_use]
    pub const fn worked(&self) -> bool {
        matches!(self, Self::Worked)
    }
}

/// Whatever a job's own error type is.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// One kind of background work, for one tenant.
///
/// # The contract
///
/// - **Bounded.** A tick does *some* work and returns; it does not loop until
///   finished. The worker decides how many ticks a tenant gets before yielding
///   its slot, and that is what stops one busy tenant starving the rest.
/// - **Abandonable.** The worker may stop calling `tick` at any point between
///   calls — a deploy, a crash, a lost lease. Anything a tick leaves behind must
///   be safe to find later, which for everything here means each tick is its own
///   transaction.
/// - **Honest about `Activity`.** See [`Activity`].
#[async_trait::async_trait]
pub trait Job: Send + Sync + 'static {
    /// Stable name, for logs and metrics.
    fn name(&self) -> &'static str;

    /// The module this job belongs to, if any.
    ///
    /// The worker skips it for tenants that have not enabled that module — so a
    /// tenant declining accounting pays nothing for its projections, which is
    /// what "modular" has to mean if it is going to mean anything. `None` is a
    /// kernel job that every tenant gets.
    fn module(&self) -> Option<ModuleId> {
        None
    }

    /// Does a bounded amount of work for one tenant.
    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError>;
}

/// Background work for the **platform**, not for any one tenant.
///
/// # Why this is not a [`Job`]
///
/// Because a `Job` is handed a `TenantDb`, and the things that need this have no
/// tenant. The control plane's outbox is the first: an invitation is a
/// control-plane row, so the promise to email it is a control-plane row, and
/// there is no tenant database it could sensibly live in.
///
/// Running it as a `Job` would mean doing control-plane work once per tenant,
/// under a tenant's lease, N times a cycle. `SKIP LOCKED` would make that safe
/// and it would still be wrong: the amount of work would scale with the number
/// of tenants rather than with the amount of work.
///
/// # When it runs
///
/// Once per claim cycle, inline, before tenants are claimed — so an idle
/// deployment with no tenants due still pumps the queue every
/// `empty_claim_pause`.
///
/// ponytail: inline means a slow relay delays tenant claiming by however long
/// the batch takes. Bounded, because every implementation takes a batch limit.
/// Move it onto the task tracker if a platform job ever grows one that is not.
#[async_trait::async_trait]
pub trait PlatformJob: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Does a bounded amount of work against the control plane.
    async fn tick(&self, control: &erp_control::ControlPlane) -> Result<Activity, BoxError>;
}
