//! Connection management for database-per-tenant.
//!
//! # The problem this exists to solve
//!
//! Postgres spends a process per connection. A thousand tenants with a pool of
//! four each would want four thousand connections, which no cluster will give
//! you. Database-per-tenant fails here or it fails nowhere, so the strategy is
//! explicit rather than emergent:
//!
//! 1. **Pools are `min_connections(0)` with an idle timeout.** An idle tenant
//!    holds no connections at all. Pool objects are cheap; connections are not.
//! 2. **A global permit is required to hold a [`TenantDb`].** One handle is
//!    taken per unit of work, so the number of connections in flight tracks
//!    *concurrency*, not tenant count. This is the actual budget.
//! 3. **Pool objects are LRU-capped** to bound memory when many tenants are
//!    touched in sequence.
//!
//! When the budget is exhausted, acquisition fails fast with
//! [`PoolError::Overloaded`] — a 503 — rather than queueing behind a saturated
//! cluster and turning a slow database into an outage.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spa_types::TenantId;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no cluster named {0:?} is configured")]
    UnknownCluster(String),
    #[error("the connection budget is exhausted; retry shortly")]
    Overloaded,
    #[error(transparent)]
    Connect(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Ceiling on concurrently-held [`TenantDb`] handles across every tenant.
    ///
    /// This is the connection budget. Set it from the cluster's
    /// `max_connections` with headroom for the control plane and for
    /// maintenance sessions — not from an expected tenant count, which is the
    /// number it deliberately does not depend on.
    pub max_concurrent_operations: usize,
    /// Per-tenant ceiling. Small on purpose: one request should not be able to
    /// consume a meaningful share of the budget.
    pub max_connections_per_tenant: u32,
    /// How long an unused connection is kept before being returned to the
    /// server. The lower this is, the more sharply connection count tracks
    /// load; the higher, the fewer reconnects under bursty traffic.
    pub idle_timeout: Duration,
    /// How long to wait for a connection from a tenant's own pool before
    /// giving up.
    pub acquire_timeout: Duration,
    /// Pool objects retained. Exceeding this evicts the least recently used.
    pub max_cached_pools: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent_operations: 64,
            max_connections_per_tenant: 4,
            idle_timeout: Duration::from_secs(30),
            acquire_timeout: Duration::from_secs(5),
            max_cached_pools: 256,
        }
    }
}

/// Where a tenant's database physically lives.
///
/// A registry rather than a single URL, because the tenant table records a
/// cluster from day one — promoting a large tenant to dedicated hardware has to
/// be a row change, not a redeploy.
#[derive(Debug, Clone)]
pub struct ClusterRegistry {
    clusters: HashMap<String, PgConnectOptions>,
}

impl ClusterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clusters: HashMap::new(),
        }
    }

    /// Registers a cluster from a connection URL. The database component is
    /// ignored — each tenant supplies its own.
    pub fn with_url(mut self, name: impl Into<String>, url: &str) -> Result<Self, sqlx::Error> {
        let options: PgConnectOptions = url.parse()?;
        self.clusters.insert(name.into(), options);
        Ok(self)
    }

    fn options_for(&self, cluster: &str, database: &str) -> Result<PgConnectOptions, PoolError> {
        self.clusters
            .get(cluster)
            .map(|base| base.clone().database(database))
            .ok_or_else(|| PoolError::UnknownCluster(cluster.to_owned()))
    }
}

impl Default for ClusterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Cached {
    pool: PgPool,
    last_used: Instant,
}

/// Per-tenant pools, bounded by a shared budget.
#[derive(Debug)]
pub struct TenantPools {
    clusters: ClusterRegistry,
    config: PoolConfig,
    budget: Arc<Semaphore>,
    cache: Mutex<HashMap<TenantId, Cached>>,
}

