//! The only handle that reaches a tenant's data.

use std::sync::Arc;

use spa_types::{ModuleId, TenantId};
use sqlx::PgPool;

use crate::model::EnabledModules;
use crate::pools::{Conn, Lane, PoolError, TenantPools, Tx};

/// A route to one tenant's database, plus what is known about that tenant.
///
/// # Why this type is the security boundary
///
/// There is no public constructor. The only way to obtain a `TenantDb` is
/// [`ControlPlane::enter`](crate::ControlPlane::enter), which checks that the
/// identity is active, the tenant is enterable, and a live membership joins
/// them. A function taking `&TenantDb` has been handed proof that those checks
/// passed.
///
/// The consequence worth being explicit about: **there is no ambient pool.** No
/// `AppState.pool`, no global. A query against the wrong tenant is not prevented
/// by a `WHERE` clause or a row-level policy — it cannot be written, because no
/// connection that could serve it is in scope.
///
/// # Cost
///
/// Holding one is free. Budget is spent by [`Self::acquire`], [`Self::begin`]
/// and [`Self::read`], for exactly as long as the returned guard lives. So a
/// handle kept across business logic, an HTTP call, or response serialization
/// costs nothing — which is what keeps connection demand proportional to
/// concurrent *queries* rather than concurrent *requests*. At 10,000 requests a
/// second that is the difference between ~120 connections and ~400.
///
/// Corollary: **do not hold a `Conn` or `Tx` across an await that isn't
/// database work.** That is the one way to reintroduce the problem.
#[derive(Debug)]
pub struct TenantDb {
    tenant: TenantId,
    write: PgPool,
    /// `None` when the cluster has no replica; [`Self::read`] then uses the
    /// primary, so callers need no fallback logic.
    read: Option<PgPool>,
    modules: EnabledModules,
    pools: Arc<TenantPools>,
    lane: Lane,
    /// What the caller may do here, and where. `None` for background and
    /// support access, which are not acting as anyone — see [`Self::role`].
    access: Option<crate::Access>,
}

impl TenantDb {
    pub(crate) const fn new(
        tenant: TenantId,
        write: PgPool,
        read: Option<PgPool>,
        modules: EnabledModules,
        pools: Arc<TenantPools>,
        lane: Lane,
    ) -> Self {
        Self {
            tenant,
            write,
            read,
            modules,
            pools,
            lane,
            access: None,
        }
    }

    pub(crate) fn set_access(&mut self, access: Option<crate::Access>) {
        self.access = access;
    }

    /// The caller's role, if a person is behind this handle.
    ///
    /// `None` for maintenance and provisioning, which act on the system's
    /// behalf rather than anyone's. A capability check against `None` must
    /// refuse — background work that needs to write should say so in its own
    /// terms, not borrow a person's authority.
    #[must_use]
    pub fn role(&self) -> Option<crate::Role> {
        self.access.as_ref().map(|a| a.role)
    }

    /// The caller's role **in a module**, which is what a module's routes are
    /// judged against.
    ///
    /// Falls back to the tenant-wide role wherever the tenant has not said
    /// otherwise, so adding a module to the product is not a permissions
    /// migration for every existing member.
    #[must_use]
    pub fn role_in(&self, module: Option<&ModuleId>) -> Option<crate::Role> {
        self.access.as_ref().map(|a| a.role_in(module))
    }

    /// Everything decided about this caller's access, for a members list.
    #[must_use]
    pub const fn access(&self) -> Option<&crate::Access> {
        self.access.as_ref()
    }

    /// Whether the caller may do this.
    ///
    /// The single predicate every check goes through. `Allowed<C>` in
    /// `spa-api` is the type-level form; this is what it calls.
    #[must_use]
    pub fn allows(&self, capability: crate::Capability) -> bool {
        self.allows_in(capability, None)
    }

