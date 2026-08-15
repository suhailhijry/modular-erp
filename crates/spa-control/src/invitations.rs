//! Giving someone access without knowing their password.
//!
//! # What this replaces
//!
//! `add_member` takes a password the owner chooses and hands over — a password
//! the owner then knows forever, and which the colleague probably never changes.
//! An invitation inverts it: the recipient sets their own, and nobody else ever
//! sees it.
//!
//! # Why there is no email here
//!
//! The invitation link is returned to whoever created it, once, and they pass it
//! on however they already talk to that person. That is not a placeholder for
//! sending mail — for a small business in this market it is frequently *better*
//! than mail, and it is the difference between this working today and waiting on
//! a decision about a mail provider.
//!
//! Sending it by email is an outbox effect (D9) and belongs with the first real
//! [`EffectHandler`](spa_eventlog::EffectHandler). The control plane has no
//! outbox table yet; adding one to carry a handler nobody has written would be
//! building the mechanism before the need.
//!
//! # What the link is worth
//!
//! Everything the invited address is worth. Anyone holding it can accept, which
//! is what an invitation link is — so it is treated as a credential: 256 bits of
//! entropy, only the SHA-256 stored, one live link per address per tenant, and
//! an expiry.
//!
//! What it cannot do is become *somebody else's* account. Acceptance always
//! binds to the invited handle: an existing account for that address must prove
//! itself with its password, and a new one is created under that address and no
//! other.

use spa_types::{IdentityId, TenantId, Timestamp};
use uuid::Uuid;

use crate::auth::InvitationToken;
use crate::members::MemberError;
use crate::model::{Actor, Scope};
use crate::{AccessError, AuthError, ControlPlane, Role, Session, SessionToken};

/// How long an invitation stays open.
///
/// Long enough to survive a holiday, short enough that a link found in an old
/// message is not a way in. ponytail: a constant until somebody wants to choose
/// it; the column already stores an instant, so making it per-invitation is a
/// parameter rather than a migration.
pub const INVITATION_LIFETIME: std::time::Duration = std::time::Duration::from_hours(24 * 14);

/// An invitation as its tenant sees it. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub id: Uuid,
    pub handle: String,
    pub role: Role,
    pub invited_by: Option<IdentityId>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

/// What the holder of a link is shown before accepting.
///
/// Deliberately includes the tenant's name and the role: accepting is granting
/// somebody access to *something*, and a link that does not say what is a link
/// people click without reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvitation {
    pub tenant: TenantId,
    pub company: String,
    pub slug: String,
    pub handle: String,
    pub role: Role,
    pub expires_at: Timestamp,
    /// Whether that address already has an account here, so the form can ask
    /// for "your password" rather than "choose a password".
    pub has_account: bool,
}

/// What accepting produced.
#[derive(Debug)]
pub struct Accepted {
    pub identity: IdentityId,
    pub tenant: TenantId,
    pub token: SessionToken,
    pub session: Session,
}

