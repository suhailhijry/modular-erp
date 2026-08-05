use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::{
    auth::{api_keys::MachinePrincipal, audience::Audience},
    event_sourcing::*,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Scale {
    None = 0,
    Read = 1,
    Edit = 2,
    Create = 3,
    Delete = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthzCondition {
    All(Vec<AuthzCondition>),
    Any(Vec<AuthzCondition>),
    Not(Box<AuthzCondition>),

    /// e.g. "custom discounts up to 500 SAR": AmountAtMostMinor(50_000).
    AmountAtMostMinor(i64),
    /// e.g. "only their own branch's documents".
    BranchIn(Vec<String>),
    HourBetween {
        from: u32,
        to_exclusive: u32,
    }, // wraps midnight, like the discount tree
    DayOfWeekIn(Vec<u32>), // 1=Mon..7=Sun
    /// Generic resource-attribute equality for facts that don't merit
    /// their own leaf yet: ResourceAttrEquals("department", "sales").
    ResourceAttrEquals {
        key: String,
        value: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct AuthzFacts {
    pub amount_minor: Option<i64>,
    pub branch_id: Option<String>,
    pub local_datetime: Option<chrono::NaiveDateTime>,
    pub resource_attrs: std::collections::BTreeMap<String, String>,
}

impl AuthzCondition {
    pub fn evaluate(&self, facts: &AuthzFacts) -> bool {
        match self {
            AuthzCondition::All(cs) => cs.iter().all(|c| c.evaluate(facts)),
            AuthzCondition::Any(cs) => cs.iter().any(|c| c.evaluate(facts)),
            AuthzCondition::Not(c) => !c.evaluate(facts),
            AuthzCondition::AmountAtMostMinor(max) => facts.amount_minor.is_some_and(|a| a <= *max),
            AuthzCondition::BranchIn(branches) => facts
                .branch_id
                .as_ref()
                .is_some_and(|b| branches.contains(b)),
            AuthzCondition::HourBetween { from, to_exclusive } => {
                facts.local_datetime.is_some_and(|dt| {
                    use chrono::Timelike;
                    let h = dt.hour();
                    if from <= to_exclusive {
                        h >= *from && h < *to_exclusive
                    } else {
                        h >= *from || h < *to_exclusive
                    }
                })
            }
            AuthzCondition::DayOfWeekIn(days) => facts.local_datetime.is_some_and(|dt| {
                use chrono::Datelike;
                days.contains(&dt.weekday().number_from_monday())
            }),
            AuthzCondition::ResourceAttrEquals { key, value } => {
                facts.resource_attrs.get(key).is_some_and(|v| v == value)
            }
        }
    }

    /// Same shape as the discount tree's validate: every problem in one
    /// round trip; enforced at Grant time, trusted thereafter.
    pub fn validate(&self, max_depth: usize, max_nodes: usize) -> Vec<String> {
        let mut problems = Vec::new();
        let mut nodes = 0usize;
        self.validate_inner(1, max_depth, &mut nodes, &mut problems);
        if nodes > max_nodes {
            problems.push(format!(
                "condition has {nodes} nodes, exceeding max {max_nodes}"
            ));
        }
        problems
    }

    fn validate_inner(
        &self,
        depth: usize,
        max_depth: usize,
        nodes: &mut usize,
        problems: &mut Vec<String>,
    ) {
        *nodes += 1;
        if depth > max_depth {
            problems.push(format!("condition exceeds max depth {max_depth}"));
            return;
        }
        match self {
            AuthzCondition::All(cs) | AuthzCondition::Any(cs) => {
                if cs.is_empty() {
                    problems.push("empty All/Any branch - vacuous condition".into());
                }
                for c in cs {
                    c.validate_inner(depth + 1, max_depth, nodes, problems);
                }
            }
            AuthzCondition::Not(c) => c.validate_inner(depth + 1, max_depth, nodes, problems),
            AuthzCondition::AmountAtMostMinor(v) => {
                if *v < 0 {
                    problems.push(format!("negative amount {v}"));
                }
            }
            AuthzCondition::BranchIn(b) => {
                if b.is_empty() {
                    problems.push("empty BranchIn".into());
                }
            }
            AuthzCondition::HourBetween { from, to_exclusive } => {
                if *from > 23 || *to_exclusive > 24 {
                    problems.push(format!("invalid hour range [{from}, {to_exclusive})"));
                }
                if from == to_exclusive {
                    problems.push("ambiguous hour range - omit for 'always'".into());
                }
            }
            AuthzCondition::DayOfWeekIn(d) => {
                if d.is_empty() {
                    problems.push("empty DayOfWeekIn".into());
                }
                for day in d {
                    if !(1..=7).contains(day) {
                        problems.push(format!("invalid weekday {day}"));
                    }
                }
            }
            AuthzCondition::ResourceAttrEquals { .. } => {}
        }
    }
}

/// A single grant: scale, optionally gated. condition: None = the
/// unconditional, enumerable RBAC baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalGrant {
    pub scale: Scale,
    pub condition: Option<AuthzCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "permission_grants")]
pub enum PermissionGrantsEvent {
    /// Appends a grant (a permission may hold several: one unconditional
    /// Read, one conditional Create).
    Granted {
        permission: String,
        grant: ConditionalGrant,
    },
    /// Removes ALL grants for the permission - revocation stays a
    /// one-event, fully-legible act.
    Revoked {
        permission: String,
    },
    GroupJoined {
        group_id: String,
    },
    GroupLeft {
        group_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "permission_grants")]
pub struct PermissionGrants {
    id: String,
    version: u64,
    grants: std::collections::BTreeMap<String, Vec<ConditionalGrant>>,
    groups: std::collections::BTreeSet<String>,
}

impl PermissionGrants {
    /// Best scale among grants whose condition passes under these facts.
    pub fn scale_of(&self, permission: &str, facts: &AuthzFacts) -> Scale {
        self.grants
            .get(permission)
            .map(|grants| {
                grants
                    .iter()
                    .filter(|g| g.condition.as_ref().map_or(true, |c| c.evaluate(facts)))
                    .map(|g| g.scale)
                    .max()
                    .unwrap_or(Scale::None)
            })
            .unwrap_or(Scale::None)
    }
    /// The audit view: everything granted, conditions verbatim - "who
    /// can X" degrades gracefully to "these subjects, N conditionally,
    /// here are the conditions".
    pub fn all_grants(&self) -> &std::collections::BTreeMap<String, Vec<ConditionalGrant>> {
        &self.grants
    }
    pub fn groups(&self) -> &std::collections::BTreeSet<String> {
        &self.groups
    }
}

#[derive(Debug, Clone)]
pub enum PermissionGrantsCommand {
    Grant {
        permission: String,
        grant: ConditionalGrant,
    },
    Revoke {
        permission: String,
    },
    JoinGroup {
        group_id: String,
    },
    LeaveGroup {
        group_id: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionGrantsError {
    #[error("invalid condition: {0}")]
    InvalidCondition(String),
    #[error("nothing to revoke")]
    NothingToRevoke,
    #[error("no change")]
    NoChange,
}

impl Aggregate for PermissionGrants {
    type Event = PermissionGrantsEvent;
    type Command = PermissionGrantsCommand;
    type Error = PermissionGrantsError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            PermissionGrantsEvent::Granted { permission, grant } => {
                self.grants
                    .entry(permission.clone())
                    .or_default()
                    .push(grant.clone());
            }
            PermissionGrantsEvent::Revoked { permission } => {
                self.grants.remove(permission);
            }
            PermissionGrantsEvent::GroupJoined { group_id } => {
                self.groups.insert(group_id.clone());
            }
            PermissionGrantsEvent::GroupLeft { group_id } => {
                self.groups.remove(group_id);
            }
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            PermissionGrantsCommand::Grant { permission, grant } => {
                if let Some(condition) = &grant.condition {
                    let problems = condition.validate(10, 100);
                    if !problems.is_empty() {
                        return Err(PermissionGrantsError::InvalidCondition(problems.join("; ")));
                    }
                }
                Ok(vec![PermissionGrantsEvent::Granted { permission, grant }])
            }
            PermissionGrantsCommand::Revoke { permission } => {
                if !self.grants.contains_key(&permission) {
                    return Err(PermissionGrantsError::NothingToRevoke);
                }
                Ok(vec![PermissionGrantsEvent::Revoked { permission }])
            }
            PermissionGrantsCommand::JoinGroup { group_id } => {
                if self.groups.contains(&group_id) {
                    return Err(PermissionGrantsError::NoChange);
                }
                Ok(vec![PermissionGrantsEvent::GroupJoined { group_id }])
            }
            PermissionGrantsCommand::LeaveGroup { group_id } => {
                if !self.groups.contains(&group_id) {
                    return Err(PermissionGrantsError::NoChange);
                }
                Ok(vec![PermissionGrantsEvent::GroupLeft { group_id }])
            }
        }
    }
}

/// /// Resource ACL: id = "{resource_type}:{resource_id}". Created in the
/// SAME Context as the resource itself - no ownerless window.
#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "resource_acl")]
pub enum ResourceAclEvent {
    Created {
        owner_identity_id: String,
    },
    CoOwnerAdded {
        identity_id: String,
    },
    CoOwnerRemoved {
        identity_id: String,
    },
    /// Explicit per-identity override - the sharing mechanism.
    IdentityScaleSet {
        identity_id: String,
        scale: Scale,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "resource_acl")]
pub struct ResourceAcl {
    id: String,
    version: u64,
    owner: String,
    co_owners: BTreeSet<String>,
    overrides: BTreeMap<String, Scale>,
}

pub enum Relation {
    Owner,
    CoOwner,
    Granted(Scale),
    None,
}

impl ResourceAcl {
    pub fn relation(&self, identity_id: &str) -> Relation {
        if self.owner == identity_id {
            Relation::Owner
        } else if self.co_owners.contains(identity_id) {
            Relation::CoOwner
        } else if let Some(s) = self.overrides.get(identity_id) {
            Relation::Granted(*s)
        } else {
            Relation::None
        }
    }

    pub fn owner(&self) -> &str {
        self.owner.as_ref()
    }

    pub fn co_owners(&self) -> &BTreeSet<String> {
        &self.co_owners
    }

    pub fn overrides(&self) -> &BTreeMap<String, Scale> {
        &self.overrides
    }
}

#[derive(Debug, Clone)]
pub enum ResourceAclCommand {
    /// Queued into the SAME Context as the resource's creation event by
    /// the creating orchestration - no ownerless window.
    Create {
        owner_identity_id: String,
    },
    AddCoOwner {
        identity_id: String,
    },
    RemoveCoOwner {
        identity_id: String,
    },
    SetIdentityScale {
        identity_id: String,
        scale: Scale,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceAclError {
    #[error("acl already exists for this resource")]
    AlreadyExists,
    #[error("acl does not exist")]
    NotFound,
    #[error(
        "the owner's relation cannot be edited - transfer ownership is a separate, deliberate operation"
    )]
    CannotEditOwner,
    #[error("no change")]
    NoChange,
}

impl Aggregate for ResourceAcl {
    type Event = ResourceAclEvent;
    type Command = ResourceAclCommand;
    type Error = ResourceAclError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ResourceAclEvent::Created { owner_identity_id } => {
                self.owner = owner_identity_id.clone()
            }
            ResourceAclEvent::CoOwnerAdded { identity_id } => {
                self.co_owners.insert(identity_id.clone());
            }
            ResourceAclEvent::CoOwnerRemoved { identity_id } => {
                self.co_owners.remove(identity_id);
            }
            ResourceAclEvent::IdentityScaleSet { identity_id, scale } => {
                if *scale == Scale::None {
                    self.overrides.remove(identity_id); // None = revoke the override
                } else {
                    self.overrides.insert(identity_id.clone(), *scale);
                }
            }
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            ResourceAclCommand::Create { owner_identity_id } => {
                if self.version != 0 {
                    return Err(ResourceAclError::AlreadyExists);
                }
                Ok(vec![ResourceAclEvent::Created { owner_identity_id }])
            }
            ResourceAclCommand::AddCoOwner { identity_id } => {
                if self.version == 0 {
                    return Err(ResourceAclError::NotFound);
                }
                if identity_id == self.owner {
                    return Err(ResourceAclError::CannotEditOwner);
                }
                if self.co_owners.contains(&identity_id) {
                    return Err(ResourceAclError::NoChange);
                }
                Ok(vec![ResourceAclEvent::CoOwnerAdded { identity_id }])
            }
            ResourceAclCommand::RemoveCoOwner { identity_id } => {
                if self.version == 0 {
                    return Err(ResourceAclError::NotFound);
                }
                if !self.co_owners.contains(&identity_id) {
                    return Err(ResourceAclError::NoChange);
                }
                Ok(vec![ResourceAclEvent::CoOwnerRemoved { identity_id }])
            }
            ResourceAclCommand::SetIdentityScale { identity_id, scale } => {
                if self.version == 0 {
                    return Err(ResourceAclError::NotFound);
                }
                if identity_id == self.owner {
                    return Err(ResourceAclError::CannotEditOwner);
                }
                Ok(vec![ResourceAclEvent::IdentityScaleSet {
                    identity_id,
                    scale,
                }])
            }
        }
    }
}

