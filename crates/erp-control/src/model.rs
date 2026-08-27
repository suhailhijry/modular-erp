//! Control-plane records.

use erp_types::{IdentityId, MembershipId, ModuleId, TenantId, Timestamp};
use serde::{Deserialize, Serialize};

/// Where a membership grants entry.
///
/// A single enum rather than a nullable tenant field, so "platform membership
/// with a tenant id" is not a state anyone has to check for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "tenant")]
pub enum Scope {
    /// Our own staff, including superadmins.
    Platform,
    /// A customer's system.
    Tenant(TenantId),
}

impl Scope {
    #[must_use]
    pub const fn tenant(self) -> Option<TenantId> {
        match self {
            Self::Platform => None,
            Self::Tenant(id) => Some(id),
        }
    }

    pub(crate) const fn kind_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Tenant(_) => "tenant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStatus {
    Active,
    Suspended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: IdentityId,
    pub status: IdentityStatus,
    pub created_at: Timestamp,
}

impl Identity {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.status, IdentityStatus::Active)
    }
}

/// Lifecycle of a tenant's system.
///
/// `Provisioning` is a real state, not a transient one: signup returns
/// immediately and the provisioner works in the background, so a tenant is
/// visible-but-not-yet-enterable for a few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Provisioning,
    Active,
    Suspended,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub slug: String,
    pub display_name: String,
    pub status: TenantStatus,
    /// Which Postgres cluster holds this tenant's database.
    pub cluster: String,
    pub database_name: String,
    /// Set for demo tenants; `None` for real ones.
    pub demo_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

impl Tenant {
    /// Only an active tenant may be entered. Provisioning is not ready,
    /// suspended is deliberate, deleted is gone.
    #[must_use]
    pub const fn is_enterable(&self) -> bool {
        matches!(self.status, TenantStatus::Active)
    }

    #[must_use]
    pub const fn is_demo(&self) -> bool {
        self.demo_expires_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub id: MembershipId,
    pub identity_id: IdentityId,
    pub scope: Scope,
    pub role: String,
    pub created_at: Timestamp,
}

/// A module a tenant currently has switched on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    pub tenant_id: TenantId,
    pub module_id: ModuleId,
    pub enabled_at: Timestamp,
}

/// The set of modules live for a tenant, resolved once and carried on the
/// [`TenantDb`](crate::TenantDb) handle.
///
/// Sorted, so equality and logging are stable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnabledModules(Vec<ModuleId>);

impl EnabledModules {
    #[must_use]
    pub fn new(mut modules: Vec<ModuleId>) -> Self {
        modules.sort();
        modules.dedup();
        Self(modules)
    }

    #[must_use]
    pub fn contains(&self, module: &ModuleId) -> bool {
        self.0.binary_search(module).is_ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModuleId> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Who is acting, for audit purposes.
///
/// `on_behalf_of` is how support access is recorded: both the staff member and
/// the tenant identity are named, so an impersonated action is never
/// indistinguishable from one the tenant took themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Actor {
    pub identity: Option<IdentityId>,
    pub on_behalf_of: Option<IdentityId>,
}

impl Actor {
    /// An action taken by the platform itself — a provisioner, a reaper, a
    /// scheduled job. Deliberately explicit rather than a bare `None`, so an
    /// unattributed audit row is a choice someone made.
    #[must_use]
    pub const fn system() -> Self {
        Self {
            identity: None,
            on_behalf_of: None,
        }
    }

    #[must_use]
    pub const fn identity(id: IdentityId) -> Self {
        Self {
            identity: Some(id),
            on_behalf_of: None,
        }
    }

    #[must_use]
    pub const fn impersonating(staff: IdentityId, subject: IdentityId) -> Self {
        Self {
            identity: Some(staff),
            on_behalf_of: Some(subject),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_carries_its_tenant_or_none() {
        let tenant = TenantId::new();
        assert_eq!(Scope::Tenant(tenant).tenant(), Some(tenant));
        assert_eq!(Scope::Platform.tenant(), None);
    }

    #[test]
    fn enabled_modules_are_sorted_and_deduplicated() {
        let ledger = ModuleId::new("ledger").unwrap();
        let invoicing = ModuleId::new("invoicing").unwrap();
        let modules = EnabledModules::new(vec![ledger.clone(), invoicing.clone(), ledger.clone()]);
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&ledger));
        assert!(modules.contains(&invoicing));
        assert!(!modules.contains(&ModuleId::new("inventory").unwrap()));
        // Sorted, so two equal sets built in different orders compare equal.
        assert_eq!(modules, EnabledModules::new(vec![invoicing, ledger]));
    }

    #[test]
    fn only_active_tenants_may_be_entered() {
        let base = Tenant {
            id: TenantId::new(),
            slug: "acme".into(),
            display_name: "Acme".into(),
            status: TenantStatus::Active,
            cluster: "primary".into(),
            database_name: "erp_tenant_x".into(),
            demo_expires_at: None,
            created_at: chrono::Utc::now(),
        };
        assert!(base.is_enterable());
        for status in [
            TenantStatus::Provisioning,
            TenantStatus::Suspended,
            TenantStatus::Deleted,
        ] {
            let tenant = Tenant {
                status,
                ..base.clone()
            };
            assert!(!tenant.is_enterable(), "{status:?} must not be enterable");
        }
    }
}
