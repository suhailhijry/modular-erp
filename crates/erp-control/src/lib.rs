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
mod dead_letters;
pub mod domains;
mod fleet;
mod invitations;
mod keys;
mod leases;
pub mod mail;
mod members;
pub mod messages;
mod model;
mod otp;
mod passwords;
mod placement;
mod pools;
mod provision;
mod second_factor;
pub mod shared;
pub use second_factor::{
    ENROLMENT_LIFETIME_SECONDS, Enrolled, Enrolment, FactorRequiredBy, RECOVERY_CODES, ResetError,
};
mod shutdown;
pub use shutdown::shutdown_signal;
mod signup;
mod staff;
pub use staff::{PlatformPower, PlatformRole, StaffError, StaffMember};
pub mod totp;

pub use auth::{
    AuthError, EnrolmentToken, InvitationToken, SESSION_LIFETIME, Session, SessionToken,
    SignupToken, hash_password,
};
/// Re-exported so the control plane's callers are unchanged by the split.
///
/// A **module** must not reach these through here — it depends on `erp-tenant`
/// directly, which is what stops it linking the fleet (D15). `tests/boundary.rs`
/// is what enforces that.
pub use erp_tenant::{
    Access, Budget, Capability, CommandError, Conn, EnabledModules, Lane, ModuleSetup, PoolError,
    Role, TenantDb, Tx, UnknownRole,
};
pub use fleet::{
    EventVersions, FleetPlan, MIGRATION_FLOOR, ReadModelVersions, SealingPlan, TenantSchema,
    UPGRADE_FROM_RELEASE,
};
pub use invitations::{
    Accepted, INVITATION_LIFETIME, Invitation, InvitationError, PendingInvitation,
};
pub use keys::{ApiKey, BadScope, KeyContext, KeyScope, ROTATION_OVERLAP, Secret};
pub use leases::{Claimed, WorkSchedule};
pub use members::{Member, MemberError};
pub use model::{
    Actor, AuditEntry, Entitlement, Identity, IdentityStatus, Membership, Scope, Tenant,
    TenantStatus,
};
pub use otp::{
    CODE_LIFETIME_SECONDS, MAX_ATTEMPTS as MAX_CODE_ATTEMPTS, OtpError,
    REQUEST_INTERVAL_SECONDS as CODE_REQUEST_INTERVAL_SECONDS, Requested,
    normalise as normalise_phone,
};
pub use passwords::{
    MAX_ATTEMPTS as RESET_MAX_ATTEMPTS, PasswordError, RESET_INTERVAL_SECONDS,
    RESET_LIFETIME_SECONDS,
};
pub use placement::{
    CapacityError, ClusterLoad, ClusterStatus, PlacementPolicy, declared_capacity,
};
pub use pools::{ClusterRegistry, PoolConfig, TenantPools};
pub use provision::SignedUp as ProvisionedTenant;
pub use provision::{
    ORPHAN_GRACE_SECONDS, PROVISIONING_GRACE_SECONDS, Unclaimed, orphan_age_seconds_for_tests,
};
pub use signup::{
    Confirmed, PendingSignup, REQUEST_INTERVAL, SIGNUP_LIFETIME, SignupError, SignupRequest,
};

use erp_i18n::{Composite, Localize, Message, MessageArg, StaticCatalog};