/// Per resource-type policy: what a relation is worth, and which global
/// permission governs the type. Config, not per-resource data.
#[derive(Debug, Clone, Copy)]
pub struct ResourceTypePolicy {
    pub permission_name: &'static str,
    pub owner_scale: Scale,
    pub co_owner_scale: Scale,
}

///   let policies = ResourcePolicyRegistry::new()
///       .with(crate::invoice_api::resource_policies())
///       .with(crate::accounting_api::resource_policies());
///
/// Registration is startup-time and panics on duplicates - two domains
/// claiming the same resource type is a wiring bug that must never
/// reach serving traffic.
#[derive(Default)]
pub struct ResourcePolicyRegistry {
    map: std::collections::BTreeMap<&'static str, ResourceTypePolicy>,
}

impl ResourcePolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, entries: Vec<(&'static str, ResourceTypePolicy)>) -> Self {
        for (resource_type, policy) in entries {
            if self.map.insert(resource_type, policy).is_some() {
                panic!("resource type '{resource_type}' registered by two domains - wiring bug");
            }
        }
        self
    }
    /// None = unknown type. Callers FAIL CLOSED on it - an unregistered
    /// resource type grants nothing to anyone (except system identities,
    /// whose bypass precedes policy lookup).
    pub fn get(&self, resource_type: &str) -> Option<ResourceTypePolicy> {
        self.map.get(resource_type).copied()
    }
    pub fn known_types(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.map.keys().copied()
    }
}

