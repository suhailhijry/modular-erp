//! Connection management for database-per-tenant.
//!
//! # The problem this exists to solve
//!
//! Postgres spends a process per connection. Five thousand tenants with a pool
//! of four each would want twenty thousand connections, which no cluster will
//! give you. Database-per-tenant fails here or it fails nowhere.
//!
//! # What bounds what
//!
//! The load this is sized for is client-facing: a tenant's customers book and
//! buy through the same API their employees use at the counter. So request rates
//! are high and the arithmetic matters. By Little's law, connection demand is
//! *arrival rate × how long a connection is held*:
//!
//! | 10,000 req/s, permit held for… | connections needed |
//! |---|---|
//! | the whole request (~40 ms) | ~400 |
//! | one query (~8 ms, 1.5/request) | ~120 |
//!
//! So a permit is taken **per database operation**, not per request. A handle
//! held while business logic runs, an HTTP call is made, or a response is
//! serialized costs nothing. This is the difference between the design scaling
//! and not.
//!
//! # The bound that actually sizes a cluster
//!
//! The lane budget does **not** bound open connections, and assuming it does is
//! the mistake this paragraph exists to prevent. A connection returned to a
//! tenant's pool stays open until the idle timeout, so connections accumulate
//! across every tenant touched in that window no matter how small the budget is.
//! Two ceilings, two different quantities:
//!
//! | quantity | bounded by |
//! |---|---|
//! | connections *executing* | the [`Lane`] budget |
//! | connections *open* | active tenants × `max_connections_per_tenant` |
//!
//! Measured, not assumed. `tests/soak.rs` drives 256 workers across 40 tenants
//! and observes:
//!
//! ```text
//! max_connections_per_tenant = 1  ->  peak open = 40   (exactly the tenant count)
//! max_connections_per_tenant = 2  ->  peak open = 81
//! max_connections_per_tenant = 4  ->  peak open = 95
//! ```
//!
//! So the planning rule is:
//!
//! ```text
//! connections_per_cluster ≈ concurrently_active_tenants × max_connections_per_tenant
//! ```
//!
//! **Cluster count is driven by concurrently-active tenants — not by total
//! tenants, and not by request rate.** Five thousand tenants at 25% concurrent
//! activity and two connections each is ~2,500 connections; at a comfortable
//! ~400 per Postgres instance that is seven or eight instances, which is also
//! roughly what storage would dictate. The two constraints happen to agree at
//! this scale. They will not always, and when they diverge this is the one that
//! decides.
//!
//! Lowering `max_connections_per_tenant` is the cheapest lever, and it is not
//! free: it caps how much work a single busy tenant can do in parallel. The soak
//! test showed throughput *rising* as it fell (7.7k → 22.2k ops/s), because
//! connection churn dominated — so measure before assuming a bigger pool helps.
//!
//! # Bulkheads
//!
//! One budget shared by everything means a flood of consumer bookings starves
//! the employee at the counter. Permits are therefore drawn from a [`Lane`], each
//! with its own allowance, so saturating one leaves the others untouched.
//!
//! # Fail fast
//!
//! Exhaustion returns [`PoolError::Overloaded`] — a 503 — rather than queueing.
//! A caller waiting on an exhausted budget is a request holding resources it will
//! not get, which is how a slow database becomes an outage.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spa_types::TenantId;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool, Postgres};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no cluster named {0:?} is configured")]
    UnknownCluster(String),
    #[error("the {lane} connection budget is exhausted; retry shortly")]
    Overloaded { lane: Lane },
    #[error(transparent)]
    Connect(#[from] sqlx::Error),
}

/// Which bulkhead an operation draws its budget from.
///
/// Sized separately so one class of traffic cannot exhaust another. The API
/// layer picks the lane from the authenticated audience and the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// An employee waiting on a screen. Smallest allowance, most protected —
    /// a counter that stops working is worse than a slow consumer app.
    Interactive,
    /// A tenant's customers, through their app or website. The flood.
    Client,
    /// Projections, outbox delivery, migrations, reapers. Yields to both of the
    /// above: nobody is watching.
    Background,
}

impl std::fmt::Display for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Interactive => "interactive",
            Self::Client => "client",
            Self::Background => "background",
        })
    }
}

