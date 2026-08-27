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
use std::sync::Arc;
use std::time::{Duration, Instant};

use erp_tenant::{Lane, PoolError};
use erp_types::TenantId;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

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
    /// How many prepared statements sqlx caches per connection. **Zero disables
    /// preparation**, which is what a transaction pooler needs.
    ///
    /// # Why this is configuration and not a constant
    ///
    /// sqlx prepares by default and caches the handle on the connection. A
    /// transaction pooler hands out a *different* backend for each transaction,
    /// so a cached handle refers to a statement the new backend never parsed.
    ///
    /// Poolers answer this differently — Supavisor parses SQL and broadcasts
    /// `PREPARE` across its pool; `PgBouncer` 1.21+ tracks protocol-level prepared
    /// statements up to a configured limit — and both of those are the pooler's
    /// business, not this crate's. What this crate owes a deployment is a knob
    /// it can turn without a rebuild, so that "we put a pooler in front" is a
    /// variable rather than a patch.
    ///
    /// Non-zero by default, because there is no pooler by default and
    /// preparation is worth real throughput.
    pub statement_cache_capacity: usize,
}

impl PoolConfig {
    /// The budgets this deployment was given, or the defaults.
    ///
    /// # Why these are configuration and not constants
    ///
    /// They were compiled in, and the sum of them is a claim about a database
    /// this crate has never seen. Four processes each holding a private
    /// 400-permit budget against a 200-connection server is not a bug anybody
    /// wrote — it is what happens when a per-process number is chosen once and a
    /// deployment later runs four of them.
    ///
    /// **What to set them against changes the moment a pooler goes in front.**
    /// Without one, the sum across every process must fit the server's
    /// `max_connections`. With one, the server-side number belongs to the pooler
    /// and these become client connections, which are cheap — Supavisor served
    /// 250,000 of them over 400 to the database.
    ///
    /// [`Self::demand`] and `report_budget` are what make the number visible
    /// rather than implied.
    #[must_use]
    pub fn from_env() -> Self {
        let default = Self::default();
        let read = |name: &str, fallback: usize| {
            std::env::var(name)
                .ok()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
                .unwrap_or(fallback)
        };
        Self {
            interactive_operations: read("POOL_INTERACTIVE", default.interactive_operations),
            client_operations: read("POOL_CLIENT", default.client_operations),
            background_operations: read("POOL_BACKGROUND", default.background_operations),
            max_connections_per_tenant: u32::try_from(read(
                "POOL_PER_TENANT",
                default.max_connections_per_tenant as usize,
            ))
            .unwrap_or(default.max_connections_per_tenant)
            .max(1),
            statement_cache_capacity: read(
                "POOL_STATEMENT_CACHE",
                default.statement_cache_capacity,
            ),
            ..default
        }
    }

    /// The most connections **one process** can hold open at once.
    ///
    /// Two ceilings, and the smaller wins:
    ///
    /// - a connection can only be *executing* while it holds a lane permit, so
    ///   the sum of the lane budgets is one bound;
    /// - a connection belongs to a tenant's pool, so `active_tenants ×
    ///   max_connections_per_tenant` is the other.
    ///
    /// Measured: 300 concurrent requests against **one** tenant held 8
    /// connections across two API replicas, because the second bound was 4 per
    /// process and the first never came near.
    #[must_use]
    pub fn demand(&self, active_tenants: usize) -> usize {
        let by_lane =
            self.interactive_operations + self.client_operations + self.background_operations;
        let by_pool = active_tenants.saturating_mul(self.max_connections_per_tenant as usize);
        by_lane.min(by_pool)
    }
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
            statement_cache_capacity: 100,
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
    /// A route that is **guaranteed not to be a transaction pooler**.
    ///
    /// See [`Role::Direct`]. Falls back to the primary when absent, which is
    /// correct for every deployment that has no pooler — the primary *is*
    /// direct then.
    direct: Option<PgConnectOptions>,
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
                direct: None,
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

    /// Adds a route that is guaranteed not to be a transaction pooler.
    ///
    /// Only needed when the primary URL *is* one. See [`Role::Direct`].
    pub fn with_direct(mut self, name: &str, url: &str) -> Result<Self, sqlx::Error> {
        let direct: PgConnectOptions = url.parse()?;
        let cluster = self.clusters.get_mut(name).ok_or_else(|| {
            sqlx::Error::Configuration(
                format!("no cluster named `{name}` to attach a direct route to").into(),
            )
        })?;
        cluster.direct = Some(direct);
        Ok(self)
    }

