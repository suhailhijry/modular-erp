//! **Getting back in, and changing your mind.**
//!
//! Two ways to write a new password, and neither of them existed. `log_in` was
//! the only thing that ever read `authenticator.secret` and `register_login`
//! the only thing that wrote one — from signup and from accepting an
//! invitation. So a tenant owner who forgot their password was locked out
//! permanently, and nobody is above a tenant owner.
//!
//! # A reset replaces the password. It does not replace the second factor
//!
//! Redeeming a link demands the enrolled factor before it writes anything, and
//! **issues no session**. Both halves are load-bearing and each closes a
//! different door:
//!
//! - **No session**, because `disable_second_factor` takes nothing but a live
//!   one. A reset that handed back a session would be a two-call factor
//!   removal: open the link, get a session, delete the enrolment. So a reset
//!   ends with "changed" and the person logs in, through the same gate as
//!   everybody else.
//! - **The factor**, because the mailbox is exactly what a second factor is
//!   there to survive. Somebody who has your password, or your laptop with a
//!   mail client signed in, or your old employer's forwarding, has the reset
//!   link. If that were enough, enrolling would protect the login form and
//!   nothing else.
//!
//! **Requiring it strands nobody who was not already stranded.** Anybody who
//! cannot present the factor here cannot log in with it either, so this closes
//! no door that was open — which is what makes it consistent with *switching a
//! control on must not be the act that strands you*. The recovery codes are the
//! path, and they exist for exactly this person.
//!
//! # An address with no account is answered the same and mailed nothing
//!
//! The response is `202` either way. What it is *not* is a message: mailing
//! every address anybody types would make this an unauthenticated mail cannon
//! aimed at strangers, and the cost is not their inbox — it is the complaint
//! rate on the fleet's sending domain, which is what carries every tenant's
//! signup and invitation mail. That is the vector `0010_signups.sql` closed,
//! and it is not worth reopening to hide one `INSERT` of timing.

use erp_types::{IdentityId, Timestamp};

use crate::auth::{AuthError, hash_password};
use crate::{AccessError, ControlPlane};

/// How long a reset link works for.
///
/// One hour, against a signup's day and an invitation's fortnight. Those two
/// wait on somebody deciding something; this one waits on a person standing at
/// the login screen now. Every extra minute is a working credential sitting in
/// a mailbox that may itself be why they are locked out.
pub const RESET_LIFETIME_SECONDS: i64 = 60 * 60;

/// The least time between two reset emails to one address.
///
/// A minute. It is the only thing bounding how much mail one stranger can aim
/// at an address that does have an account.
pub const RESET_INTERVAL_SECONDS: i64 = 60;

/// How many wrong second-factor codes one link survives.
///
/// Five, matching `otp::MAX_ATTEMPTS`. The token is 256 bits and unguessable;
/// the six digits it gates are twenty, and twenty bits is where guessing
/// actually goes.
pub const MAX_ATTEMPTS: i32 = 5;

