//! Platform staff: the people who run the platform rather than a tenant, and
//! what each of them may do.
//!
//! # Why a second vocabulary
//!
//! A tenant's [`Role`](crate::Role) answers what somebody may do inside one
//! company's books. Staff answer a different question — may they suspend a
//! tenant, read the platform's audit trail, open a tenant for support — and
//! forcing them through the tenant enum would let `support` answer questions
//! about a ledger. So the platform has its own closed pair, [`PlatformRole`] and
//! [`PlatformPower`], and [`PlatformRole::may`] is where a platform power is
//! decided. [`ControlPlane::staff_may`] is the door that asks it, and every
//! platform door goes through that one: `erp_web::Staff<P>` for the HTTP routes
//! and [`ControlPlane::enter_for_support`] for support access.
//!
//! # The first superadmin
//!
//! Nothing over HTTP can make one — every staff route needs a superadmin
//! already. `bin/operator grant-staff` is the bootstrap, and
//! `operator revoke-staff` is the way out when the last superadmin is the
//! problem, which is why only the HTTP path refuses to remove the last one.

use std::str::FromStr;

use erp_types::{IdentityId, Timestamp};

use crate::model::{Actor, Scope};
use crate::{AccessError, ControlPlane, UnknownRole};

/// What a member of platform staff is.
///
/// Split by job rather than ranked by seniority, so each role can do what its
/// job needs and nothing its job does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformRole {
    /// Answers tickets: reads the platform's audit trail, handles the control
    /// plane's dead letters, and opens a tenant for support.
    Support,
    /// Suspends and reinstates tenants. Nothing inside any tenant.
    Billing,
    /// Everything, including who else is staff.
    Superadmin,
}

/// Something only platform staff may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformPower {
    /// Suspend a tenant, and reinstate one.
    SuspendTenants,
    /// List, requeue and dismiss the control plane's dead letters.
    HandleDeadLetters,
    /// Read the platform's audit trail.
    ReadAuditTrail,
    /// Open a tenant's books for support — [`ControlPlane::enter_for_support`].
    EnterForSupport,
    /// Grant, change and revoke platform staff.
    ManageStaff,
    /// Reset anybody's second factor, with a reason —
    /// [`ControlPlane::reset_any_second_factor`]. Support and superadmin, for
    /// the person who works for two companies and so cannot be reset by either
    /// of them. Resetting *staff* needs [`Self::ManageStaff`] on top, so
    /// support cannot reset a superadmin's.
    ResetSecondFactors,
}

impl PlatformPower {
    /// Every power, for tests.
    pub const ALL: [Self; 6] = [
        Self::SuspendTenants,
        Self::HandleDeadLetters,
        Self::ReadAuditTrail,
        Self::EnterForSupport,
        Self::ManageStaff,
        Self::ResetSecondFactors,
    ];

    /// For error messages and audit lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SuspendTenants => "suspend_tenants",
            Self::HandleDeadLetters => "handle_dead_letters",
            Self::ReadAuditTrail => "read_audit_trail",
            Self::EnterForSupport => "enter_for_support",
            Self::ManageStaff => "manage_staff",
            Self::ResetSecondFactors => "reset_second_factors",
        }
    }
}

impl PlatformRole {
    /// Every role, for tests and for the document's list of them.
    pub const ALL: [Self; 3] = [Self::Support, Self::Billing, Self::Superadmin];

    /// **The one place a platform power is decided.**
    ///
    /// The matrix is the product owner's, from 2026-09-11. `every_role_may_
    /// exactly_what_it_should` restates it rather than deriving it, and the
    /// HTTP matrix in `erp-api` checks that the doors honour it.
    #[must_use]
    pub const fn may(self, power: PlatformPower) -> bool {
        match self {
            Self::Superadmin => true,
            Self::Billing => matches!(power, PlatformPower::SuspendTenants),
            Self::Support => matches!(
                power,
                PlatformPower::ReadAuditTrail
                    | PlatformPower::HandleDeadLetters
                    | PlatformPower::EnterForSupport
                    | PlatformPower::ResetSecondFactors
            ),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Billing => "billing",
            Self::Superadmin => "superadmin",
        }
    }
}