/// Per-lane operation allowances.
///
/// **These are per process.** Four API nodes with `client_operations: 240` allow
/// 960 concurrent client operations in total, so the sum across nodes must fit
/// the cluster's `max_connections` with headroom for the control plane and for
/// maintenance sessions.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub interactive_operations: usize,
    pub client_operations: usize,
    pub background_operations: usize,
    /// Per-tenant ceiling. Small on purpose: one busy tenant should not be able
    /// to consume a meaningful share of a node's budget.
    pub max_connections_per_tenant: u32,
    /// How long an unused connection is kept before being returned to the
    /// server. Lower makes connection count track load more sharply; higher
    /// means fewer reconnects under bursty traffic.
    pub idle_timeout: Duration,
    /// How long to wait for a connection from a tenant's own pool. A permit is
    /// already held at this point, so this is the inner, shorter timeout.
    pub acquire_timeout: Duration,
    /// Pool objects retained. Exceeding this evicts the least recently used.
    pub max_cached_pools: usize,
}

impl Default for PoolConfig {
    /// Sized for roughly 10,000 requests/second per node at ~8 ms of database
    /// time per query.
    ///
    /// `max_connections_per_tenant` is deliberately small: it multiplies
    /// directly into open connections across every active tenant (see the module
    /// docs), and the soak test found throughput *improving* as it came down,
    /// because connection churn cost more than the extra parallelism bought.
    /// `idle_timeout` is short for the same reason — it is what drains
    /// connections from tenants that have gone quiet.
    ///
    /// Validate against your own load before trusting these. The soak test is
    /// the instrument.
    fn default() -> Self {
        Self {
            interactive_operations: 100,
            client_operations: 240,
            background_operations: 60,
            max_connections_per_tenant: 4,
            idle_timeout: Duration::from_secs(10),
            acquire_timeout: Duration::from_secs(3),
            max_cached_pools: 1024,
        }
    }
}

#[derive(Debug)]
struct Budget {
    interactive: Arc<Semaphore>,
    client: Arc<Semaphore>,
    background: Arc<Semaphore>,
}

impl Budget {
    fn new(config: &PoolConfig) -> Self {
        Self {
            interactive: Arc::new(Semaphore::new(config.interactive_operations)),
            client: Arc::new(Semaphore::new(config.client_operations)),
            background: Arc::new(Semaphore::new(config.background_operations)),
        }
    }

    fn lane(&self, lane: Lane) -> &Arc<Semaphore> {
        match lane {
            Lane::Interactive => &self.interactive,
            Lane::Client => &self.client,
            Lane::Background => &self.background,
        }
    }

    fn try_acquire(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError> {
        Arc::clone(self.lane(lane))
            .try_acquire_owned()
            .map_err(|_| PoolError::Overloaded { lane })
    }

    fn available(&self, lane: Lane) -> usize {
        self.lane(lane).available_permits()
    }
}

/// Where a cluster's databases live, and where its replicas are.
#[derive(Debug, Clone)]
struct Cluster {
    primary: PgConnectOptions,
    /// Falls back to the primary when absent, so `read()` is always callable and
    /// adding replicas later is a configuration change rather than a code change.
    replica: Option<PgConnectOptions>,
}

/// The tenant → physical location map.
///
/// A registry rather than a single URL, because the tenant table records a
/// cluster from day one — promoting a large tenant to dedicated hardware has to
/// be a row change, not a redeploy. At the scale in the module docs this is not
/// hypothetical: 5,000 tenants at ~700 databases per instance is eight
/// instances before anyone has done anything unusual.
#[derive(Debug, Clone, Default)]
pub struct ClusterRegistry {
    clusters: HashMap<String, Cluster>,
}

impl ClusterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a cluster's primary. The database component of the URL is
    /// ignored — each tenant supplies its own.
    pub fn with_url(mut self, name: impl Into<String>, url: &str) -> Result<Self, sqlx::Error> {
        let primary: PgConnectOptions = url.parse()?;
        self.clusters.insert(
            name.into(),
            Cluster {
                primary,
                replica: None,
            },
        );
        Ok(self)
    }