/// Only the codes declared in this crate.
static OWN_CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// Everything a control-plane failure can say, in every supported language.
///
/// Composed rather than a single table because `PoolError` moved to `erp-tenant`
/// with `TenantDb` (D15), and it still renders as `OVERLOADED`. A catalog that
/// covered only this crate's own codes would leave that error as a bare code —
/// which is what `tests/localization.rs` refuses.
pub static CATALOG: Composite = Composite::new(&[&OWN_CATALOG, &erp_tenant::CATALOG]);

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cache::TtlCache;
pub use domains::{DnsProver, DomainProver, NoResolver, ProofError, record_name, record_value};
use erp_types::{Cursor, IdentityId, MembershipId, ModuleId, NotACursor, Page, TenantId};
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
    /// A status change asked of a tenant in a different status: suspending one
    /// that is not active, reinstating one that is not suspended, activating
    /// one that is not provisioning. Refused rather than accepted as a no-op,
    /// so a repeat is never recorded as though it had done something.
    #[error("tenant is {status:?}; this needs it {expected:?}")]
    WrongTenantStatus {
        status: TenantStatus,
        expected: TenantStatus,
    },
    /// A suspension with no reason, or one over 500 characters. The rule is the
    /// `tenant_suspension_is_complete` constraint; this is its refusal, named.
    #[error("a suspension needs a reason of 1 to 500 characters")]
    SuspensionReason,
    /// No live membership joins this identity to this tenant.
    ///
    /// API responses must render this and [`AccessError::NoSuchTenant`]
    /// identically. Distinguishing them tells an attacker which tenant slugs
    /// exist, which is a free enumeration oracle.
    #[error("no membership for this identity in this tenant")]
    NotAMember,
    /// **This tenant requires a second factor and this member has none.**
    ///
    /// Deliberately *not* rendered as `NotAMember`, unlike most access
    /// failures: the caller is a member, they are signed in, and the only thing
    /// they can do about it is enrol. A 404 would tell them to give up. It
    /// leaks nothing an attacker could use — reaching this at all means already
    /// holding a session and a live membership.
    #[error("this tenant requires a second factor and this account has none")]
    SecondFactorRequired,
    /// **No platform role that may do this** — not staff at all, or staff whose
    /// role does not include the power. One variant for both: the answer to
    /// either is to ask a superadmin, and it names the power so they know what
    /// to ask for.
    #[error("only platform staff who may {} can do this", .0.as_str())]
    StaffOnly(PlatformPower),
    /// Staff whose role may, and who have no second factor. Every platform
    /// door refuses them until they enrol one.
    #[error("platform staff need a second factor and this account has none")]
    StaffSecondFactorRequired,
    /// A domain this tenant has not claimed.
    #[error("{0} has not been claimed by this tenant")]
    DomainNotClaimed(String),
    /// The record that proves the domain is not published, or says something
    /// else. Carries what to publish.
    #[error("{domain} is not proved: publish {record} TXT {expected}")]
    DomainNotProved {
        domain: String,
        record: String,
        expected: String,
    },
    /// The resolver could not be asked. A retry, not a refusal.
    #[error("the domain could not be looked up: {0}")]
    DomainProofUnavailable(String),
    /// Not `https://<host>[:port]`.
    #[error("{0} is not an origin this API licenses")]
    NotAnOrigin(String),
    /// An origin whose host is not the domain it was claimed under.
    #[error("{origin} is not under {domain}")]
    OriginOutsideDomain { origin: String, domain: String },
    #[error(transparent)]
    Pool(#[from] PoolError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("stored data is invalid: {0}")]
    Corrupt(String),
    /// The tenant is further behind than this build will upgrade from.
    ///
    /// D17: two majors, and upgrades are **sequential**. A build that will
    /// migrate from anything would be a build whose upgrade path was never
    /// tested — the matrix grows with the length of the support window, and
    /// only a single hop can be exhaustively covered.
    ///
    /// Carries the release to install first, because an operator told "too old"
    /// with no next step will guess, and guessing is what this refuses.
    #[error(
        "this tenant is at migration {at}, below the floor of {floor} this build \
         upgrades from; install {install_first} first (D17: upgrades are sequential)"
    )]
    TooOldToUpgrade {
        at: i64,
        floor: i64,
        install_first: &'static str,
    },
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
    /// A caller's platform role, cached as the parsed [`PlatformRole`] so a
    /// platform door needs no second query.
    ///
    /// A separate cache because it is a separate question with a separate
    /// vocabulary: forcing platform roles through [`Role`] would let "support"
    /// answer questions about what someone may do inside a tenant's books.
    platform: TtlCache<IdentityId, Option<PlatformRole>>,
    entitlements: TtlCache<TenantId, EnabledModules>,
    /// Projection groups known to be at or past a build's read model, per
    /// tenant. **Only that answer is kept** — see [`Self::read_model_behind`].
    read_models: TtlCache<(TenantId, &'static str), ()>,
    /// The origins a tenant's public API answers to, from `tenant_origin`.
    ///
    /// Cached on the same terms as everything else on the entry path: a
    /// cross-origin request asks this once per request, and asking the control
    /// database every time would put CORS on the hot path of the very surface
    /// that is expected to be flooded.
    origins: TtlCache<TenantId, Arc<[String]>>,
    /// Which tenant a custom host names, by the host. Cleared whenever any
    /// tenant's domains change; hosts are few and the lookup is on every
    /// request to one.
    hosts: TtlCache<String, Option<Tenant>>,
    /// How a domain is proved. The system's DNS in production; a test's
    /// stand-in otherwise.
    prover: Arc<dyn DomainProver>,
    /// The caches every node shares, when this deployment has any.
    ///
    /// `None` is a supported shape and means exactly the behaviour this system
    /// had before it existed: sessions from Postgres on every request, and an
    /// invalidation that reaches only the node that wrote it. See
    /// [`shared`](crate::shared).
    shared: Option<crate::shared::Shared>,
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
            read_models: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            origins: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            hosts: TtlCache::new(ENTRY_CACHE_TTL, ENTRY_CACHE_CAPACITY),
            prover: match DnsProver::from_system() {
                Ok(dns) => Arc::new(dns),
                Err(e) => {
                    tracing::warn!(error = %e, "no DNS resolver; domains cannot be proved here");
                    Arc::new(NoResolver)
                }
            },
            shared: None,
            entry_hits: AtomicU64::new(0),
            entry_misses: AtomicU64::new(0),
        }
    }

    /// The same control plane, sharing its session cache and its invalidations
    /// with every other node.
    #[must_use]
    pub fn sharing(mut self, shared: crate::shared::Shared) -> Self {
        self.shared = Some(shared);
        self
    }

    /// How domains are proved. Tests hand in a resolver that answers what they
    /// say; a deployment keeps the system's DNS.
    #[must_use]
    pub fn with_prover(mut self, prover: Arc<dyn DomainProver>) -> Self {
        self.prover = prover;
        self
    }

    /// Drops a key locally **and tells every other node to**.
    ///
    /// Every `invalidate` call in this crate goes through here. The local drop
    /// is immediate and unconditional; the broadcast is best-effort, because a
    /// failure to publish leaves the other nodes on the TTL window that was the
    /// only behaviour before this existed.
    async fn forget(&self, what: crate::shared::Invalidate) {
        use crate::shared::Invalidate;
        match what {
            Invalidate::Identity(id) => self.identities.invalidate(&id),
            Invalidate::Tenant(id) => self.tenants_cache.invalidate(&id),
            Invalidate::Membership { identity, tenant } => {
                self.memberships.invalidate(&(identity, tenant));
            }
            Invalidate::Platform(id) => self.platform.invalidate(&id),
            Invalidate::Entitlements(id) => self.entitlements.invalidate(&id),
            Invalidate::Origins(id) => {
                self.origins.invalidate(&id);
                self.hosts.clear();
            }
        }
        if let Some(shared) = &self.shared {
            shared.publish(&what).await;
        }
    }

    /// Applies an invalidation that arrived from another node. Local only —
    /// re-publishing it would be a loop.
    pub fn apply_invalidation(&self, what: &crate::shared::Invalidate) {
        use crate::shared::Invalidate;
        match what {
            Invalidate::Identity(id) => self.identities.invalidate(id),
            Invalidate::Tenant(id) => self.tenants_cache.invalidate(id),
            Invalidate::Membership { identity, tenant } => {
                self.memberships.invalidate(&(*identity, *tenant));
            }
            Invalidate::Platform(id) => self.platform.invalidate(id),
            Invalidate::Entitlements(id) => self.entitlements.invalidate(id),
            Invalidate::Origins(id) => {
                self.origins.invalidate(id);
                self.hosts.clear();
            }
        }
    }

    /// The shared cache, if this deployment has one.
    #[must_use]
    pub const fn shared(&self) -> Option<&crate::shared::Shared> {
        self.shared.as_ref()
    }

    /// The tenant pools, for a composition root that wants to report on them.
    #[must_use]
    pub fn pools(&self) -> &TenantPools {
        &self.tenants
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
        self.read_models.clear();
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
        let (tenant, access) = self.admitted(identity_id, tenant_id, true).await?;
        let mut db = self.open(&tenant, lane).await?;
        db.set_access(Some(access));
        Ok(db)
    }

    /// **What this identity may do in this tenant, whatever its status** —
    /// every check [`Self::enter`] makes but whether the tenant is serving, and
    /// no connection.
    ///
    /// For what the control plane keeps *about* a tenant, which never needed
    /// its database: its audit trail above all. `enter` answers a suspended
    /// tenant 503, and its owner is the one person who must still read why
    /// (decision 12 of 2026-09-11). What the role then permits is
    /// [`Access::allows`]'s to say, as it is behind `enter`.
    pub async fn admit(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
    ) -> Result<Access, AccessError> {
        Ok(self.admitted(identity_id, tenant_id, false).await?.1)
    }

    /// `enter`'s checks, in `enter`'s order; `serving` is whether a tenant
    /// that is not active is refused.
    async fn admitted(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
        serving: bool,
    ) -> Result<(Tenant, Access), AccessError> {
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
        if serving && !tenant.is_enterable() {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        let access = self
            .cached_membership(identity_id, tenant_id)
            .await?
            .ok_or(AccessError::NotAMember)?;

        // **Entry to this tenant, and nothing else.** The session stays valid
        // and the person's other tenants stay reachable: an owner turning this
        // on asked to protect their own business, not to sign somebody out of
        // somebody else's. Costs nothing — `tenant` came from the cache above.
        if tenant.requires_second_factor && !self.has_second_factor(identity_id).await? {
            return Err(AccessError::SecondFactorRequired);
        }

        Ok((tenant, access))
    }

    /// Opens a tenant on behalf of platform staff, recording who and why.
    ///
    /// Deliberately a separate method rather than a branch inside [`Self::enter`].
    /// Support access is a different act from a customer using their own system,
    /// and the audit trail has to say so — otherwise an engineer reading a
    /// tenant's ledger is indistinguishable from the tenant's owner doing it.
    ///
    /// The caller must pass [`Self::staff_may`] for
    /// [`PlatformPower::EnterForSupport`]: support or superadmin, with a second
    /// factor. Billing suspends tenants and never reads their books.
    pub async fn enter_for_support(
        &self,
        staff_id: IdentityId,
        tenant_id: TenantId,
        reason: &str,
    ) -> Result<TenantDb, AccessError> {
        self.staff_may(staff_id, PlatformPower::EnterForSupport)
            .await?;

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

        // audit-only: entering changes nothing in the control plane, so there
        // is no transaction for the entry to share; it is written before the
        // door opens, and a failure to write it keeps the door shut.
        let mut conn = self.pool.acquire().await?;
        self.record(
            &mut conn,
            Actor::identity(staff_id),
            Some(tenant_id),
            "tenant.support_access",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "reason": reason }),
        )
        .await?;
        drop(conn);

        // Support access is interactive by definition — an engineer is waiting.
        self.open(&tenant, Lane::Interactive).await
    }

    /// Opens a tenant for background work: projections, the outbox, the jobs.
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
        // schema yet. A suspended one is not refused here, and nothing runs for
        // it anyway: the worker, this door's caller, never claims one
        // (`claim_tenants`) and stops a visit whose tenant is suspended under
        // it (`renew_lease`). One still *suspending* is claimed, and this is
        // the door its drain jobs come through. The fleet migrator and module
        // refresh do bring a suspended tenant's schema current, and
        // `reseal_fleet` its secrets, but through their own direct
        // connections, not through here.
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

    /// Opens a tenant for one of **its customers**, who has no account here.
    ///
    /// The booking site, the order form, the thing a shop's own customers touch.
    ///
    /// # Why this is not a bypass, and how it differs from maintenance
    ///
    /// It takes **no identity**, so nothing it returns can be attributed to a
    /// person and no capability check can pass: `TenantDb::role()` is `None`,
    /// and a check against `None` refuses. A public handler therefore cannot
    /// reach a guarded command even by mistake — it has to call the module
    /// function directly, which is a visible act rather than an omission.
    ///
    /// It differs from [`Self::enter_for_maintenance`] in the two ways that
    /// matter:
    ///
    /// - **`Lane::Client`, not `Lane::Background`.** Somebody is waiting, so it
    ///   must not yield the way a projection tick does — and it must not draw
    ///   from the interactive lane either, because a bot hammering a booking
    ///   form would then starve the counter staff serving people in the shop.
    ///   That is the whole reason the lane exists, and this is its first caller.
    /// - **The tenant must be enterable.** A suspended tenant's public booking
    ///   page must go dark, and so must its payment gateway's callbacks — the
    ///   settle sweep answers those after reinstatement.
    pub async fn enter_for_the_public(&self, tenant_id: TenantId) -> Result<TenantDb, AccessError> {
        let tenant = self
            .cached_tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;
        if !tenant.is_enterable() {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        self.open(&tenant, Lane::Client).await
    }

    // -----------------------------------------------------------------------
    // Cross-origin access to a tenant's public API — Phase 17
    // -----------------------------------------------------------------------

    /// Whether a browser at this origin may call this tenant's public API.
    ///
    /// **Never a wildcard, and never a suffix match.** The stored origin is
    /// compared whole: `https://salon.com` does not admit
    /// `https://salon.com.attacker.example`, which is what a naive
    /// `ends_with` would do and is the single most common way this check is
    /// written wrong.
    ///
    /// A tenant with no origins recorded is a tenant with no public site, and
    /// answers no cross-origin request at all. That is the safe default and it
    /// is the one every tenant starts in.
    pub async fn allows_origin(
        &self,
        tenant_id: TenantId,
        origin: &str,
    ) -> Result<bool, AccessError> {
        let origin = origin.trim().to_lowercase();
        // `contains`, deliberately: it is whole-string equality and cannot be
        // mistaken for a prefix or suffix test the way a hand-written closure
        // can. See the module docs on `erp_web::cors`.
        Ok(self.cached_origins(tenant_id).await?.contains(&origin))
    }

    /// Every origin this tenant answers, for a settings screen.
    pub async fn origins(&self, tenant_id: TenantId) -> Result<Vec<String>, AccessError> {
        Ok(self.cached_origins(tenant_id).await?.to_vec())
    }

    async fn cached_origins(&self, tenant_id: TenantId) -> Result<Arc<[String]>, AccessError> {
        if let Some(hit) = self.origins.get(&tenant_id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();

        // **Only verified domains license an origin.** The join is the check:
        // an unverified domain has rows here and licenses nothing, so the row
        // can be written the moment a tenant asks and start working the moment
        // they prove it, with no second write to forget.
        let rows: Vec<String> = sqlx::query_scalar!(
            r#"SELECT o.origin as "origin!"
                 FROM tenant_origin o
                 JOIN tenant_domain d
                   ON d.tenant = o.tenant AND d.domain = o.domain
                WHERE o.tenant = $1 AND d.verified_at IS NOT NULL
                ORDER BY o.origin"#,
            tenant_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        let fresh: Arc<[String]> = rows.into();
        self.origins.put(tenant_id, Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Records a domain a tenant says they own, and the token that proves it.
    ///
    /// Nothing is licensed by this on its own — see [`Self::verify_domain`].
    /// Sending the same domain again returns the existing token rather than
    /// minting a new one, because a tenant who has already published a TXT
    /// record must not be told to publish a different one.
    pub async fn claim_domain(
        &self,
        tenant_id: TenantId,
        domain: &str,
        actor: Actor,
    ) -> Result<String, AccessError> {
        let domain = domain.trim().to_lowercase();
        let token = crate::auth::verification_token().map_err(AccessError::Auth)?;
        let mut tx = self.pool.begin().await?;
        let existing: Option<String> = sqlx::query_scalar!(
            "INSERT INTO tenant_domain (tenant, domain, verification_token)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant, domain) DO UPDATE
                 SET domain = EXCLUDED.domain
             RETURNING verification_token",
            tenant_id.as_uuid(),
            domain,
            token,
        )
        .fetch_optional(&mut *tx)
        .await?;

        let token = existing.unwrap_or(token);
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "tenant.domain_claimed",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "domain": domain }),
        )
        .await?;
        tx.commit().await?;
        Ok(token)
    }

    /// **Proves a claimed domain by the record the tenant was told to publish.**
    ///
    /// Looks up `_erp-challenge.<domain>` and compares; only a match sets
    /// `verified_at`. Already-proved is the same answer. An absent or wrong
    /// record is [`AccessError::DomainNotProved`], which carries the record to
    /// publish; a resolver that cannot answer is
    /// [`AccessError::DomainProofUnavailable`], which is a retry, not a refusal.
    /// The first version set `verified_at` on request and checked nothing.
    pub async fn verify_domain(
        &self,
        tenant_id: TenantId,
        domain: &str,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let domain = domain.trim().to_lowercase();
        let claimed = sqlx::query!(
            r#"SELECT verification_token as "token!", verified_at
                 FROM tenant_domain WHERE tenant = $1 AND domain = $2"#,
            tenant_id.as_uuid(),
            domain,
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AccessError::DomainNotClaimed(domain.clone()))?;
        if claimed.verified_at.is_some() {
            return Ok(());
        }
        let name = record_name(&domain);
        let records = self
            .prover
            .txt_records(&name)
            .await
            .map_err(|e| AccessError::DomainProofUnavailable(e.to_string()))?;
        if !domains::proves(&records, &claimed.token) {
            return Err(AccessError::DomainNotProved {
                domain,
                record: name,
                expected: record_value(&claimed.token),
            });
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE tenant_domain SET verified_at = now()
              WHERE tenant = $1 AND domain = $2 AND verified_at IS NULL",
            tenant_id.as_uuid(),
            domain,
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "tenant.domain_verified",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "domain": domain, "record": name }),
        )
        .await?;
        tx.commit().await?;
        // After the commit, so a read racing this cannot refill the cache
        // with the row as it was.
        self.forget(crate::shared::Invalidate::Origins(tenant_id))
            .await;
        Ok(())
    }

    /// **Licenses an origin under a proved domain.**
    ///
    /// The origin has to be `https://<host>[:port]` and nothing else, the host
    /// has to be the domain or under it, and the domain has to be this tenant's
    /// and proved. The first version lowercased the string and stored it; once
    /// CORS serves authenticated routes an entry here is the whole tenant, so
    /// every one of those is checked.
    pub async fn allow_origin(
        &self,
        tenant_id: TenantId,
        domain: &str,
        origin: &str,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let domain = domain.trim().to_lowercase();
        let origin = origin.trim().to_lowercase();
        let host = domains::origin_host(&origin)
            .ok_or_else(|| AccessError::NotAnOrigin(origin.clone()))?;
        if !domains::is_under(&host, &domain) {
            return Err(AccessError::OriginOutsideDomain { origin, domain });
        }
        let claimed = sqlx::query_scalar!(
            "SELECT verified_at FROM tenant_domain WHERE tenant = $1 AND domain = $2",
            tenant_id.as_uuid(),
            domain,
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AccessError::DomainNotClaimed(domain.clone()))?;
        if claimed.is_none() {
            return Err(AccessError::DomainNotProved {
                domain: domain.clone(),
                record: record_name(&domain),
                expected: format!("{}<token>", domains::RECORD_PREFIX),
            });
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO tenant_origin (tenant, origin, domain)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant, origin) DO NOTHING",
            tenant_id.as_uuid(),
            origin,
            domain,
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "tenant.origin_allowed",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "origin": origin, "domain": domain }),
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Origins(tenant_id))
            .await;
        Ok(())
    }

    /// Withdraws one origin. Takes effect across the fleet within the entry
    /// cache's TTL, and at once on the node that did it.
    pub async fn revoke_origin(
        &self,
        tenant_id: TenantId,
        origin: &str,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let origin = origin.trim().to_lowercase();
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "DELETE FROM tenant_origin WHERE tenant = $1 AND origin = $2",
            tenant_id.as_uuid(),
            origin,
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "tenant.origin_revoked",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "origin": origin }),
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Origins(tenant_id))
            .await;
        Ok(())
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
    /// # A claimed tenant is not due again until its lease lapses
    ///
    /// `next_visit_at` is pushed to the end of the lease **here**, not at the end
    /// of the visit. The first version left it where it was and let a worker
    /// re-claim tenants it already held, on the argument that renewing and
    /// claiming could be one call. What that actually did was hand the worker's
    /// own in-flight tenants straight back to it on the next loop — `next_visit_at`
    /// was still in the past and the lease was its own — so one due tenant filled
    /// every concurrency slot with visits of itself, and two of those visits
    /// could both `fetch` a saved-card charge, both see nothing, and both charge.
    /// Renewing is [`Self::renew_lease`] now, an explicit call from inside the
    /// visit, and a claim is a claim.
    pub async fn claim_tenants(
        &self,
        owner: &str,
        limit: i64,
        schedule: WorkSchedule,
    ) -> Result<Vec<Claimed>, AccessError> {
        let lease_millis = i64::try_from(schedule.lease.as_millis()).unwrap_or(i64::MAX);

        let rows = sqlx::query!(
            r#"
            UPDATE tenant
               SET worker_lease_owner = $1,
                   worker_lease_until = now() + ($3::BIGINT * INTERVAL '1 millisecond'),
                   next_visit_at      = now() + ($3::BIGINT * INTERVAL '1 millisecond')
             WHERE id IN (
                 SELECT id
                   FROM tenant
                  WHERE status IN ('active', 'suspending')
                    AND next_visit_at <= now()
                    AND (worker_lease_until IS NULL OR worker_lease_until <= now())
                  ORDER BY next_visit_at
                  LIMIT $2
                    FOR UPDATE SKIP LOCKED
             )
            RETURNING id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at,
                      requires_second_factor, created_at, idle_visits
            "#,
            owner,
            limit,
            lease_millis,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let idle_visits = row.idle_visits;
                tenant_from_row(
                    row.id,
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.requires_second_factor,
                    row.created_at,
                )
                .map(|tenant| Claimed {
                    tenant,
                    idle_visits,
                })
            })
            .collect()
    }

    /// Schedules the next visit to a tenant, and drops the lease.
    ///
    /// `after` is zero when the visit did work — there is more to do and it
    /// should be looked at again immediately — and
    /// [`WorkSchedule::next_idle_delay`] when it did not.
    /// `worked` decides whether the tenant's idle streak continues or resets,
    /// and the streak is what the next delay is computed from. Written in the
    /// same statement as `next_visit_at`, because a count that disagreed with
    /// the schedule it produced would be worse than no count at all.
    /// **Keeps hold of a tenant this worker is still working on.**
    ///
    /// Called between jobs inside a visit. Extends both the lease and
    /// `next_visit_at` by one lease length, and only if this owner still holds
    /// the tenant: `false` means the lease lapsed and somebody else may have it,
    /// and the right answer to that is to stop rather than to keep going beside
    /// them. A visit that outlives its lease without renewing is exactly the
    /// concurrent-visit race `claim_tenants` describes, from the other side.
    ///
    /// `false` also means the tenant stopped being visited — suspended since
    /// the visit began, or moved from `suspending` to `suspended` by another
    /// visit's drain. `claim_tenants` would not have claimed it, and a visit
    /// already under way must not run the rest of its jobs either, saved-card
    /// charges among them, for a tenant nothing should run for. A tenant that
    /// became `suspending` mid-visit is still ours: the visit's remaining jobs
    /// are judged by [`crate::TenantStatus::is_visited`]'s caller, the worker,
    /// which reads the status it claimed the tenant under.
    pub async fn renew_lease(
        &self,
        tenant_id: TenantId,
        owner: &str,
        lease: Duration,
    ) -> Result<bool, AccessError> {
        let lease_millis = i64::try_from(lease.as_millis()).unwrap_or(i64::MAX);
        let renewed = sqlx::query!(
            "UPDATE tenant
                SET worker_lease_until = now() + ($3::BIGINT * INTERVAL '1 millisecond'),
                    next_visit_at      = now() + ($3::BIGINT * INTERVAL '1 millisecond')
              WHERE id = $1
                AND worker_lease_owner = $2
                AND worker_lease_until > now()
                AND status IN ('active', 'suspending')",
            tenant_id.as_uuid(),
            owner,
            lease_millis,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(renewed == 1)
    }

    pub async fn schedule_next_visit(
        &self,
        tenant_id: TenantId,
        after: Duration,
        worked: bool,
    ) -> Result<(), AccessError> {
        let millis = i64::try_from(after.as_millis()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE tenant
                SET next_visit_at      = now() + ($2::BIGINT * INTERVAL '1 millisecond'),
                    worker_lease_owner = NULL,
                    worker_lease_until = NULL,
                    idle_visits        = CASE WHEN $3 THEN 0
                                              -- Saturating, so a tenant left
                                              -- alone for years does not
                                              -- overflow the column.
                                              ELSE least(idle_visits + 1, 1000) END
              WHERE id = $1",
            tenant_id.as_uuid(),
            millis,
            worked,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// **Requires — or stops requiring — a second factor of this tenant's
    /// members.**
    ///
    /// Turning it on is refused unless the person doing it has enrolled one
    /// themselves. One rule, no special cases, and it guarantees that at least
    /// one person can still get in: an owner who could switch this on from an
    /// unprotected account would be one click from locking the business out of
    /// its own books, at which point the only way back is platform support.
    ///
    /// Turning it **off** carries no such condition. Somebody has to be able to
    /// undo this, and requiring a second factor to remove the requirement is
    /// the trap it exists to prevent.
    ///
    /// # Errors
    /// [`AccessError::SecondFactorRequired`] if the caller is switching it on
    /// without one, or the database's own errors.
    pub async fn set_second_factor_requirement(
        &self,
        tenant_id: TenantId,
        by: IdentityId,
        required: bool,
    ) -> Result<(), AccessError> {
        if required && !self.has_second_factor(by).await? {
            return Err(AccessError::SecondFactorRequired);
        }
        sqlx::query!(
            "UPDATE tenant SET requires_second_factor = $2 WHERE id = $1",
            tenant_id.as_uuid(),
            required,
        )
        .execute(&self.pool)
        .await?;

        // **The tenant row is cached and `enter` reads it from there**, so a
        // requirement nobody forgets is a requirement nobody enforces.
        self.forget(crate::shared::Invalidate::Tenant(tenant_id))
            .await;
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

    /// Drops every lease this worker holds and makes those tenants due now.
    ///
    /// Called on the way out. Nothing depends on it — the leases would lapse
    /// anyway — but releasing them means a rolling deploy hands work over in
    /// milliseconds instead of one lease interval. A claim pushes
    /// `next_visit_at` to the end of the lease, so a release has to pull it back
    /// or the handover would wait out the lease after all; a visit cut short by
    /// shutdown is due immediately, which is what `Visit::reschedule` says too.
    pub async fn release_leases(&self, owner: &str) -> Result<u64, AccessError> {
        let released = sqlx::query!(
            "UPDATE tenant
                SET worker_lease_owner = NULL,
                    worker_lease_until = NULL,
                    next_visit_at      = least(next_visit_at, now())
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
            Arc::clone(&self.tenants) as Arc<dyn erp_tenant::Budget>,
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

    /// What an identity's membership says it may do in a tenant, through the
    /// entry cache. `None` is "no live membership".
    ///
    /// The membership **alone**: not whether the identity is active, nor the
    /// tenant's second-factor rule. [`Self::admit`] is [`Self::enter`]'s whole
    /// answer bar the tenant's status, and is what a door asks.
    pub async fn access(
        &self,
        identity: IdentityId,
        tenant: TenantId,
    ) -> Result<Option<Access>, AccessError> {
        self.cached_membership(identity, tenant).await
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

    /// This identity's platform role, or `None` if they are not staff.
    ///
    /// What the role permits is [`PlatformRole::may`]'s to say, and
    /// [`Self::staff_may`] is the only caller.
    async fn cached_platform_role(
        &self,
        identity_id: IdentityId,
    ) -> Result<Option<PlatformRole>, AccessError> {
        if let Some(hit) = self.platform.get(&identity_id) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let fresh = self.platform_role(identity_id).await?;
        self.platform.put(identity_id, fresh);
        Ok(fresh)
    }

    /// **The first of `wanted`'s groups whose tables in this tenant are not at
    /// the version beside it** — older, or newer than this build — with the
    /// version they are, or `None`.
    ///
    /// For the request path, which refuses a module's routes while this says
    /// anything (decision 7 of 2026-09-11): numbers served from a shape this
    /// build does not project are numbers nobody can vouch for, and a newer
    /// shape is one it never has. A group with no checkpoint row has no tables
    /// to be stale; the route that needs them fails on its own, loudly.
    ///
    /// # Why only "current" is cached
    ///
    /// Behind is the state that has to end the moment it can: a rebuild swaps
    /// the new tables in, in another process, and nothing here hears of it. So
    /// a behind answer is never kept — every request for that module reads the
    /// checkpoint again, and the first after the swap is served. That is the
    /// same rule every entry cache follows (a refusal is not stored, so access
    /// granted takes effect at once), and it is what makes the cache need no
    /// invalidation on a swap. Current is kept for the entry TTL, which bounds
    /// the one way a group goes backwards: a restore, or a migrator rolled back.
    ///
    /// Not counted as an entry hit or miss: a miss here costs the tenant's
    /// database a query, not the control plane one.
    pub async fn read_model_behind(
        &self,
        db: &TenantDb,
        wanted: &[(&'static str, i16)],
    ) -> Result<Option<(&'static str, i16)>, AccessError> {
        let tenant = db.tenant();
        let unknown: Vec<&str> = wanted
            .iter()
            .filter(|(group, _)| self.read_models.get(&(tenant, *group)).is_none())
            .map(|(group, _)| *group)
            .collect();
        if unknown.is_empty() {
            return Ok(None);
        }

        let mut conn = db.read().await?;
        let rows = sqlx::query!(
            "SELECT group_name, read_model_version FROM projection_checkpoint
              WHERE group_name = ANY($1)",
            &unknown as &[&str],
        )
        .fetch_all(&mut *conn)
        .await?;
        drop(conn);

        let mut behind = None;
        for (group, version) in wanted {
            if !unknown.contains(group) {
                continue;
            }
            match rows.iter().find(|row| row.group_name == *group) {
                // **`!=`, not `<`** — the projection runner's rule, for the
                // request path. It was `<`, so during a rolling deploy a pod
                // still on the old build served tables the migrator had
                // already swapped to the new shape, by rules that no longer
                // described them. Newer is as unservable as older; only this
                // build's own version is.
                Some(row) if row.read_model_version != *version => {
                    behind.get_or_insert((*group, row.read_model_version));
                }
                _ => self.read_models.put((tenant, *group), ()),
            }
        }
        Ok(behind)
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
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query!(
            r#"INSERT INTO identity (id) VALUES ($1)
               RETURNING id as "id: IdentityId", status, created_at"#,
            id.as_uuid(),
        )
        .fetch_one(&mut *tx)
        .await?;

        self.record(
            &mut tx,
            actor,
            None,
            "identity.created",
            "identity",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;

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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE identity
                SET status = 'suspended', suspended_reason = $2, suspended_at = now()
              WHERE id = $1",
            id.as_uuid(),
            reason,
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            None,
            "identity.suspended",
            "identity",
            &id.to_string(),
            serde_json::json!({ "reason": reason }),
        )
        .await?;
        tx.commit().await?;
        // Local invalidation: this node stops honouring the identity at once.
        // Other nodes converge within ENTRY_CACHE_TTL — a documented window,
        // see the `cache` module.
        self.forget(crate::shared::Invalidate::Identity(id)).await;
        Ok(())
    }

    /// **Erases a person**, keeping what the platform did.
    ///
    /// # What goes and what stays
    ///
    /// The identity row goes, and with it every authenticator, session and
    /// membership — those cascade. What stays is the audit trail, with this
    /// person's **link** removed from it: the entries they produced remain,
    /// saying what was done and when, attributed to nobody. That is the same
    /// shape an entry has always had for a system-initiated action.
    ///
    /// **Their address is not removed from it.** Some entries name a person by
    /// login rather than by link — `invitation.created` and
    /// `invitation.accepted` and `signup.confirmed` carry the handle in
    /// `detail`, and `signup.requested` has it as its subject — and those
    /// stay, as does the identity's id as the subject of entries about them.
    /// That is a decision (5 of 2026-09-11), not an oversight: the trail is
    /// kept as the legal record of who was given access to what, and when, and
    /// an invitation entry that no longer said who was invited would record
    /// nothing. The trigger allows no other change anyway.
    ///
    /// Business records are untouched, and deliberately. An invoice naming a
    /// customer is a legal document a tax authority requires to be kept for
    /// six years; erasing a *user account* is a different act from destroying
    /// the books, and conflating them would put the business in breach to keep
    /// one person happy.
    ///
    /// # Why this could not be done before
    ///
    /// `audit_entry`'s trigger refused the `ON DELETE SET NULL` its own foreign
    /// keys declared, so `DELETE FROM identity` failed for anybody who had ever
    /// acted. See `migrations/control/0007_erasure.sql`.
    ///
    /// # What is deliberately not here
    ///
    /// An HTTP endpoint. **Who may erase whom** is a policy question — a
    /// tenant owner erasing a colleague is not the same act as a person erasing
    /// themselves, and neither is platform staff erasing a customer — and
    /// answering it in passing while fixing a schema bug would be answering it
    /// badly.
    pub async fn erase_identity(&self, id: IdentityId, actor: Actor) -> Result<(), AccessError> {
        // **Recorded first, in the erasure's own transaction.** The entry names
        // the identity by id in `subject_id`, which is text and survives the
        // delete; and an erasure nobody can see having happened is the one
        // kind this must not be, which is why the two commit together.
        let mut tx = self.pool.begin().await?;
        self.record(
            &mut tx,
            actor,
            None,
            "identity.erased",
            "identity",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;

        sqlx::query!("DELETE FROM identity WHERE id = $1", id.as_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        // This node stops honouring them at once; others converge within
        // `ENTRY_CACHE_TTL`, the same window as a suspension.
        self.forget(crate::shared::Invalidate::Identity(id)).await;
        Ok(())
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
        let mut tx = self.pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;

        self.record(
            &mut tx,
            actor,
            None,
            "cluster.registered",
            "cluster",
            name,
            serde_json::json!({
                "max_active_tenants": max_active_tenants,
                "max_databases": max_databases,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE cluster SET status = $2 WHERE name = $1",
            name,
            status.as_str(),
        )
        .execute(&mut *tx)
        .await?;

        self.record(
            &mut tx,
            actor,
            None,
            "cluster.status_changed",
            "cluster",
            name,
            serde_json::json!({ "status": status.as_str() }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
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

        let mut tx = self.pool.begin().await?;
        let row = sqlx::query!(
            r#"INSERT INTO tenant (id, slug, display_name, cluster, database_name)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING id as "id: TenantId", slug, display_name, status, cluster,
                         database_name, demo_expires_at,
                      requires_second_factor, created_at"#,
            id.as_uuid(),
            slug,
            display_name,
            cluster,
            database_name,
        )
        .fetch_one(&mut *tx)
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
            &mut tx,
            actor,
            Some(id),
            "tenant.registered",
            "tenant",
            &id.to_string(),
            serde_json::json!({ "slug": slug, "cluster": cluster }),
        )
        .await?;
        tx.commit().await?;

        tenant_from_row(
            row.id,
            row.slug,
            row.display_name,
            &row.status,
            row.cluster,
            row.database_name,
            row.demo_expires_at,
            row.requires_second_factor,
            row.created_at,
        )
    }

    pub async fn tenant(&self, id: TenantId) -> Result<Option<Tenant>, AccessError> {
        let row = sqlx::query!(
            r#"SELECT id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at,
                      requires_second_factor, created_at
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
                row.requires_second_factor,
                row.created_at,
            )
        })
        .transpose()
    }

    /// **The tenant a custom host names**, if that host is under a domain a
    /// tenant has proved. `api.salon.example` reaches the tenant that proved
    /// `salon.example`; the longest proved domain wins when one is under
    /// another. `None` for a host nobody has proved — including every host that
    /// merely resembles one. Cached by host and cleared on any domain change.
    pub async fn tenant_by_host(&self, host: &str) -> Result<Option<Tenant>, AccessError> {
        let host = host
            .split(':')
            .next()
            .unwrap_or(host)
            .trim()
            .trim_end_matches('.')
            .to_lowercase();
        if host.is_empty() {
            return Ok(None);
        }
        if let Some(hit) = self.hosts.get(&host) {
            self.hit();
            return Ok(hit);
        }
        self.miss();
        let row = sqlx::query!(
            r#"SELECT t.id as "id: TenantId", t.slug, t.display_name, t.status, t.cluster,
                      t.database_name, t.demo_expires_at,
                      t.requires_second_factor, t.created_at
                 FROM tenant_domain d
                 JOIN tenant t ON t.id = d.tenant
                WHERE d.verified_at IS NOT NULL
                  AND ($1 = d.domain OR $1 LIKE '%.' || d.domain)
                ORDER BY length(d.domain) DESC
                LIMIT 1"#,
            host,
        )
        .fetch_optional(&self.pool)
        .await?;
        let tenant = row
            .map(|row| {
                tenant_from_row(
                    row.id,
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.requires_second_factor,
                    row.created_at,
                )
            })
            .transpose()?;
        self.hosts.put(host, tenant.clone());
        Ok(tenant)
    }

    pub async fn tenant_by_slug(&self, slug: &str) -> Result<Option<Tenant>, AccessError> {
        let row = sqlx::query!(
            r#"SELECT id as "id: TenantId", slug, display_name, status, cluster,
                      database_name, demo_expires_at,
                      requires_second_factor, created_at
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
                row.requires_second_factor,
                row.created_at,
            )
        })
        .transpose()
    }

    /// Marks a tenant active. Called by the provisioning workflow once the
    /// database exists, is migrated, and is seeded — never before, or entry
    /// would succeed against a database with no schema.
    ///
    /// Refuses a tenant that is not provisioning, with
    /// [`AccessError::WrongTenantStatus`]: reinstating a suspended one is
    /// [`Self::reinstate_tenant`], and this used to answer `Ok` to it, change
    /// nothing, and record a `tenant.activated` anyway.
    pub async fn activate_tenant(&self, id: TenantId, actor: Actor) -> Result<(), AccessError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query!(
            "UPDATE tenant SET status = 'active', activated_at = now()
              WHERE id = $1 AND status = 'provisioning'",
            id.as_uuid(),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        self.moved(
            tx,
            id,
            rows,
            TenantStatus::Provisioning,
            "tenant.activated",
            serde_json::json!({}),
            actor,
        )
        .await
    }

    /// **Suspends a tenant: its doors shut now, and once its issued documents
    /// are signed and reported nothing runs for it until it is reinstated.**
    ///
    /// The first half is `suspending` (decided 2026-09-14). Every door refuses
    /// it — members, API keys and the public alike get the same
    /// `access.tenant_unavailable` — but the worker keeps claiming it for the
    /// two jobs that sign and report to ZATCA, and for those alone; when they
    /// have nothing left it calls [`Self::finish_suspension`], and from
    /// `suspended` on the worker stops claiming it, and a visit already under
    /// way stops before its next job, because [`Self::renew_lease`] answers
    /// `false`. Support can still open it ([`Self::enter_for_support`]) in
    /// either half, and the fleet migrator still brings its schema current, so
    /// it comes back to one this build can read.
    ///
    /// Sessions are left alone: they belong to people, who may work for other
    /// tenants too. This node refuses the tenant on the next request; the others
    /// within [`ENTRY_CACHE_TTL`], or at once where the caches are shared.
    ///
    /// The reason goes on the tenant row and into the audit trail.
    ///
    /// # Errors
    /// [`AccessError::WrongTenantStatus`] unless the tenant is active;
    /// [`AccessError::SuspensionReason`] for a blank one or one over 500
    /// characters; [`AccessError::NoSuchTenant`].
    pub async fn suspend_tenant(
        &self,
        id: TenantId,
        reason: &str,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let reason = reason.trim();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query!(
            "UPDATE tenant
                SET status = 'suspending', suspended_reason = $2, suspended_at = now()
              WHERE id = $1 AND status = 'active'",
            id.as_uuid(),
            reason,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db)
                if db.constraint() == Some("tenant_suspension_is_complete") =>
            {
                AccessError::SuspensionReason
            }
            _ => AccessError::Database(e),
        })?
        .rows_affected();
        self.moved(
            tx,
            id,
            rows,
            TenantStatus::Active,
            "tenant.suspended",
            serde_json::json!({ "reason": reason }),
            actor,
        )
        .await
    }

    /// Lifts a suspension. The tenant is enterable on this node at once, and
    /// the worker claims it when its `next_visit_at` comes round, which is when
    /// the jobs it missed catch up.
    ///
    /// # Errors
    /// [`AccessError::WrongTenantStatus`] unless the tenant is suspended;
    /// [`AccessError::NoSuchTenant`].
    pub async fn reinstate_tenant(&self, id: TenantId, actor: Actor) -> Result<(), AccessError> {
        let mut tx = self.pool.begin().await?;
        // From either half of a suspension: one still draining is reinstated
        // as readily as one that finished, and its drain simply stops mattering.
        let rows = sqlx::query!(
            "UPDATE tenant SET status = 'active', suspended_reason = NULL, suspended_at = NULL
              WHERE id = $1 AND status IN ('suspending', 'suspended')",
            id.as_uuid(),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        self.moved(
            tx,
            id,
            rows,
            TenantStatus::Suspended,
            "tenant.reinstated",
            serde_json::json!({}),
            actor,
        )
        .await
    }

    /// **The second half of a suspension**: the worker found nothing left to
    /// sign or report, so nothing runs for the tenant from here on.
    ///
    /// `false` when the tenant is no longer `suspending` — reinstated while the
    /// drain ran, or already finished by another visit — which is not an error:
    /// the worker that asked simply has nothing to finish. Recorded under the
    /// system's name, because no person did it; the reason and the instant
    /// staff wrote stay on the row.
    ///
    /// # Errors
    /// The database.
    pub async fn finish_suspension(&self, id: TenantId) -> Result<bool, AccessError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query!(
            "UPDATE tenant SET status = 'suspended'
              WHERE id = $1 AND status = 'suspending'",
            id.as_uuid(),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if rows == 0 {
            return Ok(false);
        }
        self.record(
            &mut tx,
            Actor::system(),
            Some(id),
            "tenant.suspension_complete",
            "tenant",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Tenant(id)).await;
        Ok(true)
    }

    /// **The one place a tenant status change is judged**, after its `UPDATE
    /// ... WHERE status = expected` has run.
    ///
    /// No row changed means the tenant was not in `expected`, and the caller
    /// is told what it is in instead — never `Ok`, so a repeat is not recorded
    /// as though it did something. A change is put on the record **in the
    /// transaction the `UPDATE` ran on**, committed here, and then forgotten
    /// from every entry cache.
    #[expect(
        clippy::too_many_arguments,
        reason = "the transaction joined a call that was already at the limit; the six audit fields are `record`'s"
    )]
    async fn moved(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        id: TenantId,
        rows: u64,
        expected: TenantStatus,
        action: &str,
        detail: serde_json::Value,
        actor: Actor,
    ) -> Result<(), AccessError> {
        if rows == 0 {
            // Dropping `tx` rolls back the nothing it did.
            return Err(match self.tenant(id).await? {
                None => AccessError::NoSuchTenant,
                Some(tenant) => AccessError::WrongTenantStatus {
                    status: tenant.status,
                    expected,
                },
            });
        }
        self.record(
            &mut tx,
            actor,
            Some(id),
            action,
            "tenant",
            &id.to_string(),
            detail,
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Tenant(id)).await;
        Ok(())
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
        let mut tx = self.pool.begin().await?;
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
        .fetch_optional(&mut *tx)
        .await?;

        let Some(id) = revived.map(MembershipId::from_uuid) else {
            // A live membership was already there. Idempotent success, and
            // deliberately without touching the role — callers that mean to
            // change one call `change_role`, which knows about last owners.
            // Nothing was written, so the transaction drops.
            return self
                .membership_id(identity_id, scope)
                .await?
                .ok_or_else(|| {
                    AccessError::Corrupt(
                        "a membership conflicted and then could not be found".to_owned(),
                    )
                });
        };

        self.record(
            &mut tx,
            actor,
            scope.tenant(),
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
        tx.commit().await?;

        // A grant or revocation must take effect now on this node, not after
        // the TTL. Both caches, because the scope decides which one holds it.
        match scope.tenant() {
            Some(tenant) => {
                self.forget(crate::shared::Invalidate::Membership {
                    identity: identity_id,
                    tenant,
                })
                .await;
            }
            None => {
                self.forget(crate::shared::Invalidate::Platform(identity_id))
                    .await;
            }
        }

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
        let mut tx = self.pool.begin().await?;
        let revoked = sqlx::query!(
            "UPDATE membership SET revoked_at = now()
              WHERE identity_id = $1
                AND tenant_id IS NOT DISTINCT FROM $2
                AND revoked_at IS NULL",
            identity_id.as_uuid(),
            scope.tenant().map(TenantId::into_uuid),
        )
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;

        self.record(
            &mut tx,
            actor,
            scope.tenant(),
            "membership.revoked",
            "identity",
            &identity_id.to_string(),
            serde_json::json!({
                "scope": scope.kind_str(),
                "tenant": scope.tenant().map(|t| t.to_string()),
            }),
        )
        .await?;
        tx.commit().await?;

        // A grant or revocation must take effect now on this node, not after
        // the TTL. Both caches, because the scope decides which one holds it.
        match scope.tenant() {
            Some(tenant) => {
                self.forget(crate::shared::Invalidate::Membership {
                    identity: identity_id,
                    tenant,
                })
                .await;
            }
            None => {
                self.forget(crate::shared::Invalidate::Platform(identity_id))
                    .await;
            }
        }

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

    /// The live platform role, uncached. A stored role this build does not
    /// know is an error, not a default — the reason `live_access` gives.
    async fn platform_role(
        &self,
        identity_id: IdentityId,
    ) -> Result<Option<PlatformRole>, AccessError> {
        sqlx::query_scalar!(
            "SELECT role FROM membership
              WHERE identity_id = $1 AND scope_kind = 'platform' AND revoked_at IS NULL",
            identity_id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?
        .map(|role| staff::parse_platform_role(&role))
        .transpose()
    }

    /// Every tenant this identity may enter. The tenant switcher's query.
    pub async fn tenants_for_identity(
        &self,
        identity_id: IdentityId,
    ) -> Result<Vec<Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT t.id as "id: TenantId", t.slug, t.display_name, t.status, t.cluster,
                      t.database_name, t.demo_expires_at,
                      t.requires_second_factor, t.created_at
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
                    row.requires_second_factor,
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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO entitlement (tenant_id, module_id) VALUES ($1, $2)
             ON CONFLICT (tenant_id, module_id)
             DO UPDATE SET disabled_at = NULL, enabled_at = now()",
            tenant_id.as_uuid(),
            module.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "module.enabled",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "module": module.as_str() }),
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Entitlements(tenant_id))
            .await;
        Ok(())
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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE entitlement SET disabled_at = now()
              WHERE tenant_id = $1 AND module_id = $2 AND disabled_at IS NULL",
            tenant_id.as_uuid(),
            module.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "module.disabled",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "module": module.as_str() }),
        )
        .await?;
        tx.commit().await?;
        self.forget(crate::shared::Invalidate::Entitlements(tenant_id))
            .await;
        Ok(())
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

    /// Appends an audit entry — the only way one is written. The table's
    /// append-only trigger refuses `DELETE` and every `UPDATE` but one:
    /// erasure nulling an actor (`0007_erasure.sql`, re-pinned in `0019`).
    ///
    /// `tenant` is the company the entry concerns, or `None` for one that
    /// concerns none — a person, a cluster, the platform's own outbox. It is
    /// what [`Self::tenant_audit`] selects on. A tenant given here is stored
    /// as given. `None` is filled in by the table's insert trigger
    /// (`audit_entry_tenant` in `0019`) when the subject is a tenant, `detail`
    /// has a `tenant`, or the subject is an API key, which is how a pod still
    /// on the build before `0019` files its entries during a deploy. So a
    /// `None` entry with such a subject or detail is in that tenant's trail
    /// anyway. Every writer that passes `None` today names none of the three.
    ///
    /// **On the connection the change was made on**, so the entry commits with
    /// the change or not at all. Until 2026-09-14 this ran on the pool, after
    /// the caller's own commit, and a crash between the two left an act that
    /// stood with no record that it happened — the one shape an audit trail
    /// must not have. Every writer passes its transaction; the two acts with
    /// no control-plane write of their own, support entering a tenant and a
    /// confirmed signup whose build is many transactions, acquire a
    /// connection and say `audit-only:` beside it. `tests/audit.rs` scans for
    /// anything else.
    #[expect(
        clippy::too_many_arguments,
        reason = "the connection joined the six fields an entry has always taken; a struct for them would be thirty-one call sites of ceremony"
    )]
    pub async fn record(
        &self,
        conn: &mut sqlx::PgConnection,
        actor: Actor,
        tenant: Option<TenantId>,
        action: &str,
        subject_type: &str,
        subject_id: &str,
        detail: serde_json::Value,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "INSERT INTO audit_entry
                (actor_identity_id, on_behalf_of_identity_id, tenant_id, action, subject_type,
                 subject_id, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            actor.identity.map(IdentityId::into_uuid),
            actor.on_behalf_of.map(IdentityId::into_uuid),
            tenant.map(TenantId::into_uuid),
            action,
            subject_type,
            subject_id,
            detail,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// **A tenant's trail**, newest first: every entry [`Self::record`] was
    /// told concerns it, or whose `None` the insert trigger filled in with it.
    /// Not what concerns a person the tenant happens to
    /// hold — a suspension of somebody may be about their conduct elsewhere.
    ///
    /// Asks nothing about the tenant's status, and the caller must not either:
    /// a suspended tenant's owner reads the reason here. The door is
    /// [`Self::admit`], never [`Self::enter`].
    pub async fn tenant_audit(
        &self,
        tenant: TenantId,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Page<AuditEntry>, AccessError> {
        self.audit(Some(tenant), None, false, limit, before).await
    }

    /// **A person's own trail**, newest first: entries about them, and
    /// entries they made or were impersonated in — the right of access under
    /// the PDPL (decision 6 of 2026-09-11).
    pub async fn identity_audit(
        &self,
        identity: IdentityId,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Page<AuditEntry>, AccessError> {
        self.audit(None, Some(identity), false, limit, before).await
    }

    /// **The whole trail, for staff**, narrowed to a tenant, a person, or
    /// both (both is the entries that are in each). Neither is everything,
    /// the entries about no tenant and no person included — the platform's
    /// dead letters, clusters, signups nobody confirmed. Every actor is named.
    pub async fn platform_audit(
        &self,
        tenant: Option<TenantId>,
        identity: Option<IdentityId>,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Page<AuditEntry>, AccessError> {
        self.audit(tenant, identity, true, limit, before).await
    }

    /// The three readers' one query. `staff` names every actor; otherwise an
    /// actor is named only where they are, or were, a member of the tenant the
    /// entry concerns — `membership` keeps revoked rows, so somebody who has
    /// left is still named for what they did. Staff who are not, and never
    /// were, members of the tenant appear by id alone; one who was is a
    /// co-member like any other, and named.
    // ponytail: one statement with optional filters, planned per call; split it
    // per reader if a plan goes bad at volume.
    async fn audit(
        &self,
        tenant: Option<TenantId>,
        identity: Option<IdentityId>,
        staff: bool,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Page<AuditEntry>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT e.id, e.at,
                      e.actor_identity_id as "actor: IdentityId",
                      e.on_behalf_of_identity_id as "on_behalf_of: IdentityId",
                      e.tenant_id as "tenant: TenantId",
                      e.action, e.subject_type, e.subject_id, e.detail,
                      (SELECT a.handle FROM authenticator a
                        WHERE a.identity_id = e.actor_identity_id AND a.kind = 'password'
                          AND ($3 OR EXISTS (SELECT 1 FROM membership m
                                              WHERE m.identity_id = e.actor_identity_id
                                                AND m.tenant_id = e.tenant_id))
                        LIMIT 1) as actor_handle
                 FROM audit_entry e
                WHERE ($1::uuid IS NULL OR e.tenant_id = $1)
                  AND ($2::uuid IS NULL
                       OR (e.subject_type = 'identity' AND e.subject_id = $2::text)
                       OR e.actor_identity_id = $2
                       OR e.on_behalf_of_identity_id = $2)
                  AND ($4::bigint IS NULL OR e.id < $4)
                ORDER BY e.id DESC
                LIMIT $5"#,
            tenant.map(TenantId::into_uuid),
            identity.map(IdentityId::into_uuid),
            staff,
            before,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .into_iter()
            .map(|row| AuditEntry {
                id: row.id,
                at: row.at,
                actor: row.actor,
                actor_handle: row.actor_handle,
                on_behalf_of: row.on_behalf_of,
                tenant: row.tenant,
                action: row.action,
                subject_type: row.subject_type,
                subject_id: row.subject_id,
                detail: row.detail,
            })
            .collect();
        Ok(Page::of(entries, limit, |entry| {
            Cursor::over(&[&entry.id.to_string()])
        }))
    }
}

/// Where a page of the audit trail resumes, from the cursor
/// [`ControlPlane::tenant_audit`] and its siblings handed out. Anything else —
/// another list's cursor, a hand-made one — is refused, never read as "from
/// the top" (L6).
pub fn audit_position(cursor: &Cursor) -> Result<i64, NotACursor> {
    match cursor.parts() {
        [id] => id.parse().map_err(|_| NotACursor),
        _ => Err(NotACursor),
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
            // **Not `ACCESS_DENIED`.** The caller is a member and is signed in;
            // the one thing they can do about this is enrol, and a message that
            // says "denied" tells them to give up instead.
            Self::SecondFactorRequired => Message::new(messages::TENANT_REQUIRES_SECOND_FACTOR),
            // The same 403 a tenant role gets, naming the power the way that
            // one names the capability.
            Self::StaffOnly(power) => Message::new(messages::NOT_PERMITTED)
                .with("capability", MessageArg::text(power.as_str())),
            Self::StaffSecondFactorRequired => Message::new(messages::STAFF_SECOND_FACTOR_REQUIRED),
            Self::TenantNotActive { status } => match status {
                // Provisioning is a retry, and saying so saves a support ticket.
                TenantStatus::Provisioning => Message::new(messages::TENANT_PROVISIONING),
                _ => Message::new(messages::TENANT_UNAVAILABLE),
            },
            // Staff-facing: only a platform route reaches these, so naming the
            // status leaks nothing the caller may not see.
            Self::WrongTenantStatus { status, expected } => {
                Message::new(messages::WRONG_TENANT_STATUS)
                    .with("status", MessageArg::text(status.as_str()))
                    .with("expected", MessageArg::text(expected.as_str()))
            }
            Self::SuspensionReason => Message::new(messages::SUSPENSION_REASON),
            Self::DomainNotClaimed(domain) => Message::new(messages::DOMAIN_NOT_CLAIMED)
                .with("domain", MessageArg::text(domain.clone())),
            Self::DomainNotProved {
                domain,
                record,
                expected,
            } => Message::new(messages::DOMAIN_NOT_PROVED)
                .with("domain", MessageArg::text(domain.clone()))
                .with("record", MessageArg::text(record.clone()))
                .with("expected", MessageArg::text(expected.clone())),
            Self::DomainProofUnavailable(_) => Message::new(messages::DOMAIN_PROOF_UNAVAILABLE),
            Self::NotAnOrigin(origin) => Message::new(messages::NOT_AN_ORIGIN)
                .with("origin", MessageArg::text(origin.clone())),
            Self::OriginOutsideDomain { origin, domain } => {
                Message::new(messages::ORIGIN_OUTSIDE_DOMAIN)
                    .with("origin", MessageArg::text(origin.clone()))
                    .with("domain", MessageArg::text(domain.clone()))
            }
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
            //
            // `TooOldToUpgrade` is here for a different reason: it cannot reach a
            // request at all. Only `migrate_fleet` produces it, and its audience
            // is an operator reading `Display`, which names the release to
            // install. Giving it a user-facing code would be inventing an
            // audience it does not have.
            Self::Database(_) | Self::Corrupt(_) | Self::TooOldToUpgrade { .. } => {
                Message::new(messages::INTERNAL)
            }
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
    format!("erp_tenant_{}", id.as_uuid().simple())
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
        "suspending" => Ok(TenantStatus::Suspending),
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
    demo_expires_at: Option<erp_types::Timestamp>,
    requires_second_factor: bool,
    created_at: erp_types::Timestamp,
) -> Result<Tenant, AccessError> {
    Ok(Tenant {
        id,
        slug,
        display_name,
        status: parse_tenant_status(status)?,
        cluster,
        database_name,
        demo_expires_at,
        requires_second_factor,
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
        assert!(name.starts_with("erp_tenant_"));
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