/// Why a password could not be written.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    /// **One answer for every way a link fails.** Wrong, expired, spent, and
    /// too many wrong codes are one message, for the reason
    /// [`SignupError::NotValid`](crate::SignupError::NotValid) gives: telling a
    /// link-holder which is telling them something they have not proved they
    /// should know.
    #[error("that reset link is not valid")]
    NotValid,
    /// A link went to this address less than [`RESET_INTERVAL_SECONDS`] ago.
    ///
    /// **Never reaches a caller who did not supply a known address**, because
    /// it would answer the question the `202` exists to refuse.
    #[error("a link was sent to this address {sent} seconds ago; retry in {retry_in}")]
    TooSoon { sent: i64, retry_in: i64 },
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl ControlPlane {
    /// **Starts a reset, and says nothing about whether the address is known.**
    ///
    /// # Errors
    /// [`PasswordError::TooSoon`] only for an address that has an account —
    /// see the variant. Otherwise the database.
    pub async fn request_password_reset(
        &self,
        handle: &str,
        locale: erp_i18n::Locale,
        link_base: &str,
    ) -> Result<Option<crate::auth::ResetToken>, PasswordError> {
        let handle = handle.trim().to_lowercase();

        // No account: nothing written, nothing sent, and the caller is told
        // exactly what a known address is told.
        let Some(identity) = self.identity_for_handle(&handle).await? else {
            return Ok(None);
        };

        if let Some(sent) = self.last_reset_at(&handle).await? {
            let elapsed = (Timestamp::from(chrono::Utc::now()) - sent).num_seconds();
            if elapsed < RESET_INTERVAL_SECONDS {
                return Err(PasswordError::TooSoon {
                    sent: elapsed,
                    retry_in: RESET_INTERVAL_SECONDS - elapsed,
                });
            }
        }

        let (token, digest) = crate::auth::ResetToken::mint()?;
        let id = uuid::Uuid::now_v7();

        let mut tx = self.pool.begin().await.map_err(AccessError::Database)?;
        sqlx::query!(
            "INSERT INTO password_reset (id, token_hash, handle, identity_id, expires_at)
             VALUES ($1, $2, $3, $4, now() + ($5::BIGINT * INTERVAL '1 second'))",
            id,
            digest,
            handle,
            identity.as_uuid(),
            RESET_LIFETIME_SECONDS,
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        // The mail in the same transaction as the row it is about (D9), for the
        // reason signup gives: sending inline would either mail somebody about
        // a link that rolled back, or lose the send to a crash with nothing
        // recording it was owed.
        let (subject, body) =
            crate::mail::reset_messages(&format!("{link_base}{}", token.expose()));
        let email = crate::mail::Email::rendered(&crate::CATALOG, locale, handle, &subject, &body);
        erp_eventlog::enqueue(&mut tx, None, &[email.promised(format!("reset:{id}"))])
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;
        tx.commit().await.map_err(AccessError::Database)?;

        Ok(Some(token))
    }

    /// **Redeems a link and writes the password.** Issues nothing.
    ///
    /// # Errors
    /// [`PasswordError::NotValid`] for any unusable link, and
    /// [`AuthError::SecondFactorRequired`] when the account has one and no code
    /// came with the request — the link is left unspent so a client can ask for
    /// six digits and come back.
    pub async fn reset_password(
        &self,
        token: &str,
        new_password: &str,
        code: Option<&str>,
        now: Timestamp,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<(), PasswordError> {
        let secret = hash_password(new_password)?;

        // Claimed in the statement that finds it, so two people opening one
        // link resolve to one — the shape `otp::verify_code` uses.
        let Some(row) = sqlx::query!(
            r#"UPDATE password_reset SET used_at = now()
                WHERE token_hash = $1
                  AND used_at IS NULL
                  AND expires_at > now()
                  AND attempts < $2
              RETURNING id, identity_id as "identity: IdentityId""#,
            crate::auth::ResetToken::digest_of(token),
            MAX_ATTEMPTS,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?
        else {
            return Err(PasswordError::NotValid);
        };

        if self.has_second_factor(row.identity).await? {
            let Some(code) = code else {
                // Unspent: the account has a factor and the client did not know
                // it. Asking for six digits is the next screen, not a failure.
                self.unclaim_reset(row.id).await?;
                return Err(AuthError::SecondFactorRequired.into());
            };
            if let Err(e) = self
                .verify_second_factor(row.identity, code, now, sealing)
                .await
            {
                // A wrong code costs an attempt and puts the link back, so five
                // guesses kill it rather than one.
                self.spend_reset_attempt(row.id).await?;
                return Err(e.into());
            }
        }

        self.set_password(row.identity, &secret).await?;
        // **Every session ends.** A reset means the old password is not trusted,
        // and a session minted under it is the old password still working.
        self.log_out_everywhere(row.identity).await?;
        Ok(())
    }

    /// **Changes a password for somebody who is holding one.**
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] if the current password is wrong.
    pub async fn change_password(
        &self,
        identity: IdentityId,
        current: &str,
        new_password: &str,
    ) -> Result<(), PasswordError> {
        let handle = self.password_handle(identity).await?;
        // Through `authenticate`, so a wrong current password costs what a
        // wrong login costs and takes the same time.
        self.authenticate(&handle, current).await?;

        let secret = hash_password(new_password)?;
        self.set_password(identity, &secret).await?;
        // Including this one. A password changed because it may be known is a
        // password whose sessions may be somebody else's.
        self.log_out_everywhere(identity).await?;
        Ok(())
    }

    /// The one write. **`UPDATE`, never `INSERT`** — see the module doc and
    /// `migrations/control/0010_signups.sql`: inserting here would claim a
    /// handle, and neither of these flows is allowed to create a login.
    async fn set_password(&self, identity: IdentityId, secret: &str) -> Result<(), AuthError> {
        let changed = sqlx::query!(
            "UPDATE authenticator SET secret = $2
              WHERE identity_id = $1 AND kind = 'password'",
            identity.as_uuid(),
            secret,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if changed == 0 {
            // An identity that signs in by phone alone has no password row to
            // rewrite, and this must not quietly create one.
            return Err(AuthError::InvalidCredentials);
        }
        Ok(())
    }

    async fn password_handle(&self, identity: IdentityId) -> Result<String, AuthError> {
        sqlx::query_scalar!(
            "SELECT handle FROM authenticator WHERE identity_id = $1 AND kind = 'password'",
            identity.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::InvalidCredentials)
    }

    async fn last_reset_at(&self, handle: &str) -> Result<Option<Timestamp>, AccessError> {
        sqlx::query_scalar!(
            "SELECT max(created_at) FROM password_reset WHERE handle = $1",
            handle,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(AccessError::Database)
    }

    async fn unclaim_reset(&self, id: uuid::Uuid) -> Result<(), AccessError> {
        sqlx::query!("UPDATE password_reset SET used_at = NULL WHERE id = $1", id)
            .execute(&self.pool)
            .await
            .map_err(AccessError::Database)?;
        Ok(())
    }

    async fn spend_reset_attempt(&self, id: uuid::Uuid) -> Result<(), AccessError> {
        sqlx::query!(
            "UPDATE password_reset SET used_at = NULL, attempts = attempts + 1 WHERE id = $1",
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(AccessError::Database)?;
        Ok(())
    }

    /// Collects spent and expired links. Registered beside the other sweeps.
    ///
    /// # Errors
    /// If the database does.
    pub async fn sweep_password_resets(&self) -> Result<u64, AccessError> {
        Ok(
            sqlx::query!("DELETE FROM password_reset WHERE expires_at < now()")
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?
                .rows_affected(),
        )
    }
}

impl erp_i18n::Localize for PasswordError {
    fn message(&self) -> erp_i18n::Message {
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NotValid => Message::new(crate::messages::RESET_NOT_VALID),
            Self::TooSoon { sent, retry_in } => Message::new(crate::messages::RESET_TOO_SOON)
                .with("sent", MessageArg::Int(*sent))
                .with("retry_in", MessageArg::Int(*retry_in)),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}