#[derive(Debug, thiserror::Error)]
pub enum InvitationError {
    /// No live invitation for that token. Covers wrong, expired, revoked and
    /// already-accepted, all as one answer — telling a link-holder *which* is
    /// telling them something they have not proved they should know.
    #[error("that invitation is not valid")]
    NotValid,
    /// The address already has an account and the password did not match it.
    #[error("that password does not match the existing account")]
    WrongPassword,
    #[error(transparent)]
    Member(#[from] MemberError),
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl spa_i18n::Localize for InvitationError {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::Message;
        match self {
            Self::NotValid => Message::new(messages::INVITATION_NOT_VALID),
            // Deliberately the ordinary bad-credentials message: this is a login
            // wearing a different hat, and it should not say anything a login
            // would not.
            Self::WrongPassword => Message::new(messages::INVALID_CREDENTIALS),
            Self::Member(e) => e.message(),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}

/// A stored role, validated on the way out.
///
/// Data written by an older version of this system is exactly where "it was
/// valid when we wrote it" stops being a guarantee.
fn parse_role(raw: &str) -> Result<Role, AccessError> {
    raw.parse()
        .map_err(|_| AccessError::Corrupt(format!("invitation.role: {raw}")))
}

impl ControlPlane {
    /// Creates an invitation and returns the link's token **once**.
    ///
    /// Re-inviting an address revokes the previous invitation rather than
    /// leaving two live links — otherwise revoking one would revoke one of
    /// several ways in, which is not revoking.
    pub async fn invite(
        &self,
        tenant_id: TenantId,
        handle: String,
        role: Role,
        invited_by: IdentityId,
    ) -> Result<(Invitation, InvitationToken), InvitationError> {
        let handle = handle.trim().to_lowercase();

        // Someone who is already in does not need an invitation, and sending
        // one would imply they are not.
        if let Some(identity) = self.identity_for_handle(&handle).await?
            && self.cached_membership(identity, tenant_id).await?.is_some()
        {
            return Err(MemberError::AlreadyAMember(handle).into());
        }

        let (token, digest) = InvitationToken::mint()?;
        let seconds = i64::try_from(INVITATION_LIFETIME.as_secs()).unwrap_or(i64::MAX);
        let id = Uuid::now_v7();

        let mut tx = self.pool.begin().await.map_err(AccessError::Database)?;

        // Revoke first, in the same transaction as the insert: the partial
        // unique index enforces one outstanding invitation per address, so
        // doing this in two commits would leave a window where the second
        // insert fails and the first invitation is already gone.
        sqlx::query!(
            "UPDATE invitation SET revoked_at = now()
              WHERE tenant_id = $1 AND handle = $2
                AND accepted_at IS NULL AND revoked_at IS NULL",
            tenant_id.as_uuid(),
            handle,
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        let row = sqlx::query!(
            "INSERT INTO invitation
                 (id, tenant_id, token_hash, handle, role, invited_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6,
                     now() + ($7::BIGINT * INTERVAL '1 second'))
             RETURNING expires_at, created_at",
            id,
            tenant_id.as_uuid(),
            digest,
            handle,
            role.as_str(),
            invited_by.as_uuid(),
            seconds,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        tx.commit().await.map_err(AccessError::Database)?;

        self.record(
            Actor::identity(invited_by),
            "invitation.created",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "handle": handle, "role": role.as_str() }),
        )
        .await?;

        Ok((
            Invitation {
                id,
                handle,
                role,
                invited_by: Some(invited_by),
                expires_at: row.expires_at,
                created_at: row.created_at,
            },
            token,
        ))
    }

    /// Outstanding invitations for a tenant. Never includes tokens.
    pub async fn invitations(&self, tenant_id: TenantId) -> Result<Vec<Invitation>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT id, handle, role,
                      invited_by as "invited_by: IdentityId",
                      expires_at, created_at
                 FROM invitation
                WHERE tenant_id = $1
                  AND accepted_at IS NULL AND revoked_at IS NULL
                  AND expires_at > now()
                ORDER BY created_at DESC"#,
            tenant_id.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(Invitation {
                    id: row.id,
                    handle: row.handle,
                    role: parse_role(&row.role)?,
                    invited_by: row.invited_by,
                    expires_at: row.expires_at,
                    created_at: row.created_at,
                })
            })
            .collect()
    }

