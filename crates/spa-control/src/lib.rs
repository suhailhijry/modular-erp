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

mod model;
mod pools;
mod tenant_db;

pub use model::{
    Actor, EnabledModules, Entitlement, Identity, IdentityStatus, Membership, Scope, Tenant,
    TenantStatus,
};
pub use pools::{ClusterRegistry, PoolConfig, PoolError, TenantPools};
pub use tenant_db::TenantDb;

use spa_types::{IdentityId, MembershipId, ModuleId, TenantId};
use sqlx::PgPool;

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
}

/// Handle on the core database.
#[derive(Debug)]
pub struct ControlPlane {
    pool: PgPool,
    tenants: TenantPools,
}

impl ControlPlane {
    #[must_use]
    pub const fn new(pool: PgPool, tenants: TenantPools) -> Self {
        Self { pool, tenants }
    }

    /// The core database. Control-plane queries only — this is not a route to
    /// tenant data.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    #[must_use]
    pub const fn tenants(&self) -> &TenantPools {
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
    ) -> Result<TenantDb, AccessError> {
        let identity = self
            .identity(identity_id)
            .await?
            .ok_or(AccessError::NoSuchIdentity)?;
        if !identity.is_active() {
            return Err(AccessError::IdentitySuspended);
        }

        let tenant = self
            .tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;
        if !tenant.is_enterable() {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        if !self.has_live_membership(identity_id, tenant_id).await? {
            return Err(AccessError::NotAMember);
        }

        self.open(&tenant).await
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
            .identity(staff_id)
            .await?
            .ok_or(AccessError::NoSuchIdentity)?;
        if !staff.is_active() {
            return Err(AccessError::IdentitySuspended);
        }
        if !self.has_platform_membership(staff_id).await? {
            return Err(AccessError::NotAMember);
        }

        let tenant = self
            .tenant(tenant_id)
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

        self.open(&tenant).await
    }

    async fn open(&self, tenant: &Tenant) -> Result<TenantDb, AccessError> {
        let modules = self.enabled_modules(tenant.id).await?;
        let (pool, permit) = self
            .tenants
            .acquire(tenant.id, &tenant.cluster, &tenant.database_name)
            .await?;
        Ok(TenantDb::new(tenant.id, pool, modules, permit))
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
    // Tenants
    // -----------------------------------------------------------------------

    /// Registers a tenant in `provisioning`. The database itself is created by
    /// the provisioning workflow, which is why this does not touch a cluster.
    pub async fn register_tenant(
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
        .await?;

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
        sqlx::query!(
            "INSERT INTO membership (id, identity_id, scope_kind, tenant_id, role)
             VALUES ($1, $2, $3, $4, $5)",
            id.as_uuid(),
            identity_id.as_uuid(),
            scope.kind_str(),
            scope.tenant().map(TenantId::into_uuid),
            role,
        )
        .execute(&self.pool)
        .await?;

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

    pub async fn revoke_membership(
        &self,
        identity_id: IdentityId,
        scope: Scope,
        actor: Actor,
    ) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE membership SET revoked_at = now()
              WHERE identity_id = $1
                AND tenant_id IS NOT DISTINCT FROM $2
                AND revoked_at IS NULL",
            identity_id.as_uuid(),
            scope.tenant().map(TenantId::into_uuid),
        )
        .execute(&self.pool)
        .await?;

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
        .await
    }

    async fn has_live_membership(
        &self,
        identity_id: IdentityId,
        tenant_id: TenantId,
    ) -> Result<bool, AccessError> {
        let found = sqlx::query_scalar!(
            "SELECT 1 FROM membership
              WHERE identity_id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
            identity_id.as_uuid(),
            tenant_id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn has_platform_membership(&self, identity_id: IdentityId) -> Result<bool, AccessError> {
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
fn tenant_from_row(
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