pub struct Authorizer {
    pub store: Arc<dyn EventStore>,
    pub policies: Arc<ResourcePolicyRegistry>,
}

impl Authorizer {
    /// Global check. SUPERADMIN RULE (as specified): system users bypass
    /// permission checks ENTIRELY, conditions included - the bypass
    /// fires before any grant or condition is even loaded. The only
    /// thing that stops a system user is an endpoint that structurally
    /// refuses, with no permission involved at all.
    pub async fn require(
        &self,
        auth: &AuthContext,
        permission: &str,
        needed: Scale,
        facts: &AuthzFacts,
    ) -> Result<(), AuthzError> {
        if auth.is_system {
            return Ok(());
        }
        if self
            .effective_scale(&auth.identity_id, permission, facts)
            .await?
            >= needed
        {
            return Ok(());
        }
        Err(AuthzError::Forbidden {
            permission: permission.into(),
            needed,
        })
    }

    /// Resource-aware check: ownership first, then global RBAC.
    pub async fn require_on(
        &self,
        auth: &AuthContext,
        resource_type: &str,
        resource_id: &str,
        needed: Scale,
        facts: &AuthzFacts,
    ) -> Result<(), AuthzError> {
        if auth.is_system {
            return Ok(());
        }
        // Unknown resource type = deny for everyone (fail closed) and
        // log loudly: it means a domain forgot to register its policy,
        // which should surface as 403s + errors in dev, not silent
        // default permissions in prod.
        let Some(policy) = self.policies.get(resource_type) else {
            tracing::error!(
                resource_type,
                "authorization check against UNREGISTERED resource type - domain forgot to register its ResourceTypePolicy"
            );
            return Err(AuthzError::UnknownResourceType(resource_type.to_string()));
        };
        let acl = load_aggregate::<ResourceAcl>(
            self.store.as_ref(),
            &format!("{resource_type}:{resource_id}"),
        )
        .await
        .map_err(|e| AuthzError::Internal(e.into()))?;
        let granted_by_relation = match acl.relation(&auth.identity_id) {
            Relation::Owner => policy.owner_scale,
            Relation::CoOwner => policy.co_owner_scale,
            Relation::Granted(s) => s,
            Relation::None => Scale::None,
        };
        if granted_by_relation >= needed {
            return Ok(()); // step 2: relation to the resource suffices
        }
        // step 3: fall through to global (conditional) RBAC
        self.require(auth, policy.permission_name, needed, facts)
            .await
    }

