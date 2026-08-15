//! Applying a tenant-plane migration to every tenant that already exists.
//!
//! # Why this has to exist before the next migration does
//!
//! `provision` runs the tenant migrations when it builds a database, and
//! nothing has ever run them again. So the day `migrations/tenant/0004_*.sql`
//! ships, new tenants get it and **every existing tenant does not** — while the
//! code that needs it is deployed to all of them. Queries compile (they are
//! checked against a database that has the migration) and fail at runtime,
//! per tenant, on the live fleet.
//!
//! At two tenants that is a manual `psql`. At the two to five thousand this is
//! sized for it is an outage.
//!
//! # What makes it safe to run
//!
//! - **Idempotent.** sqlx records what it has applied in each tenant database,
//!   so a tenant already current is a no-op and a re-run is the resume.
//! - **It does not stop.** One unreachable cluster must not leave the rest of
//!   the fleet un-migrated; failures are collected and reported, and the next
//!   run retries them.
//! - **It can look without touching.** [`FleetPlan`] answers "who is behind?"
//!   with no writes, which is what you run *before* deploying rather than after.

use spa_types::TenantId;
use sqlx::Connection;
use sqlx::postgres::PgConnection;

use crate::{AccessError, ControlPlane};

/// Where one tenant's database stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantSchema {
    pub tenant: TenantId,
    pub slug: String,
    /// Highest migration version applied, or `None` for a database with no
    /// migrations table at all — which means something built it outside
    /// `provision`.
    pub version: Option<i64>,
}

impl TenantSchema {
    /// Whether this tenant has everything this build expects.
    #[must_use]
    pub fn is_current(&self, latest: i64) -> bool {
        self.version == Some(latest)
    }
}

/// What a fleet migration found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetPlan {
    /// Tenants already at the latest version.
    pub current: Vec<TenantSchema>,
    /// Tenants that need work — or, after a run, that got it.
    pub behind: Vec<TenantSchema>,
    /// Tenants that could not be reached or migrated, with why. **Not an error
    /// for the run**: the next one retries them.
    pub failed: Vec<(TenantId, String)>,
}

impl FleetPlan {
    #[must_use]
    pub fn total(&self) -> usize {
        self.current.len() + self.behind.len() + self.failed.len()
    }

    /// Whether the whole fleet is at the version this build expects.
    ///
    /// The question a deploy gate asks. A failure counts as "no" — an
    /// unreachable tenant is not a migrated one.
    #[must_use]
    pub fn is_uniform(&self) -> bool {
        self.behind.is_empty() && self.failed.is_empty()
    }
}

impl ControlPlane {
    /// The migration version this build expects every tenant to be at.
    #[must_use]
    pub fn latest_tenant_migration() -> i64 {
        spa_eventlog::MIGRATIONS
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or(0)
    }

    /// Reads every tenant's schema version without changing anything.
    ///
    /// ponytail: sequential, one connection at a time. A few thousand tenants
    /// is a few minutes, which is fine for a deploy step and not fine for a
    /// health check on a timer — parallelise it when something wants it often.
    pub async fn survey_fleet(&self) -> Result<FleetPlan, AccessError> {
        self.walk_fleet(false).await
    }

    /// Applies pending tenant-plane migrations across the fleet.
    ///
    /// Returns what it found and what it did. Tenants that were already current
    /// appear in `current`; ones that needed migrating appear in `behind`,
    /// having been migrated.
    pub async fn migrate_fleet(&self) -> Result<FleetPlan, AccessError> {
        self.walk_fleet(true).await
    }

    async fn walk_fleet(&self, apply: bool) -> Result<FleetPlan, AccessError> {
        let latest = Self::latest_tenant_migration();
        let tenants = self.tenants_with_databases().await?;
        let mut plan = FleetPlan::default();

        for tenant in tenants {
            match self.visit(&tenant, latest, apply).await {
                Ok(schema) if schema.is_current(latest) => plan.current.push(schema),
                Ok(schema) => plan.behind.push(schema),
                // Collected, not returned: one unreachable cluster must not
                // leave the rest of the fleet un-migrated.
                Err(e) => {
                    tracing::error!(
                        tenant = %tenant.id,
                        slug = %tenant.slug,
                        error = %e,
                        "could not migrate a tenant; the next run will retry it"
                    );
                    plan.failed.push((tenant.id, e.to_string()));
                }
            }
        }

        tracing::info!(
            total = plan.total(),
            current = plan.current.len(),
            behind = plan.behind.len(),
            failed = plan.failed.len(),
            latest,
            applied = apply,
            "fleet survey finished"
        );

        Ok(plan)
    }

