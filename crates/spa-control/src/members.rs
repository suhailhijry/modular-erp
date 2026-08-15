//! Who else has access to a tenant.
//!
//! # Why this is a password and not an invitation
//!
//! The polished flow is an emailed link: a token, an expiry, and the new
//! colleague choosing their own password. That needs email delivery, which needs
//! an outbox handler nothing has written yet — so it would be a feature that
//! looks finished and delivers nothing.
//!
//! What ships instead is how small businesses actually onboard staff: the owner
//! creates the account and hands over the credentials. It works today, needs no
//! infrastructure, and the invitation flow layers on top rather than replacing
//! it — [`ControlPlane::add_member`] is still what an accepted invitation calls.
//!
//! ponytail: build invitations when someone is onboarding enough people that
//! saying the password out loud is the problem.

use spa_types::{IdentityId, TenantId, Timestamp};

use crate::model::{Actor, Scope};
use crate::{AccessError, ControlPlane, Role};

/// Someone with access to a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub identity: IdentityId,
    /// Where this person's role differs from their tenant-wide one.
    pub module_roles: Vec<(spa_types::ModuleId, Role)>,
    /// The login handle. `None` for an identity with no password authenticator
    /// — which today means one created some other way, and later an invitation
    /// nobody has accepted.
    pub handle: Option<String>,
    pub role: Role,
    pub since: Timestamp,
    /// Whether the identity itself is suspended, which overrides any role.
    pub suspended: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    #[error("{0} already has access to this tenant")]
    AlreadyAMember(String),
    /// The identity named is not a member of *this* tenant.
    ///
    /// A 404 rather than a silent success. Changing or removing a stranger used
    /// to update no rows and answer `204`, which told an owner who mistyped an
    /// id that something had happened when nothing had.
    #[error("that identity is not a member of this tenant")]
    NotAMember,
    /// The last owner cannot be removed or demoted.
    ///
    /// Not paternalism: a tenant with no owner has nobody who can add one, and
    /// the only fix is a support ticket. The check costs one query.
    #[error("a tenant must keep at least one owner")]
    LastOwner,
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] crate::AuthError),
}