impl FromStr for PlatformRole {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "support" => Ok(Self::Support),
            "billing" => Ok(Self::Billing),
            "superadmin" => Ok(Self::Superadmin),
            other => Err(UnknownRole(other.to_owned())),
        }
    }
}

impl std::fmt::Display for PlatformRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Somebody on platform staff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffMember {
    pub identity: IdentityId,
    /// The login handle, when the identity has a password.
    pub handle: Option<String>,
    pub role: PlatformRole,
    pub since: Timestamp,
    /// Whether the identity itself is suspended, which overrides the role.
    pub suspended: bool,
    /// Whether they have a second factor. Every platform door refuses them
    /// without one, so `false` is somebody who cannot do their job yet.
    pub second_factor: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StaffError {
    /// No password login uses that handle. Staff are made from accounts that
    /// already exist; there is no password here to hand anybody.
    #[error("no account signs in as {0}")]
    NoSuchAccount(String),
    #[error("{0} is already platform staff")]
    AlreadyStaff(String),
    /// **Refused at the grant, not only at the door.** An account with a
    /// password and no factor is one somebody holding just the password could
    /// enrol their own factor on. Granted only once a factor exists, and with
    /// `disable_second_factor` refusing staff
    /// ([`AuthError::SecondFactorKept`](crate::AuthError::SecondFactorKept)),
    /// a staff account is never in that state. Whose phone the factor is on
    /// is the granting superadmin's to know; this cannot.
    #[error("{0} has no second factor")]
    NoSecondFactor(String),
    #[error("that identity is not platform staff")]
    NotStaff,
    /// The last live superadmin cannot be removed or demoted over HTTP: with
    /// none left, nobody can grant another. `operator revoke-staff` is the
    /// break-glass path that can.
    #[error("the platform must keep at least one live superadmin")]
    LastSuperadmin,
    #[error(transparent)]
    Access(#[from] AccessError),
}

impl erp_i18n::Localize for StaffError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchAccount(handle) => Message::new(messages::STAFF_NO_SUCH_ACCOUNT)
                .with("handle", MessageArg::text(handle.clone())),
            Self::AlreadyStaff(handle) => Message::new(messages::ALREADY_STAFF)
                .with("handle", MessageArg::text(handle.clone())),
            Self::NoSecondFactor(handle) => Message::new(messages::STAFF_NO_SECOND_FACTOR)
                .with("handle", MessageArg::text(handle.clone())),
            Self::NotStaff => Message::new(messages::NOT_STAFF),
            Self::LastSuperadmin => Message::new(messages::LAST_SUPERADMIN),
            Self::Access(e) => e.message(),
        }
    }
}

impl ControlPlane {
    /// **The door every platform power is asked at.**
    ///
    /// Refuses unless the identity is active, holds a platform role that
    /// [`PlatformRole::may`] the power, and has a second factor enrolled.
    ///
    /// **"Enrolled" stands in for "this session went through it"**, and two
    /// things keep it honest: `confirm_second_factor` ends every other session
    /// of the identity, so none from before the factor survives it, and
    /// `start_session` refuses a password-only one after. The per-request check
    /// is for staff made by `grant_membership` directly, which does not ask.
    ///
    /// # Errors
    /// [`AccessError::StaffOnly`] naming the power when there is no role or it
    /// may not; [`AccessError::StaffSecondFactorRequired`] when the role may and
    /// the factor is missing.
    pub async fn staff_may(
        &self,
        identity: IdentityId,
        power: PlatformPower,
    ) -> Result<PlatformRole, AccessError> {
        let who = self
            .cached_identity(identity)
            .await?
            .ok_or(AccessError::NoSuchIdentity)?;
        if !who.is_active() {
            return Err(AccessError::IdentitySuspended);
        }
        let role = self
            .cached_platform_role(identity)
            .await?
            .filter(|role| role.may(power))
            .ok_or(AccessError::StaffOnly(power))?;
        if !self.has_second_factor(identity).await? {
            return Err(AccessError::StaffSecondFactorRequired);
        }
        Ok(role)
    }