    /// Adds a read replica to an already-registered cluster.
    ///
    /// Reads routed here may lag the primary, so this is opt-in per query via
    /// [`TenantDb::read`](crate::TenantDb::read) rather than automatic. Anything
    /// that must observe its own write uses the primary.
    ///
    /// **A name that is not registered is an error**, not a no-op. It used to be
    /// a no-op, which meant `with_replica("primry", …)` returned `Ok` with the
    /// replica silently dropped: every read would go to the primary, the deploy
    /// would look correct, and the only symptom would be a primary carrying
    /// twice the load it was sized for.
    pub fn with_replica(mut self, name: &str, url: &str) -> Result<Self, sqlx::Error> {
        let replica: PgConnectOptions = url.parse()?;
        let cluster = self.clusters.get_mut(name).ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no cluster named `{name}` to attach a replica to").into(),
            )
        })?;
        cluster.replica = Some(replica);
        Ok(self)
    }

    /// The clusters this deployment can reach, from the environment.
    ///
    /// `PRIMARY_CLUSTER_URL` is required and `PRIMARY_REPLICA_URL` is not. Read
    /// here rather than in each binary because there are five of them and they
    /// were already drifting: every one registered the primary and **not one of
    /// them ever called [`Self::with_replica`]**, so the replica routing in
    /// `TenantDb::read` was reachable only from a unit test.
    ///
    /// Credentials come from the environment and never from the `cluster` table
    /// (architecture D13), which is why this reads variables rather than rows.
    pub fn from_env() -> Result<Self, sqlx::Error> {
        Self::from_urls(
            std::env::var("PRIMARY_CLUSTER_URL").ok().as_deref(),
            std::env::var("PRIMARY_REPLICA_URL").ok().as_deref(),
        )
    }

    /// [`Self::from_env`] with the environment already read.
    ///
    /// Separate so the decision — required, optional, blank-means-absent — can
    /// be tested without `set_var`. Editing the process environment from a test
    /// needs `unsafe`, which this workspace denies, and races every other test
    /// in the same binary regardless.
    fn from_urls(primary: Option<&str>, replica: Option<&str>) -> Result<Self, sqlx::Error> {
        let primary = primary
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| {
                sqlx::Error::Configuration(
                    "PRIMARY_CLUSTER_URL is not set; no tenant database is reachable".into(),
                )
            })?;
        let registry = Self::new().with_url("primary", primary)?;

        // Blank is absent. A compose file or a unit file that declares the
        // variable and leaves it empty is the normal way to say "no replica
        // here", and treating `""` as a URL fails at parse with a message about
        // nothing.
        match replica.filter(|url| !url.trim().is_empty()) {
            Some(url) => {
                tracing::info!("read replica configured for cluster `primary`");
                registry.with_replica("primary", url)
            }
            // Not a warning. A single-node deployment is a supported shape, and
            // `read()` falls back to the primary — which is why it is always
            // safe to call and why adding a replica later is configuration.
            None => Ok(registry),
        }
    }

    fn options_for(
        &self,
        cluster: &str,
        database: &str,
        role: Role,
    ) -> Result<PgConnectOptions, PoolError> {
        let entry = self
            .clusters
            .get(cluster)
            .ok_or_else(|| PoolError::UnknownCluster(cluster.to_owned()))?;
        let base = match role {
            Role::Read => entry.replica.as_ref().unwrap_or(&entry.primary),
            Role::Write => &entry.primary,
        };
        Ok(base.clone().database(database))
    }

    fn has_replica(&self, cluster: &str) -> bool {
        self.clusters
            .get(cluster)
            .is_some_and(|c| c.replica.is_some())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Write,
    Read,
}

#[derive(Debug)]
struct Cached {
    pool: PgPool,
    last_used: Instant,
}

/// Per-tenant pools, bounded by per-lane budgets.
#[derive(Debug)]
pub struct TenantPools {
    clusters: ClusterRegistry,
    config: PoolConfig,
    budget: Budget,
    cache: Mutex<HashMap<(TenantId, Role), Cached>>,
}