impl TenantPools {
    #[must_use]
    pub fn new(clusters: ClusterRegistry, config: PoolConfig) -> Self {
        Self {
            budget: Arc::new(Semaphore::new(config.max_concurrent_operations)),
            clusters,
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// A pool for this tenant plus the budget permit that licenses its use.
    ///
    /// The permit must be held for as long as the pool is used — it is what
    /// makes the connection count track concurrency. [`TenantDb`] holds it, and
    /// releases it on drop.
    ///
    /// [`TenantDb`]: crate::TenantDb
    pub(crate) async fn acquire(
        &self,
        tenant: TenantId,
        cluster: &str,
        database: &str,
    ) -> Result<(PgPool, OwnedSemaphorePermit), PoolError> {
        // Fail fast rather than queue: a caller waiting on an exhausted budget
        // is a request holding a connection it will not get, which is how a
        // slow database becomes an outage.
        let permit = Arc::clone(&self.budget)
            .try_acquire_owned()
            .map_err(|_| PoolError::Overloaded)?;

        let pool = self.pool_for(tenant, cluster, database).await?;
        Ok((pool, permit))
    }

    async fn pool_for(
        &self,
        tenant: TenantId,
        cluster: &str,
        database: &str,
    ) -> Result<PgPool, PoolError> {
        let mut cache = self.cache.lock().await;

        if let Some(entry) = cache.get_mut(&tenant) {
            entry.last_used = Instant::now();
            return Ok(entry.pool.clone());
        }

        let options = self.clusters.options_for(cluster, database)?;
        let pool = PgPoolOptions::new()
            // Zero minimum is the whole point: an idle tenant costs nothing.
            .min_connections(0)
            .max_connections(self.config.max_connections_per_tenant)
            .idle_timeout(Some(self.config.idle_timeout))
            .acquire_timeout(self.config.acquire_timeout)
            // Lazy: registering a tenant must not require its database to be
            // reachable this instant.
            .connect_lazy_with(options);

        if cache.len() >= self.config.max_cached_pools {
            Self::evict_oldest(&mut cache);
        }
        cache.insert(
            tenant,
            Cached {
                pool: pool.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(pool)
    }

    /// Drops the least recently used pool from the cache.
    ///
    /// Removed, not closed. A `PgPool` is reference-counted, so a request still
    /// holding a clone keeps working; the pool is torn down when the last user
    /// finishes. Closing here would break in-flight work to save memory, which
    /// is the wrong trade.
    fn evict_oldest(cache: &mut HashMap<TenantId, Cached>) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, cached)| cached.last_used)
            .map(|(tenant, _)| *tenant)
        {
            cache.remove(&oldest);
        }
    }

    /// Permits currently available. Exposed for the health endpoint and for
    /// tests that assert the budget is enforced.
    #[must_use]
    pub fn available_budget(&self) -> usize {
        self.budget.available_permits()
    }

    /// Number of pool objects held. Not the number of open connections — with
    /// `min_connections(0)` most of these hold none.
    pub async fn cached_pool_count(&self) -> usize {
        self.cache.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ClusterRegistry {
        ClusterRegistry::new()
            .with_url("primary", "postgres://postgres@localhost/postgres")
            .expect("a valid URL parses")
    }

    fn pools(config: PoolConfig) -> TenantPools {
        TenantPools::new(registry(), config)
    }

    #[tokio::test]
    async fn an_unknown_cluster_is_refused() {
        let pools = pools(PoolConfig::default());
        let err = pools
            .acquire(TenantId::new(), "does-not-exist", "spa_tenant_x")
            .await
            .expect_err("an unregistered cluster cannot be connected to");
        assert!(matches!(err, PoolError::UnknownCluster(_)));
    }

    /// The budget is the mechanism that makes database-per-tenant survivable, so
    /// it gets a test that actually exhausts it.
    #[tokio::test]
    async fn the_budget_bounds_concurrent_handles_not_tenants() {
        let pools = pools(PoolConfig {
            max_concurrent_operations: 2,
            ..PoolConfig::default()
        });

        // Two different tenants, so this is bounding concurrency rather than
        // per-tenant use.
        let first = pools
            .acquire(TenantId::new(), "primary", "postgres")
            .await
            .expect("within budget");
        let second = pools
            .acquire(TenantId::new(), "primary", "postgres")
            .await
            .expect("within budget");
        assert_eq!(pools.available_budget(), 0);

        let third = pools.acquire(TenantId::new(), "primary", "postgres").await;
        assert!(
            matches!(third, Err(PoolError::Overloaded)),
            "over budget must fail fast, not queue"
        );

        // Releasing a permit frees capacity immediately.
        drop(first);
        assert_eq!(pools.available_budget(), 1);
        // Bound, not dropped: an unbound permit would be released instantly and
        // this would assert nothing.
        let fourth = pools
            .acquire(TenantId::new(), "primary", "postgres")
            .await
            .expect("capacity was returned");
        assert_eq!(pools.available_budget(), 0);

        drop(second);
        drop(fourth);
        assert_eq!(pools.available_budget(), 2);
    }

    #[tokio::test]
    async fn pools_are_reused_across_acquisitions_of_one_tenant() {
        let pools = pools(PoolConfig::default());
        let tenant = TenantId::new();

        let (first, permit) = pools
            .acquire(tenant, "primary", "postgres")
            .await
            .expect("within budget");
        drop(permit);
        let (second, _permit) = pools
            .acquire(tenant, "primary", "postgres")
            .await
            .expect("within budget");

        assert_eq!(pools.cached_pool_count().await, 1);
        // sqlx pools are Arc-backed; the same underlying pool is handed out.
        assert!(std::ptr::eq(
            std::ptr::from_ref::<sqlx::pool::PoolOptions<sqlx::Postgres>>(first.options()),
            std::ptr::from_ref::<sqlx::pool::PoolOptions<sqlx::Postgres>>(second.options()),
        ));
    }

    #[tokio::test]
    async fn the_pool_cache_is_bounded() {
        let pools = pools(PoolConfig {
            max_cached_pools: 3,
            ..PoolConfig::default()
        });

        for _ in 0..10 {
            let (_pool, permit) = pools
                .acquire(TenantId::new(), "primary", "postgres")
                .await
                .expect("within budget");
            drop(permit);
        }

        assert_eq!(
            pools.cached_pool_count().await,
            3,
            "touching many tenants in sequence must not grow the cache without bound"
        );
    }
}