impl spa_i18n::Localize for MemberError {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::{Message, MessageArg};
        match self {
            Self::AlreadyAMember(handle) => Message::new(messages::ALREADY_A_MEMBER)
                .with("handle", MessageArg::text(handle.clone())),
            Self::NotAMember => Message::new(messages::NOT_A_MEMBER),
            Self::LastOwner => Message::new(messages::LAST_OWNER),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}

impl ControlPlane {
    /// Everyone with access to a tenant.
    pub async fn members(&self, tenant_id: TenantId) -> Result<Vec<Member>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT m.identity_id as "identity: IdentityId",
                      m.role,
                      m.created_at,
                      i.status,
                      (SELECT a.handle FROM authenticator a
                        WHERE a.identity_id = m.identity_id AND a.kind = 'password'
                        LIMIT 1) as handle,
                      COALESCE(
                          (SELECT array_agg(r.module_id || '=' || r.role ORDER BY r.module_id)
                             FROM membership_module_role r
                            WHERE r.membership_id = m.id),
                          '{}'
                      ) as "module_roles!: Vec<String>"
                 FROM membership m
                 JOIN identity i ON i.id = m.identity_id
                WHERE m.tenant_id = $1 AND m.revoked_at IS NULL
                ORDER BY m.created_at"#,
            tenant_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(Member {
                    identity: row.identity,
                    handle: row.handle,
                    module_roles: row
                        .module_roles
                        .iter()
                        .map(|pair| parse_module_role(pair))
                        .collect::<Result<_, _>>()?,
                    // A role this build cannot read is an error, not a guess.
                    role: row
                        .role
                        .parse::<Role>()
                        .map_err(|e| AccessError::Corrupt(e.to_string()))?,
                    since: row.created_at,
                    suspended: row.status != "active",
                })
            })
            .collect()
    }

    /// Gives someone access, creating their account if they do not have one.
    ///
    /// Idempotent in the direction that matters: an existing identity is
    /// reused, so adding `owner@acme.test` to a second tenant does not make a
    /// second account for them. Adding them to a tenant they are already in is
    /// refused — the caller almost certainly meant [`Self::change_role`].
    pub async fn add_member(
        &self,
        tenant_id: TenantId,
        handle: String,
        password: String,
        role: Role,
        actor: Actor,
    ) -> Result<IdentityId, MemberError> {
        let handle = handle.trim().to_lowercase();

        let existing = sqlx::query_scalar!(
            r#"SELECT identity_id as "identity: IdentityId" FROM authenticator
                WHERE kind = 'password' AND handle = $1"#,
            handle,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?;

        let identity = if let Some(identity) = existing {
            if self.cached_membership(identity, tenant_id).await?.is_some() {
                return Err(MemberError::AlreadyAMember(handle));
            }
            identity
        } else {
            let created = self.create_identity(actor).await?;
            // The password is the caller's to choose and to hand over — which is
            // what invitations exist to avoid. Registering rather than
            // upserting: the branch above already established this handle is
            // free, and a race that says otherwise must fail rather than
            // overwrite somebody.
            self.register_login(created.id, handle.clone(), password)
                .await?;
            created.id
        };

        self.grant_membership(identity, Scope::Tenant(tenant_id), role.as_str(), actor)
            .await?;
        Ok(identity)
    }

    /// Changes what someone may do.
    ///
    /// Refuses to demote the last owner, for the same reason
    /// [`Self::remove_member`] refuses to remove them.
    pub async fn change_role(
        &self,
        tenant_id: TenantId,
        identity: IdentityId,
        role: Role,
        actor: Actor,
    ) -> Result<(), MemberError> {
        if role != Role::Owner && self.is_last_owner(tenant_id, identity).await? {
            return Err(MemberError::LastOwner);
        }

        let changed = sqlx::query!(
            "UPDATE membership SET role = $3
              WHERE tenant_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
            tenant_id.as_uuid(),
            identity.as_uuid(),
            role.as_str(),
        )
        .execute(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .rows_affected();

        if changed == 0 {
            return Err(MemberError::NotAMember);
        }

        // Now, not after the TTL: a demotion that takes five seconds to apply is
        // five seconds of someone doing what they were just told they cannot.
        self.memberships.invalidate(&(identity, tenant_id));

        self.record(
            actor,
            "membership.role_changed",
            "identity",
            &identity.to_string(),
            serde_json::json!({ "tenant": tenant_id.to_string(), "role": role.as_str() }),
        )
        .await?;
        Ok(())
    }

    /// Takes access away, keeping the identity and its history.
    pub async fn remove_member(
        &self,
        tenant_id: TenantId,
        identity: IdentityId,
        actor: Actor,
    ) -> Result<(), MemberError> {
        if self.is_last_owner(tenant_id, identity).await? {
            return Err(MemberError::LastOwner);
        }
        if !self
            .revoke_membership(identity, Scope::Tenant(tenant_id), actor)
            .await?
        {
            return Err(MemberError::NotAMember);
        }
        Ok(())
    }

    /// Gives somebody a different role in one module, or clears the exception.
    ///
    /// `None` removes the override, putting them back on their tenant-wide
    /// role — which is a different thing from setting them to `Viewer` there,
    /// and the difference matters when the tenant-wide role later changes.
    pub async fn set_module_role(
        &self,
        tenant_id: TenantId,
        identity: IdentityId,
        module: &spa_types::ModuleId,
        role: Option<Role>,
        actor: Actor,
    ) -> Result<(), MemberError> {
        let membership = sqlx::query_scalar!(
            "SELECT id FROM membership
              WHERE tenant_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
            tenant_id.as_uuid(),
            identity.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .ok_or(MemberError::NotAMember)?;

        match role {
            Some(role) => {
                sqlx::query!(
                    "INSERT INTO membership_module_role (membership_id, module_id, role)
                     VALUES ($1, $2, $3)
                     ON CONFLICT (membership_id, module_id)
                     DO UPDATE SET role = EXCLUDED.role, set_at = now()",
                    membership,
                    module.as_str(),
                    role.as_str(),
                )
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?;
            }
            None => {
                sqlx::query!(
                    "DELETE FROM membership_module_role
                      WHERE membership_id = $1 AND module_id = $2",
                    membership,
                    module.as_str(),
                )
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?;
            }
        }

        // Now, not after the TTL — same reason a demotion is invalidated at
        // once: the seconds in between are seconds of somebody doing what they
        // have just been told they cannot.
        self.memberships.invalidate(&(identity, tenant_id));

        self.record(
            actor,
            "membership.module_role_changed",
            "identity",
            &identity.to_string(),
            serde_json::json!({
                "tenant": tenant_id.to_string(),
                "module": module.as_str(),
                "role": role.map(Role::as_str),
            }),
        )
        .await?;
        Ok(())
    }

    /// Whether removing or demoting this identity would leave the tenant
    /// ownerless.
    async fn is_last_owner(
        &self,
        tenant_id: TenantId,
        identity: IdentityId,
    ) -> Result<bool, AccessError> {
        let others = sqlx::query_scalar!(
            r#"SELECT count(*) as "count!" FROM membership
                WHERE tenant_id = $1
                  AND identity_id <> $2
                  AND role = 'owner'
                  AND revoked_at IS NULL"#,
            tenant_id.as_uuid(),
            identity.as_uuid(),
        )
        .fetch_one(&self.pool)
        .await?;

        // Only a problem if *this* identity is currently an owner: demoting a
        // clerk in a tenant with one owner is fine.
        let is_owner = self
            .cached_membership(identity, tenant_id)
            .await?
            .is_some_and(|access| access.role == Role::Owner);
        Ok(is_owner && others == 0)
    }
}

/// One `module=role` pair, as [`ControlPlane::members`] reads them back.
///
/// Validated rather than trusted: this is data coming *out* of the database,
/// which is where values written by an older version of the system arrive.
fn parse_module_role(pair: &str) -> Result<(spa_types::ModuleId, Role), AccessError> {
    let (module, role) = pair
        .split_once('=')
        .ok_or_else(|| AccessError::Corrupt(format!("module role {pair:?}")))?;
    Ok((
        spa_types::ModuleId::new(module.to_owned())
            .map_err(|e| AccessError::Corrupt(format!("module role: {e}")))?,
        role.parse::<Role>()
            .map_err(|e| AccessError::Corrupt(e.to_string()))?,
    ))
}