impl TenantPools {
    #[must_use]
    pub fn new(clusters: ClusterRegistry, config: PoolConfig) -> Self {
        Self {
            budget: Budget::new(&config),
            clusters,
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The pools a [`TenantDb`](crate::TenantDb) needs, without spending budget.
    ///
    /// Deliberately free: obtaining a handle is not an operation. Budget is
    /// spent when a connection is actually taken, which is what keeps demand
    /// proportional to concurrent queries rather than to concurrent requests.
    pub(crate) async fn handles(
        &self,
        tenant: TenantId,
        cluster: &str,
        database: &str,
    ) -> Result<(PgPool, Option<PgPool>), PoolError> {
        let write = self
            .pool_for(tenant, cluster, database, Role::Write)
            .await?;
        let read = if self.clusters.has_replica(cluster) {
            Some(self.pool_for(tenant, cluster, database, Role::Read).await?)
        } else {
            None
        };
        Ok((write, read))
    }

    pub(crate) fn permit(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError> {
        self.budget.try_acquire(lane)
    }

    /// Connect options for a cluster, with no database chosen.
    ///
    /// For provisioning, which needs a connection to `postgres` before the
    /// tenant's database exists. Not a pool: these are one-shot.
    pub(crate) fn cluster_options(&self, cluster: &str) -> Result<PgConnectOptions, PoolError> {
        self.clusters.options_for(cluster, "postgres", Role::Write)
    }

    /// Closes and forgets a tenant's pools.
    ///
    /// `DROP DATABASE` fails while anything is connected, so abandoning a
    /// half-built tenant has to let go first.
    pub(crate) async fn forget(&self, tenant: TenantId) {
        let mut cache = self.cache.lock().await;
        for role in [Role::Write, Role::Read] {
            if let Some(entry) = cache.remove(&(tenant, role)) {
                entry.pool.close().await;
            }
        }
    }

    async fn pool_for(
        &self,
        tenant: TenantId,
        cluster: &str,
        database: &str,
        role: Role,
    ) -> Result<PgPool, PoolError> {
        let mut cache = self.cache.lock().await;

        if let Some(entry) = cache.get_mut(&(tenant, role)) {
            entry.last_used = Instant::now();
            return Ok(entry.pool.clone());
        }

        let options = self.clusters.options_for(cluster, database, role)?;
        let pool = PgPoolOptions::new()
            // Zero minimum is the whole point: an idle tenant costs nothing, and
            // with thousands of tenants most are idle at any instant.
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
            (tenant, role),
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
    /// holding a clone keeps working and the pool is torn down when the last
    /// user finishes. Closing here would break in-flight work to save memory,
    /// which is the wrong trade.
    fn evict_oldest(cache: &mut HashMap<(TenantId, Role), Cached>) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, cached)| cached.last_used)
            .map(|(key, _)| *key)
        {
            cache.remove(&oldest);
        }
    }

    /// Permits available in a lane. For the health endpoint, and for tests that
    /// assert bulkheads hold.
    #[must_use]
    pub fn available(&self, lane: Lane) -> usize {
        self.budget.available(lane)
    }

    /// Pool objects held — not open connections. With `min_connections(0)` most
    /// of these hold none.
    pub async fn cached_pool_count(&self) -> usize {
        self.cache.lock().await.len()
    }
}

// ---------------------------------------------------------------------------
// Permit-carrying connection handles
// ---------------------------------------------------------------------------

/// A pooled connection that holds its budget permit for exactly as long as it
/// lives. Dropping it returns both.
#[derive(Debug)]
pub struct Conn {
    inner: sqlx::pool::PoolConnection<Postgres>,
    _permit: OwnedSemaphorePermit,
}

impl Conn {
    pub(crate) const fn new(
        inner: sqlx::pool::PoolConnection<Postgres>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }
}

impl Deref for Conn {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// A transaction that holds its budget permit until it commits or rolls back.
///
/// Not `Drop`-committing: an unfinished transaction rolls back, which is the
/// safe default and matches sqlx.
#[derive(Debug)]
pub struct Tx {
    inner: sqlx::Transaction<'static, Postgres>,
    _permit: OwnedSemaphorePermit,
}

impl Tx {
    pub(crate) const fn new(
        inner: sqlx::Transaction<'static, Postgres>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            inner,
            _permit: permit,
        }
    }

    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

impl Deref for Tx {
    type Target = PgConnection;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Tx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
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

    fn tiny() -> PoolConfig {
        PoolConfig {
            interactive_operations: 2,
            client_operations: 2,
            background_operations: 1,
            ..PoolConfig::default()
        }
    }

    #[tokio::test]
    async fn an_unknown_cluster_is_refused() {
        let pools = pools(PoolConfig::default());
        let err = pools
            .handles(TenantId::new(), "does-not-exist", "spa_tenant_x")
            .await
            .expect_err("an unregistered cluster cannot be connected to");
        assert!(matches!(err, PoolError::UnknownCluster(_)));
    }

    /// Obtaining a handle must not spend budget — that is the whole reason the
    /// design scales past a few hundred requests per second.
    #[tokio::test]
    async fn taking_a_handle_costs_no_budget() {
        let pools = pools(tiny());
        for _ in 0..50 {
            pools
                .handles(TenantId::new(), "primary", "postgres")
                .await
                .expect("handles are free");
        }
        assert_eq!(pools.available(Lane::Interactive), 2);
        assert_eq!(pools.available(Lane::Client), 2);
    }

