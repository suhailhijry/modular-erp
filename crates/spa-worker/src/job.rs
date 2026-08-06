//! What a worker actually does to a tenant.

use spa_control::TenantDb;

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

    /// Does a bounded amount of work for one tenant.
    async fn tick(&self, db: &TenantDb) -> Result<Activity, BoxError>;
}