    /// max over own grants and each group's grants; every subject's
    /// conditions judged against the SAME facts snapshot.
    async fn effective_scale(
        &self,
        identity_id: &str,
        permission: &str,
        facts: &AuthzFacts,
    ) -> Result<Scale, AuthzError> {
        let user_grants = load_aggregate::<PermissionGrants>(
            self.store.as_ref(),
            &format!("perms:identity:{identity_id}"),
        )
        .await
        .map_err(|e| AuthzError::Internal(e.into()))?;
        let mut best = user_grants.scale_of(permission, facts);
        for group in user_grants.groups() {
            let g = load_aggregate::<PermissionGrants>(
                self.store.as_ref(),
                &format!("perms:group:{group}"),
            )
            .await
            .map_err(|e| AuthzError::Internal(e.into()))?;
            best = best.max(g.scale_of(permission, facts));
        }
        Ok(best)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthzError {
    #[error("forbidden: needs {permission} at {needed:?}")]
    Forbidden { permission: String, needed: Scale },
    #[error("unknown resource type '{0}' - no domain registered a policy for it")]
    UnknownResourceType(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

#[derive(Clone)]
pub struct MachineContext {
    pub api_key_id: String,
    pub principal: MachinePrincipal,
    pub scopes: Vec<String>,
}

#[derive(Clone)]
pub struct AuthContext {
    pub identity_id: String,
    pub audience: Audience,
    pub is_system: bool,
    pub session_id: String,
}

#[derive(Clone)]
pub struct EmployeeContext {
    pub employee_id: String,
}

#[derive(Clone)]
pub struct ClientContext {
    pub client_id: String,
}
