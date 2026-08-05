//! The only handle that reaches a tenant's data.

use spa_types::{ModuleId, TenantId};
use sqlx::PgPool;
use tokio::sync::OwnedSemaphorePermit;

use crate::model::EnabledModules;

/// A connection to one tenant's database, plus what is known about that tenant.
///
/// # Why this type is the security boundary
///
/// There is no public constructor. The only way to obtain a `TenantDb` is
/// [`ControlPlane::enter`], which checks that the identity is active, the tenant
/// is enterable, and a live membership joins them. A function that takes
/// `&TenantDb` has therefore been handed proof that those checks passed.
///
/// The consequence worth being explicit about: **there is no ambient pool.** No
/// `AppState.pool`, no global. A query against the wrong tenant is not prevented
/// by a `WHERE` clause or a row-level policy — it cannot be written, because no
/// connection that could serve it is in scope. That is the property
/// database-per-tenant is bought for.
///
/// # Lifetime
///
/// One handle per unit of work. It carries a budget permit (see
/// [`TenantPools`](crate::TenantPools)), released on drop, which is what keeps
/// connection count proportional to concurrency rather than to tenant count.
/// Holding one across an idle wait is a bug: it spends budget for nothing.
#[derive(Debug)]
pub struct TenantDb {
    tenant: TenantId,
    pool: PgPool,
    modules: EnabledModules,
    /// Released on drop. Never read — its existence is the point.
    _budget: OwnedSemaphorePermit,
}

impl TenantDb {
    pub(crate) const fn new(
        tenant: TenantId,
        pool: PgPool,
        modules: EnabledModules,
        budget: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            tenant,
            pool,
            modules,
            _budget: budget,
        }
    }

    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// This tenant's connection pool, and no other's.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    #[must_use]
    pub const fn modules(&self) -> &EnabledModules {
        &self.modules
    }

    /// Whether a module is live for this tenant.
    ///
    /// Phase 4 replaces most uses of this with a `ModuleEnabled<M>` token, so
    /// that a disabled module's handlers cannot be constructed rather than
    /// returning 403 at runtime. Until then this is the check.
    #[must_use]
    pub fn has_module(&self, module: &ModuleId) -> bool {
        self.modules.contains(module)
    }
}
