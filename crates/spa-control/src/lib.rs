//! The control plane: identities, memberships, tenants, entitlements, and the
//! [`TenantDb`] handle that is the only route to a tenant's data.
//!
//! # What lives here and what does not
//!
//! This plane answers three questions, all of them on the hot path of every
//! request:
//!
//! - *Who is this?* — identity
//! - *May they enter this tenant?* — membership
//! - *Which modules apply?* — entitlement
//!
//! It does **not** answer *what may they do here*. Fine-grained permission is
//! tenant-local and lives in the tenant's own database, next to the data it
//! governs. The split means no request ever joins across the two planes.
//!
//! # Persistence
//!
//! Normalized tables plus an append-only audit trail, not an event stream
//! (architecture decision D2). These records are small, highly relational, read
//! constantly, and must support cross-tenant reporting — none of which an event
//! log helps with. Provisioning workflows, which genuinely need resumable
//! state, are event-sourced separately.

mod auth;
mod cache;
mod fleet;
mod invitations;
mod leases;
mod members;
pub mod messages;
mod model;
mod placement;
mod pools;
mod provision;
mod roles;
mod tenant_db;

pub use auth::{
    AuthError, InvitationToken, SESSION_LIFETIME, Session, SessionToken, hash_password,
};
pub use fleet::{EventVersions, FleetPlan, TenantSchema};
pub use invitations::{
    Accepted, INVITATION_LIFETIME, Invitation, InvitationError, PendingInvitation,
};
pub use leases::WorkSchedule;
pub use members::{Member, MemberError};
pub use model::{
    Actor, EnabledModules, Entitlement, Identity, IdentityStatus, Membership, Scope, Tenant,
    TenantStatus,
};
pub use placement::{ClusterLoad, ClusterStatus, PlacementPolicy};
pub use pools::{ClusterRegistry, Conn, Lane, PoolConfig, PoolError, TenantPools, Tx};
pub use provision::{ModuleSetup, SignedUp as ProvisionedTenant};
pub use roles::{Access, Capability, Role, UnknownRole};
pub use tenant_db::{CommandError, TenantDb};

use spa_i18n::{Localize, Message, MessageArg, StaticCatalog};

/// This crate's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cache::TtlCache;
use spa_types::{IdentityId, MembershipId, ModuleId, TenantId};
use sqlx::PgPool;

/// How long entry-path lookups are cached.
///
/// Five seconds is the compromise: it removes ~99% of control-plane load at any
/// meaningful request rate, while bounding how long a suspension or revocation
/// can lag on a node that did not perform it. See [`cache`] for the full
/// argument.
const ENTRY_CACHE_TTL: Duration = Duration::from_secs(5);
const ENTRY_CACHE_CAPACITY: usize = 50_000;

/// Migrations for the control-plane database.
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations/control");