    /// One tenant: read its version, and migrate it if asked.
    ///
    /// The version reported is the one *before* migrating, so a caller can see
    /// what was behind rather than a fleet that is uniformly current and no
    /// record of what changed.
    async fn visit(
        &self,
        tenant: &crate::model::Tenant,
        latest: i64,
        apply: bool,
    ) -> Result<TenantSchema, AccessError> {
        let options = self
            .tenants
            .cluster_options(&tenant.cluster)?
            .database(&tenant.database_name);

        let mut conn = PgConnection::connect_with(&options)
            .await
            .map_err(AccessError::Database)?;

        let version = applied_version(&mut conn).await?;
        let schema = TenantSchema {
            tenant: tenant.id,
            slug: tenant.slug.clone(),
            version,
        };

        if apply && !schema.is_current(latest) {
            // By value, then dropped — `Migrator::run` is generic over
            // `Acquire<'_>` and a borrow of it here would put that bound in
            // this future. See `provision.rs`.
            let conn = crate::provision::migrate(conn)
                .await
                .map_err(|e| AccessError::Corrupt(format!("tenant migrations: {e}")))?;
            conn.close().await.ok();

            tracing::info!(
                tenant = %tenant.id,
                slug = %tenant.slug,
                from = ?version,
                to = latest,
                "migrated a tenant database"
            );
        } else {
            conn.close().await.ok();
        }

        Ok(schema)
    }

    /// Every tenant whose database should exist.
    ///
    /// Deliberately includes suspended ones: a suspended tenant is a tenant
    /// that may come back, and coming back to a schema three versions behind is
    /// the failure this whole file is about. `provisioning` and `deleted` are
    /// skipped — the first has no finished database and the second has none at
    /// all.
    async fn tenants_with_databases(&self) -> Result<Vec<crate::model::Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT id, slug, display_name, status, cluster,
                      database_name, demo_expires_at, created_at
                 FROM tenant
                WHERE status IN ('active', 'suspended')
                ORDER BY created_at"#
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                crate::tenant_from_row(
                    TenantId::from_uuid(row.id),
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
}

/// The highest migration a tenant database has applied.
///
/// `None` when there is no `_sqlx_migrations` table, which means the database
/// was not built by `provision` — reported rather than treated as version zero,
/// because "never migrated" and "built by something else" want different
/// answers from an operator.
async fn applied_version(conn: &mut PgConnection) -> Result<Option<i64>, AccessError> {
    let has_table: Option<bool> =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .map_err(AccessError::Database)?;

    if has_table != Some(true) {
        return Ok(None);
    }

    sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
        .fetch_one(&mut *conn)
        .await
        .map_err(AccessError::Database)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_is_current_only_at_the_exact_latest_version() {
        let schema = |version| TenantSchema {
            tenant: TenantId::new(),
            slug: "acme".to_owned(),
            version,
        };

        assert!(schema(Some(3)).is_current(3));
        assert!(!schema(Some(2)).is_current(3));
        // A tenant *ahead* of this build is not current either: it was migrated
        // by a newer deploy, and this one's queries may not match it.
        assert!(!schema(Some(4)).is_current(3));
        assert!(!schema(None).is_current(3));
    }

    #[test]
    fn a_failure_means_the_fleet_is_not_uniform() {
        let mut plan = FleetPlan::default();
        assert!(plan.is_uniform(), "an empty fleet is trivially uniform");

        plan.failed
            .push((TenantId::new(), "unreachable".to_owned()));
        assert!(
            !plan.is_uniform(),
            "a tenant nobody could reach is not a migrated tenant"
        );
    }

    #[test]
    fn this_build_expects_a_real_migration_version() {
        // Guards against the `unwrap_or(0)` quietly becoming the answer, which
        // would make every tenant look current forever.
        assert!(ControlPlane::latest_tenant_migration() > 0);
    }
}