    /// The connection options for a cluster's **direct** route, for callers that
    /// open their own connection rather than going through a pool.
    ///
    /// Provisioning is the only one: `CREATE DATABASE` cannot run inside a
    /// transaction, so it cannot run through a transaction pooler either.
    pub fn direct_options(&self, cluster: &str) -> Result<PgConnectOptions, PoolError> {
        self.options_for(cluster, "postgres", Role::Direct)
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
            std::env::var("PRIMARY_DIRECT_URL").ok().as_deref(),
        )
    }

    /// [`Self::from_env`] with the environment already read.
    ///
    /// Separate so the decision — required, optional, blank-means-absent — can
    /// be tested without `set_var`. Editing the process environment from a test
    /// needs `unsafe`, which this workspace denies, and races every other test
    /// in the same binary regardless.
    fn from_urls(
        primary: Option<&str>,
        replica: Option<&str>,
        direct: Option<&str>,
    ) -> Result<Self, sqlx::Error> {
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
        let registry = match replica.filter(|url| !url.trim().is_empty()) {
            Some(url) => {
                tracing::info!("read replica configured for cluster `primary`");
                registry.with_replica("primary", url)?
            }
            // Not a warning. A single-node deployment is a supported shape, and
            // `read()` falls back to the primary — which is why it is always
            // safe to call and why adding a replica later is configuration.
            None => registry,
        };

        match direct.filter(|url| !url.trim().is_empty()) {
            Some(url) => {
                tracing::info!(
                    "direct route configured for cluster `primary`; provisioning \
                     and schema installs will bypass the pooler"
                );
                registry.with_direct("primary", url)
            }
            // No pooler in front, so the primary is already direct.
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
            Role::Direct => entry.direct.as_ref().unwrap_or(&entry.primary),
        };
        Ok(base.clone().database(database))
    }

    fn has_replica(&self, cluster: &str) -> bool {
        self.clusters
            .get(cluster)
            .is_some_and(|c| c.replica.is_some())
    }
}