    /// Withdraws an invitation. Idempotent; a already-dead one is a no-op.
    pub async fn revoke_invitation(
        &self,
        tenant_id: TenantId,
        invitation: Uuid,
        actor: Actor,
    ) -> Result<(), AccessError> {
        // Scoped by tenant as well as id, so an id from one tenant cannot
        // revoke another's invitation.
        sqlx::query!(
            "UPDATE invitation SET revoked_at = now()
              WHERE id = $1 AND tenant_id = $2 AND accepted_at IS NULL AND revoked_at IS NULL",
            invitation,
            tenant_id.as_uuid(),
        )
        .execute(&self.pool)
        .await?;

        self.record(
            actor,
            "invitation.revoked",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "invitation": invitation.to_string() }),
        )
        .await
    }

    /// What a link says about itself, before anyone accepts it.
    pub async fn pending_invitation(
        &self,
        token: &str,
    ) -> Result<PendingInvitation, InvitationError> {
        let row = sqlx::query!(
            r#"SELECT i.tenant_id, i.handle, i.role, i.expires_at,
                      t.display_name, t.slug
                 FROM invitation i
                 JOIN tenant t ON t.id = i.tenant_id
                WHERE i.token_hash = $1
                  AND i.accepted_at IS NULL AND i.revoked_at IS NULL
                  AND i.expires_at > now()"#,
            InvitationToken::digest_of(token),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .ok_or(InvitationError::NotValid)?;

        let handle = row.handle;
        Ok(PendingInvitation {
            tenant: TenantId::from_uuid(row.tenant_id),
            company: row.display_name,
            slug: row.slug,
            has_account: self.identity_for_handle(&handle).await?.is_some(),
            handle,
            role: parse_role(&row.role)?,
            expires_at: row.expires_at,
        })
    }

    /// Takes up an invitation, and signs the accepter in.
    ///
    /// # The password means two things
    ///
    /// If the invited address already has an account, it must match — this is a
    /// login, and a link-holder who is not that person gets no further. If it
    /// does not, the password *becomes* the account's, created under the invited
    /// address and no other.
    ///
    /// That does let a link-holder learn whether the address has an account
    /// here. Accepted: they were already shown the address by
    /// [`Self::pending_invitation`], so the link tells them more than the
    /// difference between these two answers does.
    pub async fn accept_invitation(
        &self,
        token: &str,
        password: String,
    ) -> Result<Accepted, InvitationError> {
        let digest = InvitationToken::digest_of(token);

        // Claimed first, in one statement: two people opening the same link at
        // once must not both get a membership, and `WHERE accepted_at IS NULL`
        // on an `UPDATE` is the whole of the lock.
        //
        // `accepted_by` is filled in below — the row is claimed before the
        // identity exists, which is why the check constraint allows this state
        // for exactly as long as this function takes.
        let claimed = sqlx::query!(
            r#"UPDATE invitation SET accepted_at = now()
                WHERE token_hash = $1
                  AND accepted_at IS NULL AND revoked_at IS NULL
                  AND expires_at > now()
            RETURNING id, tenant_id, handle, role"#,
            digest,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        .ok_or(InvitationError::NotValid)?;

        let role = parse_role(&claimed.role)?;
        let tenant = TenantId::from_uuid(claimed.tenant_id);

        let outcome = self
            .establish(&claimed.handle, password, tenant, role)
            .await;

        match outcome {
            Ok((identity, token, session)) => {
                sqlx::query!(
                    "UPDATE invitation SET accepted_by = $2 WHERE id = $1",
                    claimed.id,
                    identity.as_uuid(),
                )
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?;

                self.record(
                    Actor::identity(identity),
                    "invitation.accepted",
                    "tenant",
                    &tenant.to_string(),
                    serde_json::json!({ "handle": claimed.handle, "role": role.as_str() }),
                )
                .await?;

                Ok(Accepted {
                    identity,
                    tenant,
                    token,
                    session,
                })
            }
            Err(e) => {
                // Unclaim it. A wrong password must not burn the invitation —
                // that would turn a typo into a support ticket.
                sqlx::query!(
                    "UPDATE invitation SET accepted_at = NULL WHERE id = $1 AND accepted_by IS NULL",
                    claimed.id,
                )
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?;
                Err(e)
            }
        }
    }

    /// Signs the accepter in — logging in if the address is known, creating the
    /// account if it is not — and gives them their membership.
    async fn establish(
        &self,
        handle: &str,
        password: String,
        tenant: TenantId,
        role: Role,
    ) -> Result<(IdentityId, SessionToken, Session), InvitationError> {
        let (identity, token, session) =
            if let Some(identity) = self.identity_for_handle(handle).await? {
                // A login, in every respect including its timing. The invitation
                // proves which tenant; the password proves who.
                let (token, session) = self
                    .log_in(handle, &password)
                    .await
                    .map_err(|_| InvitationError::WrongPassword)?;
                (identity, token, session)
            } else {
                let created = self.create_identity(Actor::system()).await?;
                self.register_login(created.id, handle.to_owned(), password)
                    .await?;
                let (token, session) = self.start_session(created.id).await?;
                (created.id, token, session)
            };

        // Already a member is fine here rather than an error: the invitation was
        // valid, and refusing at the last step would leave it burnt.
        if self.cached_membership(identity, tenant).await?.is_none() {
            self.grant_membership(
                identity,
                Scope::Tenant(tenant),
                role.as_str(),
                Actor::identity(identity),
            )
            .await?;
        }

        Ok((identity, token, session))
    }

    /// The identity behind a login handle, if there is one.
    pub(crate) async fn identity_for_handle(
        &self,
        handle: &str,
    ) -> Result<Option<IdentityId>, AccessError> {
        sqlx::query_scalar!(
            r#"SELECT identity_id as "identity: IdentityId" FROM authenticator
                WHERE kind = 'password' AND handle = $1"#,
            handle.trim().to_lowercase(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)
    }
}