/// Why entry to a tenant was refused.
///
/// Distinct variants because the caller must be able to distinguish them:
/// "still provisioning" is a retry, "not a member" is not. What reaches an API
/// client is deliberately coarser — see the note on [`AccessError::NotAMember`].
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error("no such identity")]
    NoSuchIdentity,
    #[error("this identity is suspended")]
    IdentitySuspended,
    #[error("no such tenant")]
    NoSuchTenant,
    /// The tenant exists but cannot be entered right now — still provisioning,
    /// suspended, or deleted. Carries the status so a caller can tell a retry
    /// from a refusal.
    #[error("tenant is {status:?}, not active")]
    TenantNotActive { status: TenantStatus },
    /// No live membership joins this identity to this tenant.
    ///
    /// API responses must render this and [`AccessError::NoSuchTenant`]
    /// identically. Distinguishing them tells an attacker which tenant slugs
    /// exist, which is a free enumeration oracle.
    #[error("no membership for this identity in this tenant")]
    NotAMember,
    #[error(transparent)]
    Pool(#[from] PoolError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("stored data is invalid: {0}")]
    Corrupt(String),
    /// Every cluster is full, draining, or offline.
    ///
    /// An operational condition, not a user error: someone needs to bring
    /// capacity online. Carries the count so the alert says how bad it is.
    #[error("no cluster has capacity ({clusters_at_limit} at their limit)")]
    NoCapacity { clusters_at_limit: usize },
    #[error("the name {0:?} is already taken")]
    SlugTaken(String),
    /// A credential failed on a path that is not a login — signing up with an
    /// address that already has an account, most of all.
    #[error(transparent)]
    Auth(#[from] crate::AuthError),
}

/// Handle on the core database.
#[derive(Debug)]
pub struct ControlPlane {
    pool: PgPool,
    tenants: Arc<TenantPools>,
    identities: TtlCache<IdentityId, Option<Identity>>,
    tenants_cache: TtlCache<TenantId, Option<Tenant>>,
    /// A caller's role in a tenant. Cached as the parsed [`Role`], so
    /// authorization needs no second query.
    memberships: TtlCache<(IdentityId, TenantId), Option<Access>>,
    /// Whether an identity is platform staff.
    ///
    /// A separate cache because it is a separate question with a separate
    /// vocabulary: platform roles are `support`, `superadmin`, `billing`, and
    /// forcing them through [`Role`] would let "support" answer questions about
    /// what someone may do inside a tenant's books.
    platform: TtlCache<IdentityId, bool>,
    entitlements: TtlCache<TenantId, EnabledModules>,
    /// Entry-path cache hits and misses. A miss is a control-database round
    /// trip, so the ratio is the number that decides whether the control plane
    /// survives the request rate — worth exporting, not just asserting in tests.
    entry_hits: AtomicU64,
    entry_misses: AtomicU64,
}

impl ControlPlane {
    #[must_use]
    pub fn new(pool: PgPool, tenants: TenantPools) -> Self {
        Self {
            pool,
            tenants: Arc::new(tenants),
            identities: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            tenants_cache: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            memberships: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            platform: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            entitlements: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            entry_hits: AtomicU64::new(0),
            entry_misses: AtomicU64::new(0),
        }
    }

    /// `(hits, misses)` on the entry path since start.
    ///
    /// A miss is one control-database round trip. At 10,000 requests a second a
    /// 99% hit rate is the difference between 400 and 40,000 queries per second
    /// against a database that cannot be sharded.
    #[must_use]
    pub fn entry_cache_stats(&self) -> (u64, u64) {
        (
            self.entry_hits.load(Ordering::Relaxed),
            self.entry_misses.load(Ordering::Relaxed),
        )
    }

    fn hit(&self) {
        self.entry_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn miss(&self) {
        self.entry_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Drops every cached entry-path lookup.
    ///
    /// For tests, and for an operator who has changed something out of band and
    /// does not want to wait out the TTL.
    pub fn clear_caches(&self) {
        self.identities.clear();
        self.tenants_cache.clear();
        self.memberships.clear();
        self.platform.clear();
        self.entitlements.clear();
    }

    /// The core database. Control-plane queries only — this is not a route to
    /// tenant data.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    #[must_use]
    pub fn tenants(&self) -> &TenantPools {
        &self.tenants
    }

    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        MIGRATIONS.run(&self.pool).await
    }

    // -----------------------------------------------------------------------
    // The gate
    // -----------------------------------------------------------------------

    /// Opens a tenant's database for an identity, or refuses.
    ///
    /// Four checks, in an order chosen so the cheapest refusals happen first
    /// and no connection is spent on a request that will be denied:
    ///
    /// 1. the identity exists and is active
    /// 2. the tenant exists and is enterable
    /// 3. a live membership joins them
    /// 4. the connection budget has room
    ///
    /// Only then is a [`TenantDb`] minted. Because that type has no other
    /// constructor, every function taking one has been handed proof that all
    /// four passed.
    ///
    /// Platform staff do **not** get in this way, even superadmins. There is no
    /// `is_system` bypass; support access is [`Self::enter_for_support`], which
    /// is audited.
    pub async fn enter(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
        lane: Lane,
    ) -> Result<TenantDb, AccessError> {
        let identity = self
            .cached_identity(identity_id)
            .await?
            .ok_or(AccessError::NoSuchIdentity)?;
        if !identity.is_active() {
            return Err(AccessError::IdentitySuspended);
        }

        let tenant = self
            .cached_tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;
        if !tenant.is_enterable() {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        let access = self
            .cached_membership(identity_id, tenant_id)
            .await?
            .ok_or(AccessError::NotAMember)?;

        let mut db = self.open(&tenant, lane).await?;
        db.set_access(Some(access));
        Ok(db)
    }

    /// Opens a tenant on behalf of platform staff, recording who and why.
    ///
    /// Deliberately a separate method rather than a branch inside [`Self::enter`].
    /// Support access is a different act from a customer using their own system,
    /// and the audit trail has to say so — otherwise an engineer reading a
    /// tenant's ledger is indistinguishable from the tenant's owner doing it.
    ///
    /// The caller must have a live platform membership.
    pub async fn enter_for_support(
        &self,
        staff_id: IdentityId,
        tenant_id: TenantId,
        reason: &str,
    ) -> Result<TenantDb, AccessError> {
        let staff = self
            .cached_identity(staff_id)
            .await?
            .ok_or(AccessError::NoSuchIdentity)?;
        if !staff.is_active() {
            return Err(AccessError::IdentitySuspended);
        }
        if !self.cached_platform_membership(staff_id).await? {
            return Err(AccessError::NotAMember);
        }

        let tenant = self
            .cached_tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;
        // Suspended tenants are deliberately reachable for support: diagnosing
        // why a tenant was suspended is a normal reason to need access.
        if matches!(tenant.status, TenantStatus::Deleted) {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        self.record(
            Actor::identity(staff_id),
            "tenant.support_access",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "reason": reason }),
        )
        .await?;

        // Support access is interactive by definition — an engineer is waiting.
        self.open(&tenant, Lane::Interactive).await
    }

    /// Opens a tenant for background work: projections, the outbox, migrations.
    ///
    /// # Why this is not a bypass
    ///
    /// It takes **no identity**, and that is the whole safety argument. A
    /// request handler always has one, so it has no way to reach this path by
    /// accident and no way to use it to act as somebody. Nothing it returns can
    /// be attributed to a person, because no person was involved.
    ///
    /// It is also fixed to [`Lane::Background`], so however much work the fleet
    /// is doing it draws from its own bulkhead and cannot starve a customer's
    /// request.
    ///
    /// Unaudited, deliberately: a projection tick per tenant per interval would
    /// bury the audit trail that [`Self::enter_for_support`] exists to keep
    /// readable. What background work *did* is recorded where it belongs —
    /// checkpoints, outbox rows, and the tenant's own event log.
    pub async fn enter_for_maintenance(
        &self,
        tenant_id: TenantId,
    ) -> Result<TenantDb, AccessError> {
        let tenant = self
            .cached_tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;

        // A deleted tenant's database may be gone; a provisioning one has no
        // schema yet. Suspended tenants still need their projections driven —
        // suspension stops people using the system, not the system finishing
        // what it already accepted.
        if matches!(
            tenant.status,
            TenantStatus::Deleted | TenantStatus::Provisioning
        ) {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        self.open(&tenant, Lane::Background).await
    }

    /// Claims tenants that are due for a visit, for the length of one visit.
    ///
    /// One statement does the scheduling and the mutual exclusion together: it
    /// returns tenants whose `next_visit_at` has arrived and which no other
    /// worker holds, and marks them as `owner`'s until the lease lapses.
    ///
    /// `SKIP LOCKED` means two workers claiming at the same instant get disjoint
    /// sets rather than one waiting. A worker that dies mid-visit is recovered
    /// from by the lease expiring — there is nothing to detect and nothing to
    /// rebalance.
    ///
    /// A tenant this worker already holds is re-claimable, so renewing and
    /// claiming are the same call.
    pub async fn claim_tenants(
        &self,
        owner: &str,
        limit: i64,
        schedule: WorkSchedule,
    ) -> Result<Vec<Tenant>, AccessError> {
        let lease_millis = i64::try_from(schedule.lease.as_millis()).unwrap_or(i64::MAX);

        let rows = sqlx::query!(
            r#"
            UPDATE tenant
               SET worker_lease_owner = $1,
                   worker_lease_until = now() + ($3::BIGINT * INTERVAL '1 millisecond')
             WHERE id IN (
                 SELECT id
                   FROM tenant
                  WHERE status = 'active'
                    AND next_visit_at <= now()
                    AND (worker_lease_until IS NULL
                         OR worker_lease_until <= now()
                         OR worker_lease_owner = $1)
                  ORDER BY next_visit_at
                  LIMIT $2
                    FOR UPDATE SKIP LOCKED
             )
            RETURNING id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at, created_at
            "#,
            owner,
            limit,
            lease_millis,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                tenant_from_row(
                    row.id,
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.created_at,
                )
            })
            .collect()
    }

    /// Schedules the next visit to a tenant, and drops the lease.
    ///
    /// `after` is zero when the visit did work — there is more to do and it
    /// should be looked at again immediately — and
    /// [`WorkSchedule::next_idle_delay`] when it did not.
    pub async fn schedule_next_visit(
        &self,
        tenant_id: TenantId,
        after: Duration,
    ) -> Result<(), AccessError> {
        let millis = i64::try_from(after.as_millis()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE tenant
                SET next_visit_at      = now() + ($2::BIGINT * INTERVAL '1 millisecond'),
                    worker_lease_owner = NULL,
                    worker_lease_until = NULL
              WHERE id = $1",
            tenant_id.as_uuid(),
            millis,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks a tenant as having work waiting, so the next claim picks it up.
    ///
    /// The seam the push path attaches to: today the worker polls on an
    /// interval, and when the API can tell it directly that a tenant just wrote
    /// something, it does so by calling this. Polling becomes the floor rather
    /// than the mechanism, and nothing downstream changes.
    pub async fn request_visit(&self, tenant_id: TenantId) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE tenant SET next_visit_at = now()
              WHERE id = $1 AND next_visit_at > now()",
            tenant_id.as_uuid(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drops every lease this worker holds, without changing when the tenants
    /// are next due.
    ///
    /// Called on the way out. Nothing depends on it — the leases would lapse
    /// anyway — but releasing them means a rolling deploy hands work over in
    /// milliseconds instead of one lease interval.
    pub async fn release_leases(&self, owner: &str) -> Result<u64, AccessError> {
        let released = sqlx::query!(
            "UPDATE tenant
                SET worker_lease_owner = NULL, worker_lease_until = NULL
              WHERE worker_lease_owner = $1",
            owner,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(released)
    }

    async fn open(&self, tenant: &Tenant, lane: Lane) -> Result<TenantDb, AccessError> {
        let modules = self.cached_modules(tenant.id).await?;
        let (write, read) = self
            .tenants
            .handles(tenant.id, &tenant.cluster, &tenant.database_name)
            .await?;
        Ok(TenantDb::new(
            tenant.id,
            write,
            read,
            modules,
            Arc::clone(&self.tenants),
            lane,
        ))
    }

    // -----------------------------------------------------------------------
    // Cached lookups
    //
    // Each is a read-through: hit returns immediately, miss queries and stores.
    // Failures are never cached, so granting access takes effect at once while
    // revoking it is bounded by the TTL.
    // -----------------------------------------------------------------------

    async fn cached_identity(&self, id: IdentityId) -> Result<Option<Identity>, AccessError> {
        if let Some(hit) = self.identities.get(&id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.identity(id).await?;
        self.identities.put(id, fresh.clone());
        Ok(fresh)
    }

    async fn cached_tenant(&self, id: TenantId) -> Result<Option<Tenant>, AccessError> {
        if let Some(hit) = self.tenants_cache.get(&id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.tenant(id).await?;
        self.tenants_cache.put(id, fresh.clone());
        Ok(fresh)
    }

    /// `tenant` is `None` for a platform membership.
    /// The caller's role in a scope, or `None` if they have no live membership.
    ///
    /// Caching the *role* rather than a boolean is what lets authorization be
    /// decided without a second query. The staleness window is the same one
    /// documented in [`cache`]: a demotion takes up to the TTL to take effect,
    /// which is why revoking access outright also ends the session.
    async fn cached_membership(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
    ) -> Result<Option<Access>, AccessError> {
        let key = (identity_id, tenant_id);
        if let Some(hit) = self.memberships.get(&key) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.live_access(identity_id, tenant_id).await?;
        self.memberships.put(key, fresh.clone());
        Ok(fresh)
    }

    /// Whether this identity is platform staff.
    ///
    /// Existence only. What platform staff may do is decided by the path they
    /// take — [`Self::enter_for_support`] is audited and time-boxed — not by a
    /// role string, so parsing one would be inventing a vocabulary nothing
    /// reads.
    async fn cached_platform_membership(
        &self,
        identity_id: IdentityId,
    ) -> Result<bool, AccessError> {
        if let Some(hit) = self.platform.get(&identity_id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.platform_membership(identity_id).await?;
        self.platform.put(identity_id, fresh);
        Ok(fresh)
    }

    async fn cached_modules(&self, tenant_id: TenantId) -> Result<EnabledModules, AccessError> {
        if let Some(hit) = self.entitlements.get(&tenant_id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.enabled_modules(tenant_id).await?;
        self.entitlements.put(tenant_id, fresh.clone());
        Ok(fresh)
    }

    // -----------------------------------------------------------------------
    // Identities
    // -----------------------------------------------------------------------

    pub async fn create_identity(&self, actor: Actor) -> Result<Identity, AccessError> {
        let id = IdentityId::new();
        let row = sqlx::query!(
            r#"INSERT INTO identity (id) VALUES ($1)
               RETURNING id as "id: IdentityId", status, created_at"#,
            id.as_uuid(),
        )
        .fetch_one(&self.pool)
        .await?;

        self.record(
            actor,
            "identity.created",
            "identity",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;

        Ok(Identity {
            id: row.id,
            status: parse_identity_status(&row.status)?,
            created_at: row.created_at,
        })
    }

    pub async fn identity(&self, id: IdentityId) -> Result<Option<Identity>, AccessError> {
        let row = sqlx::query!(
            r#"SELECT id as "id: IdentityId", status, created_at
               FROM identity WHERE id = $1"#,
            id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(Identity {
                id: row.id,
                status: parse_identity_status(&row.status)?,
                created_at: row.created_at,
            })
        })
        .transpose()
    }

    /// Suspends an identity. Sessions are revoked separately — this makes the
    /// *next* entry fail, which is the check that matters.
    pub async fn suspend_identity(
        &self,
        id: IdentityId,
        reason: &str,
        actor: Actor,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE identity
                SET status = 'suspended', suspended_reason = $2, suspended_at = now()
              WHERE id = $1",
            id.as_uuid(),
            reason,
        )
        .execute(&self.pool)
        .await?;
        // Local invalidation: this node stops honouring the identity at once.
        // Other nodes converge within ENTRY_CACHE_TTL — a documented window,
        // see the `cache` module.
        self.identities.invalidate(&id);

        self.record(
            actor,
            "identity.suspended",
            "identity",
            &id.to_string(),
            serde_json::json!({ "reason": reason }),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Clusters
    // -----------------------------------------------------------------------

    /// Brings a cluster online.
    ///
    /// `dsn_env`/`replica_dsn_env` name the environment variables holding the
    /// connection strings — the DSN itself is never stored, so a control-plane
    /// backup carries no credentials.
    ///
    /// `max_active_tenants` is the limit that matters: open connections scale
    /// with concurrently-active tenants, so this is what stops a cluster running
    /// out of backends. `max_databases` is the storage-shaped secondary limit.
    pub async fn register_cluster(
        &self,
        name: &str,
        dsn_env: &str,
        replica_dsn_env: Option<&str>,
        max_active_tenants: i32,
        max_databases: i32,
        actor: Actor,
    ) -> Result<(), AccessError> {
        // Declarative, not create-once: registering names a cluster's
        // *configuration*, and an operator re-declaring it — to raise a
        // capacity, or to repoint the variable its credentials come from —
        // should not have to know whether this is the first time.
        //
        // `status` is deliberately untouched. It has its own command and means
        // something operational; re-registering a draining cluster must not
        // quietly put it back into service.
        sqlx::query!(
            "INSERT INTO cluster (name, dsn_env, replica_dsn_env, max_active_tenants, max_databases)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (name) DO UPDATE
                SET dsn_env            = EXCLUDED.dsn_env,
                    replica_dsn_env    = EXCLUDED.replica_dsn_env,
                    max_active_tenants = EXCLUDED.max_active_tenants,
                    max_databases      = EXCLUDED.max_databases",
            name,
            dsn_env,
            replica_dsn_env,
            max_active_tenants,
            max_databases,
        )
        .execute(&self.pool)
        .await?;

        self.record(
            actor,
            "cluster.registered",
            "cluster",
            name,
            serde_json::json!({
                "max_active_tenants": max_active_tenants,
                "max_databases": max_databases,
            }),
        )
        .await
    }

    /// Moves a cluster between accepting placements and not.
    ///
    /// `Draining` is the one to reach for when retiring hardware: it keeps
    /// serving existing tenants while taking no new ones.
    pub async fn set_cluster_status(
        &self,
        name: &str,
        status: ClusterStatus,
        actor: Actor,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE cluster SET status = $2 WHERE name = $1",
            name,
            status.as_str(),
        )
        .execute(&self.pool)
        .await?;

        self.record(
            actor,
            "cluster.status_changed",
            "cluster",
            name,
            serde_json::json!({ "status": status.as_str() }),
        )
        .await
    }

    /// What every cluster is currently carrying.
    ///
    /// Reads the `cluster_load` view, so the counts come from the tenant table
    /// rather than from a counter that can drift out of step with reality.
    pub async fn cluster_load(&self) -> Result<Vec<ClusterLoad>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT name as "name!", status as "status!", weight as "weight!",
                      max_active_tenants as "max_active_tenants!",
                      max_databases as "max_databases!",
                      live_tenants as "live_tenants!",
                      active_tenants as "active_tenants!"
                 FROM cluster_load
                ORDER BY name"#
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(ClusterLoad {
                    name: row.name,
                    status: ClusterStatus::parse(&row.status)?,
                    live_tenants: row.live_tenants,
                    active_tenants: row.active_tenants,
                    max_active_tenants: i64::from(row.max_active_tenants),
                    max_databases: i64::from(row.max_databases),
                    weight: row.weight,
                })
            })
            .collect()
    }

    /// Picks a cluster for a new tenant.
    pub async fn choose_cluster(&self, policy: PlacementPolicy) -> Result<String, AccessError> {
        let clusters = self.cluster_load().await?;
        policy.choose(&clusters).map_or_else(
            || {
                Err(AccessError::NoCapacity {
                    clusters_at_limit: clusters.len(),
                })
            },
            |chosen| Ok(chosen.name.clone()),
        )
    }

    // -----------------------------------------------------------------------
    // Tenants
    // -----------------------------------------------------------------------

    /// Registers a tenant on a cluster chosen by the placement policy.
    ///
    /// This is the normal path: signup does not know or care which machine it
    /// lands on. Use [`Self::register_tenant_on`] to pin one, which is for
    /// migrations and for an enterprise tenant with dedicated hardware.
    pub async fn register_tenant(
        &self,
        slug: &str,
        display_name: &str,
        policy: PlacementPolicy,
        actor: Actor,
    ) -> Result<Tenant, AccessError> {
        let cluster = self.choose_cluster(policy).await?;
        self.register_tenant_on(slug, display_name, &cluster, actor)
            .await
    }

    /// Registers a tenant on a named cluster, bypassing placement.
    ///
    /// The database itself is created by the provisioning workflow, which is why
    /// this does not connect to the cluster — it only records the intent.
    pub async fn register_tenant_on(
        &self,
        slug: &str,
        display_name: &str,
        cluster: &str,
        actor: Actor,
    ) -> Result<Tenant, AccessError> {
        let id = TenantId::new();
        let database_name = tenant_database_name(id);

        let row = sqlx::query!(
            r#"INSERT INTO tenant (id, slug, display_name, cluster, database_name)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING id as "id: TenantId", slug, display_name, status, cluster,
                         database_name, demo_expires_at, created_at"#,
            id.as_uuid(),
            slug,
            display_name,
            cluster,
            database_name,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match &e {
            // A taken slug is a normal outcome of self-service signup, not a
            // database failure. Surfacing it as one would render "something went
            // wrong" to someone who just needs to pick another name.
            sqlx::Error::Database(db) if db.constraint() == Some("tenant_slug_key") => {
                AccessError::SlugTaken(slug.to_owned())
            }
            _ => AccessError::Database(e),
        })?;

        self.record(
            actor,
            "tenant.registered",
            "tenant",
            &id.to_string(),
            serde_json::json!({ "slug": slug, "cluster": cluster }),
        )
        .await?;

        tenant_from_row(
            row.id,
            row.slug,
            row.display_name,
            &row.status,
            row.cluster,
            row.database_name,
            row.demo_expires_at,
            row.created_at,
        )
    }

    pub async fn tenant(&self, id: TenantId) -> Result<Option<Tenant>, AccessError> {
        let row = sqlx::query!(
            r#"SELECT id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at, created_at
               FROM tenant WHERE id = $1"#,
            id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            tenant_from_row(
                row.id,
                row.slug,
                row.display_name,
                &row.status,
                row.cluster,
                row.database_name,
                row.demo_expires_at,
                row.created_at,
            )
        })
        .transpose()
    }

    pub async fn tenant_by_slug(&self, slug: &str) -> Result<Option<Tenant>, AccessError> {
        let row = sqlx::query!(
            r#"SELECT id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at, created_at
               FROM tenant WHERE slug = $1"#,
            slug,
        )
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            tenant_from_row(
                row.id,
                row.slug,
                row.display_name,
                &row.status,
                row.cluster,
                row.database_name,
                row.demo_expires_at,
                row.created_at,
            )
        })
        .transpose()
    }

    /// Marks a tenant active. Called by the provisioning workflow once the
    /// database exists, is migrated, and is seeded — never before, or entry
    /// would succeed against a database with no schema.
    pub async fn activate_tenant(&self, id: TenantId, actor: Actor) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE tenant SET status = 'active', activated_at = now()
              WHERE id = $1 AND status = 'provisioning'",
            id.as_uuid(),
        )
        .execute(&self.pool)
        .await?;
        self.tenants_cache.invalidate(&id);

        self.record(
            actor,
            "tenant.activated",
            "tenant",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Memberships
    // -----------------------------------------------------------------------

    pub async fn grant_membership(
        &self,
        identity_id: IdentityId,
        scope: Scope,
        role: &str,
        actor: Actor,
    ) -> Result<MembershipId, AccessError> {
        let id = MembershipId::new();

        // # Why this is an upsert, and why its `WHERE` matters
        //
        // The unique constraint covers revoked rows, so a plain `INSERT` made
        // *removing* someone permanent: an employee who left and came back, or
        // anyone removed by mistake, could never be added again — and the
        // failure was a 500 that named nothing.
        //
        // The `WHERE membership.revoked_at IS NOT NULL` is the whole safety of
        // it. Reviving a revoked membership is this function's job; quietly
        // changing a *live* member's role is not, and without that clause this
        // would be a way around `change_role`'s last-owner guard.
        let revived = sqlx::query_scalar!(
            "INSERT INTO membership (id, identity_id, scope_kind, tenant_id, role)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT ON CONSTRAINT membership_is_unique_per_scope DO UPDATE
                SET role = EXCLUDED.role, revoked_at = NULL, created_at = now()
              WHERE membership.revoked_at IS NOT NULL
            RETURNING id",
            id.as_uuid(),
            identity_id.as_uuid(),
            scope.kind_str(),
            scope.tenant().map(TenantId::into_uuid),
            role,
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(id) = revived.map(MembershipId::from_uuid) else {
            // A live membership was already there. Idempotent success, and
            // deliberately without touching the role — callers that mean to
            // change one call `change_role`, which knows about last owners.
            return self
                .membership_id(identity_id, scope)
                .await?
                .ok_or_else(|| {
                    AccessError::Corrupt(
                        "a membership conflicted and then could not be found".to_owned(),
                    )
                });
        };
        // A grant or revocation must take effect now on this node, not after
        // the TTL. Both caches, because the scope decides which one holds it.
        match scope.tenant() {
            Some(tenant_id) => self.memberships.invalidate(&(identity_id, tenant_id)),
            None => self.platform.invalidate(&identity_id),
        }

        self.record(
            actor,
            "membership.granted",
            "identity",
            &identity_id.to_string(),
            serde_json::json!({
                "scope": scope.kind_str(),
                "tenant": scope.tenant().map(|t| t.to_string()),
                "role": role,
            }),
        )
        .await?;

        Ok(id)
    }

    /// The live membership joining an identity to a scope, if there is one.
    async fn membership_id(
        &self,
        identity_id: IdentityId,
        scope: Scope,
    ) -> Result<Option<MembershipId>, AccessError> {
        Ok(sqlx::query_scalar!(
            "SELECT id FROM membership
              WHERE identity_id = $1
                AND tenant_id IS NOT DISTINCT FROM $2
                AND revoked_at IS NULL",
            identity_id.as_uuid(),
            scope.tenant().map(TenantId::into_uuid),
        )
        .fetch_optional(&self.pool)
        .await?
        .map(MembershipId::from_uuid))
    }

    /// Takes a membership away. Returns whether there was one to take.
    pub async fn revoke_membership(
        &self,
        identity_id: IdentityId,
        scope: Scope,
        actor: Actor,
    ) -> Result<bool, AccessError> {
        let revoked = sqlx::query!(
            "UPDATE membership SET revoked_at = now()
              WHERE identity_id = $1
                AND tenant_id IS NOT DISTINCT FROM $2
                AND revoked_at IS NULL",
            identity_id.as_uuid(),
            scope.tenant().map(TenantId::into_uuid),
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        // Per-module exceptions go with the membership. Removing somebody takes
        // away everything about their access, so re-adding them later starts
        // from their new role rather than from a rule nobody remembers setting.
        sqlx::query!(
            "DELETE FROM membership_module_role r
              USING membership m
              WHERE r.membership_id = m.id
                AND m.identity_id = $1
                AND m.tenant_id IS NOT DISTINCT FROM $2",
            identity_id.as_uuid(),
            scope.tenant().map(TenantId::into_uuid),
        )
        .execute(&self.pool)
        .await?;

        // A grant or revocation must take effect now on this node, not after
        // the TTL. Both caches, because the scope decides which one holds it.
        match scope.tenant() {
            Some(tenant_id) => self.memberships.invalidate(&(identity_id, tenant_id)),
            None => self.platform.invalidate(&identity_id),
        }

        self.record(
            actor,
            "membership.revoked",
            "identity",
            &identity_id.to_string(),
            serde_json::json!({
                "scope": scope.kind_str(),
                "tenant": scope.tenant().map(|t| t.to_string()),
            }),
        )
        .await?;

        Ok(revoked > 0)
    }

    /// Everything that decides what somebody may do here: their tenant-wide
    /// role and wherever the tenant said something different per module.
    ///
    /// One round trip, because it is on the entry path of every request and the
    /// overrides are almost always empty.
    async fn live_access(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
    ) -> Result<Option<Access>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT m.role as "role!", r.module_id, r.role as "module_role?"
                 FROM membership m
                 LEFT JOIN membership_module_role r ON r.membership_id = m.id
                WHERE m.identity_id = $1 AND m.tenant_id = $2 AND m.revoked_at IS NULL"#,
            identity_id.as_uuid(),
            tenant_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        let Some(first) = rows.first() else {
            return Ok(None);
        };

        // A stored role this build does not know is an error, not a default.
        // Defaulting down locks someone out silently; defaulting up lets them
        // in silently.
        let mut access = Access::new(parse_role(&first.role)?);

        for row in &rows {
            let (Some(module), Some(role)) = (row.module_id.as_ref(), row.module_role.as_ref())
            else {
                continue;
            };
            let module = ModuleId::new(module.clone())
                .map_err(|e| AccessError::Corrupt(format!("module_role.module_id: {e}")))?;
            access.overrides.push((module, parse_role(role)?));
        }

        Ok(Some(access))
    }

    async fn platform_membership(&self, identity_id: IdentityId) -> Result<bool, AccessError> {
        let found = sqlx::query_scalar!(
            "SELECT 1 FROM membership
              WHERE identity_id = $1 AND scope_kind = 'platform' AND revoked_at IS NULL",
            identity_id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    /// Every tenant this identity may enter. The tenant switcher's query.
    pub async fn tenants_for_identity(
        &self,
        identity_id: IdentityId,
    ) -> Result<Vec<Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT t.id as "id: TenantId", t.slug, t.display_name, t.status, t.cluster,
                      t.database_name, t.demo_expires_at, t.created_at
                 FROM tenant t
                 JOIN membership m ON m.tenant_id = t.id
                WHERE m.identity_id = $1
                  AND m.revoked_at IS NULL
                  AND t.status <> 'deleted'
                ORDER BY t.display_name"#,
            identity_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                tenant_from_row(
                    row.id,
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.created_at,
                )
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Entitlements
    // -----------------------------------------------------------------------

    /// Switches a module on. Idempotent: enabling an already-live module is a
    /// no-op rather than an error, because the caller is usually a workflow that
    /// may be retried.
    pub async fn enable_module(
        &self,
        tenant_id: TenantId,
        module: &ModuleId,
        actor: Actor,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "INSERT INTO entitlement (tenant_id, module_id) VALUES ($1, $2)
             ON CONFLICT (tenant_id, module_id)
             DO UPDATE SET disabled_at = NULL, enabled_at = now()",
            tenant_id.as_uuid(),
            module.as_str(),
        )
        .execute(&self.pool)
        .await?;
        self.entitlements.invalidate(&tenant_id);

        self.record(
            actor,
            "module.enabled",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "module": module.as_str() }),
        )
        .await
    }

    /// Switches a module off. **Never drops its tables** — a tenant who
    /// downgrades and returns expects their data. Storage is reclaimed only on
    /// explicit deletion, after an export.
    pub async fn disable_module(
        &self,
        tenant_id: TenantId,
        module: &ModuleId,
        actor: Actor,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE entitlement SET disabled_at = now()
              WHERE tenant_id = $1 AND module_id = $2 AND disabled_at IS NULL",
            tenant_id.as_uuid(),
            module.as_str(),
        )
        .execute(&self.pool)
        .await?;
        self.entitlements.invalidate(&tenant_id);

        self.record(
            actor,
            "module.disabled",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "module": module.as_str() }),
        )
        .await
    }

    pub async fn enabled_modules(
        &self,
        tenant_id: TenantId,
    ) -> Result<EnabledModules, AccessError> {
        let rows = sqlx::query_scalar!(
            "SELECT module_id FROM entitlement
              WHERE tenant_id = $1 AND disabled_at IS NULL",
            tenant_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        // Constructed through `ModuleId::new` rather than decoded straight into
        // the newtype: validation must apply to data coming *out* of the
        // database too, since that is where values written by older versions of
        // the system arrive.
        let modules = rows
            .into_iter()
            .map(|raw| {
                ModuleId::new(raw.clone())
                    .map_err(|e| AccessError::Corrupt(format!("entitlement.module_id: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(EnabledModules::new(modules))
    }

    // -----------------------------------------------------------------------
    // Audit
    // -----------------------------------------------------------------------

    /// Appends an audit entry. The table refuses `UPDATE` and `DELETE` at the
    /// database level, so this is the only way its contents change.
    pub async fn record(
        &self,
        actor: Actor,
        action: &str,
        subject_type: &str,
        subject_id: &str,
        detail: serde_json::Value,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "INSERT INTO audit_entry
                (actor_identity_id, on_behalf_of_identity_id, action, subject_type, subject_id, detail)
             VALUES ($1, $2, $3, $4, $5, $6)",
            actor.identity.map(IdentityId::into_uuid),
            actor.on_behalf_of.map(IdentityId::into_uuid),
            action,
            subject_type,
            subject_id,
            detail,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Localization
// ---------------------------------------------------------------------------

impl Localize for AccessError {
    /// What a *user* is told, which is not what an operator is told.
    ///
    /// `NoSuchTenant` and `NotAMember` collapse to one message on purpose: a
    /// distinct "no such tenant" would let an attacker enumerate tenant slugs by
    /// watching which error comes back. The `Display` impl keeps them apart for
    /// logs, where the distinction is useful and the audience is trusted.
    fn message(&self) -> Message {
        match self {
            Self::NoSuchIdentity => Message::new(messages::NO_SUCH_IDENTITY),
            Self::IdentitySuspended => Message::new(messages::IDENTITY_SUSPENDED),
            Self::NoSuchTenant | Self::NotAMember => Message::new(messages::ACCESS_DENIED),
            Self::TenantNotActive { status } => match status {
                // Provisioning is a retry, and saying so saves a support ticket.
                TenantStatus::Provisioning => Message::new(messages::TENANT_PROVISIONING),
                _ => Message::new(messages::TENANT_UNAVAILABLE),
            },
            Self::Pool(e) => e.message(),
            // Deliberately says nothing about clusters: a signup form has no
            // business reporting our capacity. The count reaches operators
            // through `messages::CLUSTERS_AT_LIMIT` and the log line.
            Self::NoCapacity { .. } => Message::new(messages::NO_CAPACITY),
            Self::SlugTaken(slug) => {
                Message::new(messages::SLUG_TAKEN).with("slug", MessageArg::text(slug))
            }
            Self::Auth(e) => e.message(),
            // A database failure or corrupt row is never described to a user.
            // They get "something went wrong"; the detail goes to the log.
            Self::Database(_) | Self::Corrupt(_) => Message::new(messages::INTERNAL),
        }
    }
}

impl Localize for PoolError {
    fn message(&self) -> Message {
        match self {
            Self::Overloaded { .. } => Message::new(messages::OVERLOADED),
            // An unconfigured cluster is our misconfiguration, not the user's
            // problem to understand.
            Self::UnknownCluster(_) | Self::Connect(_) => Message::new(messages::INTERNAL),
        }
    }
}

// ---------------------------------------------------------------------------
// Row conversion
// ---------------------------------------------------------------------------

/// The database name for a tenant.
///
/// Derived from the id rather than the slug: slugs are user-chosen and can be
/// renamed, and a database whose name drifts from its tenant is a debugging
/// nightmare. Lowercase hex keeps it inside the identifier rules the `tenant`
/// table's CHECK constraint enforces.
fn tenant_database_name(id: TenantId) -> String {
    format!("spa_tenant_{}", id.as_uuid().simple())
}

fn parse_identity_status(raw: &str) -> Result<IdentityStatus, AccessError> {
    match raw {
        "active" => Ok(IdentityStatus::Active),
        "suspended" => Ok(IdentityStatus::Suspended),
        other => Err(AccessError::Corrupt(format!(
            "identity.status: unknown value {other:?}"
        ))),
    }
}

fn parse_tenant_status(raw: &str) -> Result<TenantStatus, AccessError> {
    match raw {
        "provisioning" => Ok(TenantStatus::Provisioning),
        "active" => Ok(TenantStatus::Active),
        "suspended" => Ok(TenantStatus::Suspended),
        "deleted" => Ok(TenantStatus::Deleted),
        other => Err(AccessError::Corrupt(format!(
            "tenant.status: unknown value {other:?}"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
/// A stored role this build must recognise.
fn parse_role(raw: &str) -> Result<Role, AccessError> {
    raw.parse::<Role>()
        .map_err(|e| AccessError::Corrupt(e.to_string()))
}

#[expect(
    clippy::too_many_arguments,
    reason = "one parameter per column of the row it decodes; a struct here would be `Tenant` with different validation"
)]
pub(crate) fn tenant_from_row(
    id: TenantId,
    slug: String,
    display_name: String,
    status: &str,
    cluster: String,
    database_name: String,
    demo_expires_at: Option<spa_types::Timestamp>,
    created_at: spa_types::Timestamp,
) -> Result<Tenant, AccessError> {
    Ok(Tenant {
        id,
        slug,
        display_name,
        status: parse_tenant_status(status)?,
        cluster,
        database_name,
        demo_expires_at,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_database_names_satisfy_the_schema_constraint() {
        // The `tenant` table CHECKs `^[a-z][a-z0-9_]{0,62}$`; a name that fails
        // it would be rejected at insert, so generate one that always passes.
        let name = tenant_database_name(TenantId::new());
        assert!(name.starts_with("spa_tenant_"));
        assert!(
            name.len() <= 63,
            "exceeds Postgres identifier limit: {name}"
        );
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "not a safe identifier: {name}"
        );
    }

    #[test]
    fn unknown_stored_statuses_are_reported_not_guessed() {
        // Law L6: failures stop. A status this build doesn't know about means
        // data from a newer version, and treating it as "active" would be the
        // worst possible guess.
        assert!(parse_identity_status("active").is_ok());
        assert!(matches!(
            parse_identity_status("something_new"),
            Err(AccessError::Corrupt(_))
        ));
        assert!(matches!(
            parse_tenant_status("something_new"),
            Err(AccessError::Corrupt(_))
        ));
    }
}