    /// Everybody on platform staff, longest-serving first.
    pub async fn staff(&self) -> Result<Vec<StaffMember>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT m.identity_id as "identity: IdentityId",
                      m.role,
                      m.created_at,
                      i.status,
                      (SELECT a.handle FROM authenticator a
                        WHERE a.identity_id = m.identity_id AND a.kind = 'password'
                        LIMIT 1) as handle,
                      EXISTS (SELECT 1 FROM authenticator a
                               WHERE a.identity_id = m.identity_id AND a.kind = 'totp')
                          as "second_factor!"
                 FROM membership m
                 JOIN identity i ON i.id = m.identity_id
                WHERE m.scope_kind = 'platform' AND m.revoked_at IS NULL
                ORDER BY m.created_at"#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(StaffMember {
                    identity: row.identity,
                    handle: row.handle,
                    role: parse_platform_role(&row.role)?,
                    since: row.created_at,
                    suspended: row.status != "active",
                    second_factor: row.second_factor,
                })
            })
            .collect()
    }

    /// The identity a password login belongs to, by its handle.
    pub async fn identity_by_login(&self, handle: &str) -> Result<Option<IdentityId>, AccessError> {
        Ok(sqlx::query_scalar!(
            r#"SELECT identity_id as "identity: IdentityId" FROM authenticator
                WHERE kind = 'password' AND handle = $1"#,
            handle.trim().to_lowercase(),
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Makes an existing account platform staff.
    ///
    /// Refuses somebody already on staff — changing a role is
    /// [`Self::change_staff_role`], which knows about the last superadmin — and
    /// an account with no second factor (see [`StaffError::NoSecondFactor`]).
    pub async fn grant_staff(
        &self,
        handle: &str,
        role: PlatformRole,
        actor: Actor,
    ) -> Result<IdentityId, StaffError> {
        let handle = handle.trim().to_lowercase();
        let identity = self
            .identity_by_login(&handle)
            .await?
            .ok_or_else(|| StaffError::NoSuchAccount(handle.clone()))?;
        if self.platform_role(identity).await?.is_some() {
            return Err(StaffError::AlreadyStaff(handle));
        }
        if !self
            .has_second_factor(identity)
            .await
            .map_err(AccessError::from)?
        {
            return Err(StaffError::NoSecondFactor(handle));
        }
        // Recorded, and the platform cache forgotten, by `grant_membership`.
        self.grant_membership(identity, Scope::Platform, role.as_str(), actor)
            .await?;
        // **Read back, because a live row wins silently.** Two grants of one
        // person racing past the check above both "succeed" in
        // `grant_membership`, and the second has changed nothing — so whoever
        // did not get the role they asked for is told they are too late.
        if self.platform_role(identity).await? != Some(role) {
            return Err(StaffError::AlreadyStaff(handle));
        }
        Ok(identity)
    }

    /// Changes what a staff member may do. Refuses to demote the last live
    /// superadmin.
    pub async fn change_staff_role(
        &self,
        identity: IdentityId,
        role: PlatformRole,
        actor: Actor,
    ) -> Result<(), StaffError> {
        self.move_staff(identity, Some(role), actor).await
    }

    /// Takes somebody off platform staff. Refuses the last live superadmin;
    /// `revoke_membership` with [`Scope::Platform`] is the unguarded form the
    /// operator CLI uses.
    pub async fn revoke_staff(&self, identity: IdentityId, actor: Actor) -> Result<(), StaffError> {
        self.move_staff(identity, None, actor).await
    }

    /// A role change (`Some`) or a revocation (`None`), behind the
    /// last-superadmin guard.
    ///
    /// **One transaction, with every live superadmin row locked first.** Two
    /// superadmins demoting each other at the same moment would otherwise each
    /// see the other still standing, and both succeed. Locked in a fixed order,
    /// so two of these cannot deadlock on each other.
    async fn move_staff(
        &self,
        identity: IdentityId,
        to: Option<PlatformRole>,
        actor: Actor,
    ) -> Result<(), StaffError> {
        let mut tx = self.pool.begin().await.map_err(AccessError::from)?;

        let superadmins = sqlx::query_scalar!(
            r#"SELECT m.identity_id as "identity: IdentityId"
                 FROM membership m
                 JOIN identity i ON i.id = m.identity_id
                WHERE m.scope_kind = 'platform' AND m.role = 'superadmin'
                  AND m.revoked_at IS NULL AND i.status = 'active'
                ORDER BY m.identity_id
                  FOR UPDATE OF m"#,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(AccessError::from)?;

        let current = sqlx::query_scalar!(
            "SELECT role FROM membership
              WHERE identity_id = $1 AND scope_kind = 'platform' AND revoked_at IS NULL
                FOR UPDATE",
            identity.as_uuid(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(AccessError::from)?
        .ok_or(StaffError::NotStaff)?;

        let leaves_superadmin = parse_platform_role(&current)? == PlatformRole::Superadmin
            && to != Some(PlatformRole::Superadmin);
        if leaves_superadmin && superadmins.iter().all(|other| *other == identity) {
            return Err(StaffError::LastSuperadmin);
        }

        match to {
            Some(role) => sqlx::query!(
                "UPDATE membership SET role = $2
                  WHERE identity_id = $1 AND scope_kind = 'platform' AND revoked_at IS NULL",
                identity.as_uuid(),
                role.as_str(),
            )
            .execute(&mut *tx)
            .await
            .map_err(AccessError::from)?,
            None => sqlx::query!(
                "UPDATE membership SET revoked_at = now()
                  WHERE identity_id = $1 AND scope_kind = 'platform' AND revoked_at IS NULL",
                identity.as_uuid(),
            )
            .execute(&mut *tx)
            .await
            .map_err(AccessError::from)?,
        };

        let (action, detail) = match to {
            Some(role) => (
                "membership.role_changed",
                serde_json::json!({ "scope": "platform", "role": role.as_str() }),
            ),
            // The shape `revoke_membership` records, so one action has one.
            None => (
                "membership.revoked",
                serde_json::json!({ "scope": "platform", "tenant": null }),
            ),
        };
        self.record(
            &mut tx,
            actor,
            None,
            action,
            "identity",
            &identity.to_string(),
            detail,
        )
        .await?;
        tx.commit().await.map_err(AccessError::from)?;

        // Now, not after the TTL: a revoked superadmin keeping the keys for
        // five seconds is five seconds of exactly what was just taken away.
        self.forget(crate::shared::Invalidate::Platform(identity))
            .await;
        Ok(())
    }
}

/// A stored platform role this build must recognise. Refused rather than
/// defaulted, for the reason [`UnknownRole`] gives.
pub(crate) fn parse_platform_role(raw: &str) -> Result<PlatformRole, AccessError> {
    raw.parse::<PlatformRole>()
        .map_err(|e| AccessError::Corrupt(format!("platform {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_through_their_stored_form() {
        for role in PlatformRole::ALL {
            assert_eq!(role.as_str().parse::<PlatformRole>(), Ok(role));
        }
    }

    #[test]
    fn an_unknown_stored_role_is_corrupt_data_not_a_guess() {
        // A tenant role is not a platform role, whatever it grants in a tenant.
        for stored in ["owner", "admin", "Superadmin", ""] {
            assert!(
                matches!(parse_platform_role(stored), Err(AccessError::Corrupt(_))),
                "{stored:?} was read as a platform role"
            );
        }
    }

    /// **The platform matrix, as a test** — decision 2 of 2026-09-11, written
    /// out rather than derived, so widening a role has to be typed here too.
    #[test]
    fn every_role_may_exactly_what_it_should() {
        use PlatformPower::{
            EnterForSupport, HandleDeadLetters, ManageStaff, ReadAuditTrail, ResetSecondFactors,
            SuspendTenants,
        };

        let expected = [
            (
                PlatformRole::Superadmin,
                vec![
                    SuspendTenants,
                    HandleDeadLetters,
                    ReadAuditTrail,
                    EnterForSupport,
                    ManageStaff,
                    ResetSecondFactors,
                ],
            ),
            (PlatformRole::Billing, vec![SuspendTenants]),
            (
                PlatformRole::Support,
                vec![
                    ReadAuditTrail,
                    HandleDeadLetters,
                    EnterForSupport,
                    ResetSecondFactors,
                ],
            ),
        ];

        for (role, allowed) in expected {
            for power in PlatformPower::ALL {
                assert_eq!(
                    role.may(power),
                    allowed.contains(&power),
                    "{role} / {}",
                    power.as_str()
                );
            }
        }
    }
}