    /// Whether the caller may do this **in this module**.
    ///
    /// `None` means the tenant's own surface — members, invitations,
    /// entitlements — which is nobody's module and uses the tenant-wide role.
    /// That is what stops an accountant-for-sales from managing who else has
    /// access.
    #[must_use]
    pub fn allows_in(&self, capability: crate::Capability, module: Option<&ModuleId>) -> bool {
        self.access
            .as_ref()
            .is_some_and(|access| access.allows(capability, module))
    }

    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Which bulkhead this handle's operations draw from.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        self.lane
    }

    #[must_use]
    pub const fn modules(&self) -> &EnabledModules {
        &self.modules
    }

    /// Whether a module is live for this tenant.
    ///
    /// Phase 4 replaces most uses with a `ModuleEnabled<M>` token, so a disabled
    /// module's handlers cannot be constructed rather than returning 403 at
    /// runtime. Until then this is the check.
    #[must_use]
    pub fn has_module(&self, module: &ModuleId) -> bool {
        self.modules.contains(module)
    }

    /// A connection to the primary, holding a budget permit until dropped.
    pub async fn acquire(&self) -> Result<Conn, PoolError> {
        let permit = self.pools.permit(self.lane)?;
        let conn = self.write.acquire().await?;
        Ok(Conn::new(conn, permit))
    }

    /// A transaction on the primary, holding a budget permit until it commits
    /// or rolls back.
    pub async fn begin(&self) -> Result<Tx, PoolError> {
        let permit = self.pools.permit(self.lane)?;
        let tx = self.write.begin().await?;
        Ok(Tx::new(tx, permit))
    }

    /// A connection for reads that tolerate replication lag.
    ///
    /// Routes to a replica when the cluster has one, and to the primary when it
    /// does not — so this is always safe to call and adding replicas later is a
    /// configuration change, not a code change.
    ///
    /// **Read-your-writes does not hold here.** Anything that must observe a
    /// write it just made uses [`Self::acquire`]. The API contract exposes this
    /// to clients as `?consistent_after=<position>`; see architecture §5.7.
    pub async fn read(&self) -> Result<Conn, PoolError> {
        let permit = self.pools.permit(self.lane)?;
        let pool = self.read.as_ref().unwrap_or(&self.write);
        let conn = pool.acquire().await?;
        Ok(Conn::new(conn, permit))
    }

    /// Whether reads are served by a replica. For telemetry and for tests that
    /// assert routing.
    #[must_use]
    pub const fn has_replica(&self) -> bool {
        self.read.is_some()
    }

    /// Runs a command against this tenant, retrying if someone wrote first.
    ///
    /// The retry loop lives here rather than in `spa-eventlog` because each
    /// attempt needs a transaction, and a transaction needs a permit from this
    /// tenant's lane. A version taking a bare `PgPool` would either hand out an
    /// unmetered connection or hold one permit across every attempt.
    ///
    /// See `spa_eventlog::try_execute` for what one attempt does.
    pub async fn execute<A, F, E>(
        &self,
        id: &spa_types::AggregateId,
        upcasters: &spa_eventlog::Upcasters,
        metadata: &spa_eventlog::Metadata,
        decide: F,
    ) -> Result<spa_eventlog::Committed<A::Event>, CommandError<E>>
    where
        A: spa_eventlog::Aggregate,
        F: Fn(&spa_eventlog::Loaded<A>) -> Result<spa_eventlog::Decision<A::Event>, E>,
    {
        for attempt in 1..=spa_eventlog::MAX_ATTEMPTS {
            let mut tx = self.begin().await?;

            match spa_eventlog::try_execute::<A, _, E>(&mut tx, id, upcasters, metadata, &decide)
                .await
            {
                Ok(committed) => {
                    tx.commit()
                        .await
                        .map_err(spa_eventlog::ExecuteError::from)?;
                    return Ok(committed);
                }
                Err(e) if e.is_conflict() => {
                    tx.rollback()
                        .await
                        .map_err(spa_eventlog::ExecuteError::from)?;
                    tracing::debug!(
                        tenant = %self.tenant,
                        attempt,
                        "optimistic concurrency conflict, retrying"
                    );
                }
                Err(e) => {
                    tx.rollback()
                        .await
                        .map_err(spa_eventlog::ExecuteError::from)?;
                    return Err(e.into());
                }
            }
        }

        Err(spa_eventlog::ExecuteError::Contended {
            stream: spa_types::StreamId::new(A::domain(), id.clone()),
            attempts: spa_eventlog::MAX_ATTEMPTS,
        }
        .into())
    }
}

/// What a command against a tenant can fail with.
///
/// Two layers, kept apart because they mean different things to a caller:
/// [`PoolError::Overloaded`] is "come back in a moment" and deserves a 503,
/// while everything inside [`spa_eventlog::ExecuteError`] is about the command
/// itself. Flattening them would turn backpressure into a 500.
#[derive(Debug, thiserror::Error)]
pub enum CommandError<E> {
    #[error(transparent)]
    Pool(#[from] PoolError),
    #[error(transparent)]
    Execute(#[from] spa_eventlog::ExecuteError<E>),
}