/// Which route to a cluster an operation needs.
///
/// # Why `Direct` exists
///
/// A transaction pooler — `PgBouncer`, Supavisor — hands a client a *different*
/// server connection for each transaction. That is what lets 400 server
/// connections serve 250,000 clients, and it means **session state does not
/// survive between transactions**.
///
/// Most of this system is already fine with that: the projection hot path sets
/// `SET LOCAL search_path`, which is transaction-scoped, and there is no
/// `LISTEN`, no session advisory lock and no temp table anywhere in it.
///
/// What is *not* fine is provisioning. `CREATE DATABASE` cannot run inside a
/// transaction at all, and installing a module's schema is a sequence — set the
/// search path, run the DDL, set it back — whose steps depend on running on the
/// same backend. Through a transaction pooler those steps can land on three
/// different ones, and the DDL creates its tables in whatever schema the third
/// one happened to have.
///
/// So those paths ask for `Direct` and are handed a route configured never to be
/// pooled. Supabase ships exactly this shape: every project gets a pooler
/// connection string *and* a direct one.
///
/// **When no pooler is configured, `Direct` is the primary** — which is why
/// adopting one later is a variable rather than a rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Write,
    Read,
    Direct,
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

    /// Says at start-up what this process could demand and what the server
    /// allows, because neither number was written down anywhere before.
    ///
    /// Deliberately a **log line and not a refusal**. One process cannot know
    /// how many siblings a deployment runs, so it cannot compute the fleet's
    /// total — and refusing to start on a number it had to guess would be worse
    /// than the silence it replaces. What it can do is state its own share and
    /// the server's limit next to each other, so the arithmetic is somebody's
    /// job rather than nobody's.
    ///
    /// When a pooler is in front, `max_connections` read here is the pooler's
    /// client limit, not Postgres's — which is the right number to compare
    /// against anyway.
    pub async fn report_budget(&self, cluster: &str) {
        let ceiling = self.config.demand(usize::MAX);
        let options = match self.maintenance_options(cluster) {
            Ok(options) => options,
            Err(e) => {
                tracing::warn!(error = %e, "cannot reach {cluster} to report its connection budget");
                return;
            }
        };

        let allowed: Option<i64> = match sqlx::PgPool::connect_with(options).await {
            Ok(pool) => {
                let value =
                    sqlx::query_scalar::<_, String>("SELECT current_setting('max_connections')")
                        .fetch_one(&pool)
                        .await
                        .ok()
                        .and_then(|raw| raw.parse().ok());
                pool.close().await;
                value
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot reach {cluster} to report its connection budget");
                None
            }
        };

        match allowed {
            Some(max) if i64::try_from(ceiling).unwrap_or(i64::MAX) > max => tracing::warn!(
                cluster,
                this_process_at_most = ceiling,
                server_max_connections = max,
                per_tenant = self.config.max_connections_per_tenant,
                "this process alone can demand more connections than the server \
                 allows. Every other process draws from the same limit, so size \
                 POOL_INTERACTIVE / POOL_CLIENT / POOL_BACKGROUND against it, or \
                 put a pooler in front."
            ),
            Some(max) => tracing::info!(
                cluster,
                this_process_at_most = ceiling,
                server_max_connections = max,
                per_tenant = self.config.max_connections_per_tenant,
                "connection budget"
            ),
            None => tracing::info!(
                cluster,
                this_process_at_most = ceiling,
                per_tenant = self.config.max_connections_per_tenant,
                "connection budget; the server's limit could not be read"
            ),
        }
    }

    /// Connect options for a cluster's **direct** route, with no database chosen.
    ///
    /// For provisioning and fleet migration, which need a connection to
    /// `postgres` before the tenant's database exists. Not a pool: these are
    /// one-shot.
    ///
    /// [`Role::Direct`] and not `Write`, and it is the single line that makes
    /// every DDL path in this system pooler-safe. `CREATE DATABASE` cannot run
    /// inside a transaction, and installing a module's schema is a sequence
    /// whose steps share a `search_path` — neither survives a transaction
    /// pooler handing out a different backend per statement. With no pooler
    /// configured this is the primary, so nothing changes until something does.
    pub(crate) fn maintenance_options(&self, cluster: &str) -> Result<PgConnectOptions, PoolError> {
        self.clusters.options_for(cluster, "postgres", Role::Direct)
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

        let options = self
            .clusters
            .options_for(cluster, database, role)?
            .statement_cache_capacity(self.config.statement_cache_capacity);
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

/// The shared fleet's answer to *may this operation run now?*
///
/// One process serves many tenants here, so the permit comes from a budget they
/// all share. A tenant running as its own deployment (D15) supplies a different
/// implementation and never links this type.
impl erp_tenant::Budget for TenantPools {
    fn permit(&self, lane: Lane) -> Result<OwnedSemaphorePermit, PoolError> {
        self.budget.try_acquire(lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_tenant::Budget as _;

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
            .handles(TenantId::new(), "does-not-exist", "erp_tenant_x")
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
            !ClusterRegistry::from_urls(url, None, None)
                .expect("a primary is enough")
                .has_replica("primary"),
            "a replica appeared from nowhere"
        );
        assert!(
            ClusterRegistry::from_urls(url, url, None)
                .expect("primary and replica")
                .has_replica("primary"),
            "PRIMARY_REPLICA_URL was read and then not used"
        );
        assert!(
            !ClusterRegistry::from_urls(url, Some("   "), None)
                .expect("blank is not an error")
                .has_replica("primary"),
            "a blank variable is how a compose file says `no replica`"
        );
        assert!(
            ClusterRegistry::from_urls(None, url, None).is_err(),
            "a replica with no primary is a deployment that cannot write"
        );
    }

    /// **`Direct` falls back to the primary, which is what makes a pooler a
    /// configuration change rather than a rewrite.**
    ///
    /// Provisioning and schema installs ask for `Direct` unconditionally. On a
    /// deployment with no pooler that has to be the primary, or every one of
    /// them breaks the day this is merged; on a deployment with one it has to be
    /// the bypass, or `CREATE DATABASE` and `SET search_path` sequences land on
    /// a transaction pooler that cannot serve them.
    #[test]
    fn direct_is_the_primary_until_a_pooler_says_otherwise() {
        let pooled = "postgres://postgres@pooler:6543/postgres";
        let straight = "postgres://postgres@primary:5432/postgres";

        let none = ClusterRegistry::from_urls(Some(pooled), None, None).expect("primary only");
        assert_eq!(
            none.options_for("primary", "d", Role::Direct)
                .expect("options")
                .get_port(),
            6543,
            "with no direct route configured, direct must be the primary"
        );

        let split = ClusterRegistry::from_urls(Some(pooled), None, Some(straight)).expect("both");
        assert_eq!(
            split
                .options_for("primary", "d", Role::Direct)
                .expect("options")
                .get_port(),
            5432,
            "provisioning would have gone through the pooler"
        );
        assert_eq!(
            split
                .options_for("primary", "d", Role::Write)
                .expect("options")
                .get_port(),
            6543,
            "ordinary writes must still go through the pooler"
        );
    }

    #[test]
    fn a_direct_route_for_a_cluster_that_does_not_exist_is_an_error() {
        assert!(
            registry()
                .with_direct("primry", "postgres://postgres@localhost/postgres")
                .is_err()
        );
    }

    /// **The two ceilings, and which one binds.**
    ///
    /// This is the arithmetic nothing in the system stated before, and getting
    /// it backwards is how a 400-permit budget went unnoticed against a
    /// 200-connection server.
    #[test]
    fn demand_is_the_smaller_of_the_lane_budget_and_the_pool_cap() {
        let config = PoolConfig::default(); // 100 + 240 + 60 = 400, 4 per tenant

        // Few tenants: the per-tenant pool binds, and this is the measured case
        // — 300 concurrent requests against one tenant held 4 per process.
        assert_eq!(config.demand(1), 4);
        assert_eq!(config.demand(10), 40);

        // Many tenants: the lane budget binds, and it is the number that has to
        // fit `max_connections` across every process in the deployment.
        assert_eq!(config.demand(100), 400);
        assert_eq!(config.demand(usize::MAX), 400, "and it never exceeds it");

        // A deployment behind a pooler turns preparation off; nothing else about
        // the arithmetic changes.
        let pooled = PoolConfig {
            statement_cache_capacity: 0,
            ..PoolConfig::default()
        };
        assert_eq!(pooled.demand(100), config.demand(100));
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