    #[tokio::test]
    async fn permits_are_returned_when_dropped() {
        let pools = pools(tiny());
        let first = pools.permit(Lane::Client).expect("within budget");
        let second = pools.permit(Lane::Client).expect("within budget");
        assert_eq!(pools.available(Lane::Client), 0);

        assert!(matches!(
            pools.permit(Lane::Client),
            Err(PoolError::Overloaded { lane: Lane::Client })
        ));

        drop(first);
        assert_eq!(pools.available(Lane::Client), 1);
        drop(second);
        assert_eq!(pools.available(Lane::Client), 2);
    }

    /// The bulkhead property: a flood of consumer traffic must leave the
    /// employee at the counter unaffected.
    #[tokio::test]
    async fn saturating_one_lane_does_not_starve_another() {
        let pools = pools(tiny());

        let flood: Vec<_> = (0..2)
            .map(|_| pools.permit(Lane::Client).expect("within budget"))
            .collect();
        assert_eq!(pools.available(Lane::Client), 0);
        assert!(pools.permit(Lane::Client).is_err());

        // Interactive is untouched.
        assert_eq!(pools.available(Lane::Interactive), 2);
        let counter = pools
            .permit(Lane::Interactive)
            .expect("client saturation must not starve interactive work");

        // And so is background.
        let worker = pools
            .permit(Lane::Background)
            .expect("client saturation must not starve background work");

        drop(flood);
        drop(counter);
        drop(worker);
    }

    #[tokio::test]
    async fn pools_are_reused_across_acquisitions_of_one_tenant() {
        let pools = pools(PoolConfig::default());
        let tenant = TenantId::new();

        pools
            .handles(tenant, "primary", "postgres")
            .await
            .expect("handles");
        pools
            .handles(tenant, "primary", "postgres")
            .await
            .expect("handles");

        assert_eq!(
            pools.cached_pool_count().await,
            1,
            "one write pool, and no read pool because no replica is configured"
        );
    }

    #[tokio::test]
    async fn a_configured_replica_gets_its_own_pool() {
        let clusters = registry()
            .with_replica("primary", "postgres://postgres@localhost/postgres")
            .expect("a valid URL parses");
        let pools = TenantPools::new(clusters, PoolConfig::default());

        let (_write, read) = pools
            .handles(TenantId::new(), "primary", "postgres")
            .await
            .expect("handles");
        assert!(
            read.is_some(),
            "a replica must produce a separate read pool"
        );
        assert_eq!(pools.cached_pool_count().await, 2);
    }

    /// **A replica attached to a name that is not a cluster is refused.**
    ///
    /// It used to be a silent no-op, which is the worst available answer: the
    /// deploy succeeds, every read goes to the primary, and the only symptom is
    /// a primary carrying twice the load somebody sized it for.
    #[test]
    fn a_replica_for_a_cluster_that_does_not_exist_is_an_error() {
        let refused = registry().with_replica("primry", "postgres://postgres@localhost/postgres");
        assert!(refused.is_err(), "a typo silently dropped the replica");
        assert!(
            refused.unwrap_err().to_string().contains("primry"),
            "the error has to name what was not found"
        );
    }

    /// **The replica seam, from the environment that actually configures it.**
    ///
    /// `TenantDb::read` has routed to a replica since Phase 1 and **no binary
    /// ever attached one** — five composition roots, each registering the
    /// primary and nothing else, so the whole read path was reachable only from
    /// the test above.
    ///
    #[test]
    fn the_environment_decides_whether_there_is_a_replica() {
        let url = Some("postgres://postgres@localhost/postgres");

        assert!(
            !ClusterRegistry::from_urls(url, None)
                .expect("a primary is enough")
                .has_replica("primary"),
            "a replica appeared from nowhere"
        );
        assert!(
            ClusterRegistry::from_urls(url, url)
                .expect("primary and replica")
                .has_replica("primary"),
            "PRIMARY_REPLICA_URL was read and then not used"
        );
        assert!(
            !ClusterRegistry::from_urls(url, Some("   "))
                .expect("blank is not an error")
                .has_replica("primary"),
            "a blank variable is how a compose file says `no replica`"
        );
        assert!(
            ClusterRegistry::from_urls(None, url).is_err(),
            "a replica with no primary is a deployment that cannot write"
        );
    }

    #[tokio::test]
    async fn the_pool_cache_is_bounded() {
        let pools = pools(PoolConfig {
            max_cached_pools: 3,
            ..PoolConfig::default()
        });

        for _ in 0..10 {
            pools
                .handles(TenantId::new(), "primary", "postgres")
                .await
                .expect("handles");
        }

        assert_eq!(
            pools.cached_pool_count().await,
            3,
            "touching many tenants in sequence must not grow the cache without bound"
        );
    }
}
