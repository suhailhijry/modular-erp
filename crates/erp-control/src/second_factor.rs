//! Enrolling a second factor, and getting back in without it.
//!
//! [`crate::totp`] is the arithmetic and knows nothing about a database. This
//! is the part that stores, and the decisions here are storage decisions.
//!
//! # Why a login takes both factors in one call
//!
//! The usual shape is a challenge: check the password, hand back a short-lived
//! token, exchange that plus a code for a session. It needs a table of
//! half-authenticated states, and every query that reads a session has to
//! remember to exclude them. One that forgets is a login that never needed the
//! second factor.
//!
//! So there is no half-authenticated state at all: [`crate::ControlPlane::log_in`]
//! takes an optional code, refuses with [`AuthError::SecondFactorRequired`]
//! when one is enrolled and none was given, and **creates nothing until both
//! factors pass**. The cost is that a client holds the password until the
//! person has typed the code. The gain is that a session which skipped a factor
//! is not a bug that can be written.
//!
//! # Why a used code cannot be used again
//!
//! A code is good for thirty seconds and TOTP has no memory, so the same six
//! digits work for every login inside that window — including for somebody
//! reading them over a shoulder. The last accepted code is recorded against the
//! enrolment and refused a second time, which closes the replay without needing
//! a second table.

use erp_types::{IdentityId, TenantId, Timestamp};
use sha2::Digest as _;

use crate::model::Actor;
use crate::{AccessError, ControlPlane, auth::AuthError};

/// How many recovery codes an enrolment produces.
///
/// Ten is enough that losing a phone is survivable and few enough that a person
/// will actually keep the list.
pub const RECOVERY_CODES: usize = 10;

/// What enrolling starts.
#[derive(Debug, Clone)]
pub struct Enrolment {
    /// The `otpauth://` URI a QR encodes.
    pub uri: String,
    /// The same secret in base32, for somebody whose camera will not read it.
    pub secret: String,
}

/// **Who requires an identity to keep its second factor**, and so why
/// [`ControlPlane::disable_second_factor`] refuses — see
/// [`ControlPlane::second_factor_required_by`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactorRequiredBy {
    /// A live platform membership, whatever its role.
    Staff,
    /// A live membership of a tenant that requires one.
    Tenant,
}

/// How long an enrolment link works for.
///
/// One hour, the same as a password reset's and for the same reason: it waits
/// on somebody standing at a screen now, not on somebody deciding something.
/// Running out costs them nothing permanent — the account stays link-only, and
/// whoever reset it can send another.
pub const ENROLMENT_LIFETIME_SECONDS: i64 = 60 * 60;

/// Why somebody else's second factor was not reset.
///
/// Every arm is a different sentence to the person reading it, which is why
/// there is no single "refused": the owner is told to ask support, a member of
/// two companies is told to ask support for a different reason, and somebody
/// trying to reset their own is told to replace it instead.
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    /// **Your own factor, at either route.** Removing your own is
    /// `disable_second_factor`, which costs a code and is refused outright
    /// where a factor is required; this must not become the way round it.
    #[error("you cannot reset your own second factor")]
    Yourself,
    /// A member resetting the owner's. Nobody in a tenant is above its owner.
    #[error("a member cannot reset the owner's second factor")]
    TheOwner,
    /// Not a live member of this tenant. Renders as `members.not_a_member` and
    /// answers the 404 every other route about a member answers, so this one is
    /// no oracle for identities the caller cannot already list.
    #[error("that person is not a live member of this tenant")]
    NotAMember,
    /// A member resetting platform staff's. The platform's own route can, and
    /// only with [`PlatformPower::ManageStaff`](crate::PlatformPower::ManageStaff).
    #[error("that account is platform staff")]
    PlatformStaff,
    /// **The cross-tenant rule.** A factor is one account's, not one company's,
    /// so a company that is not the only one somebody works for may not weaken
    /// their sign-in. Platform support resets those.
    #[error("that person has a live membership of another tenant")]
    AnotherCompany,
    /// Nothing to mail a link to. Refused rather than resetting into a
    /// permanent lockout (L6).
    #[error("that account has no password login, so no address to mail a link to")]
    NoLogin,
    /// The platform route's reason: absent, blank, or over 500 characters.
    #[error("a reset needs a reason of 1 to 500 characters")]
    Reason,
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl erp_i18n::Localize for ResetError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::Message;
        match self {
            Self::Yourself => Message::new(messages::RESET_YOURSELF),
            Self::TheOwner => Message::new(messages::RESET_THE_OWNER),
            Self::NotAMember => Message::new(messages::NOT_A_MEMBER),
            Self::PlatformStaff => Message::new(messages::RESET_PLATFORM_STAFF),
            Self::AnotherCompany => Message::new(messages::RESET_ANOTHER_COMPANY),
            Self::NoLogin => Message::new(messages::RESET_NO_LOGIN),
            Self::Reason => Message::new(messages::RESET_REASON),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}

/// Why there was no address to mail the enrolment link to.
///
/// **Only a missing password row is [`ResetError::NoLogin`].** That answer is a
/// flat statement about somebody else's account — *"there is nowhere to send
/// it, nothing was changed"* — and a caller who reads it about a colleague who
/// plainly signs in by email will report a fault that is not there and will not
/// retry. A database that is briefly unreachable is a database that is briefly
/// unreachable: it gets a 500 through [`ResetError::Auth`], which is L6's
/// refuse-don't-degrade in the one place where degrading would look like a fact.
fn no_address(e: AuthError) -> ResetError {
    match e {
        AuthError::InvalidCredentials => ResetError::NoLogin,
        other => ResetError::Auth(other),
    }
}

/// What confirming an enrolment hands back, **once**.
#[derive(Debug, Clone)]
pub struct Enrolled {
    /// Single-use codes, in the clear. Only ever returned here: the database
    /// keeps their digests, so this list cannot be produced again.
    pub recovery_codes: Vec<String>,
}

impl ControlPlane {
    /// **Starts an enrolment.** Nothing about the identity's logins changes
    /// until [`Self::confirm_second_factor`] proves the app has the secret.
    ///
    /// Enrolling again replaces a pending enrolment, which is what somebody
    /// does when they scanned the code into the wrong phone.
    ///
    /// `link` is the token from an enrolment email, and is needed only by an
    /// account somebody else's reset left link-only — see
    /// [`Self::enrolment_permitted`].
    ///
    /// # Errors
    /// [`AuthError::EnrolmentLinkRequired`] for a link-only account with no
    /// live token. If the random source or the sealing key fails, or the
    /// database does.
    pub async fn begin_second_factor(
        &self,
        identity: IdentityId,
        issuer: &str,
        account: &str,
        sealing: &erp_eventlog::SealingKey,
        link: Option<&str>,
    ) -> Result<Enrolment, AuthError> {
        self.enrolment_permitted(identity, link).await?;
        let secret = crate::totp::generate().map_err(|e| AuthError::Hash(e.to_string()))?;
        let sealed = sealing
            .seal(&binding(identity), &secret)
            .map_err(|e| AuthError::Hash(e.to_string()))?;

        // The key id goes with the secret on both arms: a re-enrolment after
        // a rotation that kept the old id would name a key the blob is not
        // under, and `stored_secret` would refuse it.
        sqlx::query!(
            "INSERT INTO authenticator (id, identity_id, kind, handle, secret, sealed_with)
             VALUES ($1, $2, 'totp_pending', $3, $4, $5)
             ON CONFLICT (kind, handle) DO UPDATE
                SET secret = EXCLUDED.secret, sealed_with = EXCLUDED.sealed_with",
            uuid::Uuid::new_v4(),
            identity.as_uuid(),
            identity.to_string(),
            base64(&sealed),
            sealing.id(),
        )
        .execute(&self.pool)
        .await?;

        Ok(Enrolment {
            uri: crate::totp::provisioning_uri(issuer, account, &secret),
            secret: crate::totp::base32(&secret),
        })
    }

    /// **Proves the app has the secret, and turns the enrolment on.**
    ///
    /// Returns the recovery codes, which are shown once and never again.
    ///
    /// **Every other session of the identity ends**, in the same transaction.
    /// `start_session` refuses a password-only session once a factor exists,
    /// but the ones minted *before* it existed were still alive — somebody who
    /// phished the password at nine and signed in kept that session through
    /// the owner enrolling at ten, and through a platform door that asks only
    /// whether a factor is enrolled. After this, a session alive while a factor
    /// exists either went through it or is `keep`, the one that just proved it.
    /// `None` keeps nothing.
    ///
    /// `link` is the token from an enrolment email, needed only by an account
    /// somebody else's reset left link-only — see [`Self::enrolment_permitted`].
    /// A confirmation that gets through clears that state and spends **every**
    /// outstanding link of the identity, in this function's own transaction.
    ///
    /// # Errors
    /// [`AuthError::EnrolmentLinkRequired`] for a link-only account with no
    /// live token. [`AuthError::InvalidCredentials`] if the code is wrong or
    /// there is no pending enrolment — one error, because telling them apart
    /// tells an attacker whether somebody is mid-enrolment.
    #[expect(
        clippy::too_many_arguments,
        reason = "three separate proofs — the new code, the old factor, the link — and none is interchangeable with another"
    )]
    pub async fn confirm_second_factor(
        &self,
        identity: IdentityId,
        code: &str,
        previous: Option<&str>,
        now: Timestamp,
        sealing: &erp_eventlog::SealingKey,
        keep: Option<&str>,
        link: Option<&str>,
    ) -> Result<Enrolled, AuthError> {
        self.enrolment_permitted(identity, link).await?;

        // **Replacing a factor is removing one**, and the removal happens
        // below without any proof of what is being removed. A session alone was
        // enough to point an enrolment at somebody else's authenticator app and
        // destroy all ten recovery codes on the way — worse than
        // `disable_second_factor` was, because the attacker ends up *holding* a
        // factor rather than merely dropping one, and the paper that would have
        // let the owner back in is gone.
        //
        // Proved before anything is deleted, and a recovery code counts: losing
        // the phone is the case re-enrolling exists for.
        if self.has_second_factor(identity).await? {
            let Some(previous) = previous else {
                return Err(AuthError::SecondFactorRequired);
            };
            self.verify_second_factor(identity, previous, now, sealing)
                .await?;
        }

        let pending = self
            .stored_secret(identity, "totp_pending", sealing)
            .await?;
        let Some(secret) = pending else {
            return Err(AuthError::InvalidCredentials);
        };
        if !crate::totp::verify(&secret, code, seconds(now), crate::totp::DRIFT) {
            return Err(AuthError::InvalidCredentials);
        }

        let mut tx = self.pool.begin().await?;

        // The pending row becomes the live one, and any previous enrolment and
        // its recovery codes go with it. Re-enrolling is how somebody replaces
        // a lost phone, so the old factor must not survive it.
        sqlx::query!(
            "DELETE FROM authenticator
              WHERE identity_id = $1 AND kind IN ('totp', 'recovery')",
            identity.as_uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // **Exactly one pending row, or nothing commits.** Two confirmations
        // of one enrolment at once both pass every check above; the one that
        // commits second would otherwise delete the first one's new factor and
        // its codes, rename nothing, and commit ten recovery codes beside no
        // `totp` at all — a password-only account again, which is the state
        // `disable_second_factor` refuses to leave anybody a factor is required
        // of. The row renamed here stays locked until the commit, so what
        // commits always has a factor.
        let renamed = sqlx::query!(
            "UPDATE authenticator SET kind = 'totp'
              WHERE identity_id = $1 AND kind = 'totp_pending'",
            identity.as_uuid(),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if renamed != 1 {
            return Err(AuthError::InvalidCredentials);
        }

        let mut codes = Vec::with_capacity(RECOVERY_CODES);
        for index in 0..RECOVERY_CODES {
            let code = recovery_code().map_err(|e| AuthError::Hash(e.to_string()))?;
            sqlx::query!(
                "INSERT INTO authenticator (id, identity_id, kind, handle, secret)
                 VALUES ($1, $2, 'recovery', $3, $4)",
                uuid::Uuid::new_v4(),
                identity.as_uuid(),
                format!("{identity}:{index}"),
                digest(&code),
            )
            .execute(&mut *tx)
            .await?;
            codes.push(code);
        }

        sqlx::query!(
            "DELETE FROM session WHERE identity_id = $1 AND token_hash IS DISTINCT FROM $2",
            identity.as_uuid(),
            keep.map(crate::auth::SessionToken::digest),
        )
        .execute(&mut *tx)
        .await?;

        // **The account is no longer link-only, and every link it was sent is
        // spent** — both here, so an enrolment that rolls back leaves neither
        // changed. All of them and not just the one presented: two resets in a
        // row leave two live links, and the second must not still enrol after
        // the first has.
        sqlx::query!(
            "UPDATE identity SET second_factor_reset_at = NULL
              WHERE id = $1 AND second_factor_reset_at IS NOT NULL",
            identity.as_uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE second_factor_reset SET used_at = now()
              WHERE identity_id = $1 AND used_at IS NULL",
            identity.as_uuid(),
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        // After the commit, for the reason `log_out` gives.
        if let Some(shared) = &self.shared {
            shared.forget_sessions_of(identity).await;
        }
        Ok(Enrolled {
            recovery_codes: codes,
        })
    }

    /// Whether this identity must present a second factor.
    ///
    /// # Errors
    /// If the database does.
    pub async fn has_second_factor(&self, identity: IdentityId) -> Result<bool, AuthError> {
        let found = sqlx::query_scalar!(
            "SELECT 1 FROM authenticator WHERE identity_id = $1 AND kind = 'totp'",
            identity.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    /// **Checks a second factor**, accepting either a code from the app or one
    /// of the recovery codes.
    ///
    /// A recovery code is spent when it is used. A TOTP code is refused if it
    /// is the one last accepted, which is what stops a shoulder-surfed code
    /// being replayed inside its own thirty seconds.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] for anything that does not check out.
    pub async fn verify_second_factor(
        &self,
        identity: IdentityId,
        code: &str,
        now: Timestamp,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<(), AuthError> {
        let code = code.trim();
        if let Some(secret) = self.stored_secret(identity, "totp", sealing).await?
            && crate::totp::verify(&secret, code, seconds(now), crate::totp::DRIFT)
        {
            // **Spent, so it cannot be replayed.** `handle` carries the last
            // accepted code's digest; a repeat of it is refused even though the
            // arithmetic still says yes.
            let spent = sqlx::query_scalar!(
                "SELECT created_at FROM authenticator
                  WHERE identity_id = $1 AND kind = 'totp' AND secret LIKE $2",
                identity.as_uuid(),
                format!("%|{}", digest(code)),
            )
            .fetch_optional(&self.pool)
            .await?;
            if spent.is_some() {
                return Err(AuthError::InvalidCredentials);
            }
            self.remember_spent(identity, code).await?;
            return Ok(());
        }

        // Not a TOTP code, or the wrong one. A recovery code is the other way
        // in, and spending it is the whole point of it existing.
        let spent = sqlx::query!(
            "DELETE FROM authenticator
              WHERE identity_id = $1 AND kind = 'recovery' AND secret = $2
              RETURNING id",
            identity.as_uuid(),
            digest(code),
        )
        .fetch_optional(&self.pool)
        .await?;

        if spent.is_some() {
            Ok(())
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }

    /// **Turns the second factor off**, and takes the recovery codes with it.
    ///
    /// # Why this asks for the factor it is about to remove
    ///
    /// It used to need a live session and nothing else, which made a stolen one
    /// enough to strip the control the theft was supposed to run into. A
    /// session is a bearer token left on shared machines and in browser
    /// history; the factor is the thing somebody has to *hold*.
    ///
    /// **The factor rather than the password**, because an attacker who has
    /// worked their way to a session may well have the password too — that is
    /// the usual way they got close. Only the factor is evidence they do not
    /// have.
    ///
    /// **And it strands nobody**, which is what keeps it inside *switching a
    /// control on must not be the act that strands you*: anybody who cannot
    /// present a code or a recovery code cannot log in to reach this route
    /// either. It closes no door that was open.
    ///
    /// `code` is `None` only for an identity with nothing live to prove —
    /// somebody abandoning an enrolment they started and never confirmed.
    ///
    /// **Refused for anybody a factor is required of** —
    /// [`Self::second_factor_required_by`] — before a code is checked, so a
    /// recovery code is not spent on a refusal. Replacing the factor stays
    /// open. It refuses them even with only a pending enrolment to drop,
    /// which costs nothing: starting another enrolment replaces that one.
    ///
    /// # Errors
    /// [`AuthError::SecondFactorKept`] when a factor is required of them,
    /// [`AuthError::SecondFactorRequired`] when one is enrolled and no code
    /// came, [`AuthError::InvalidCredentials`] when it is wrong.
    pub async fn disable_second_factor(
        &self,
        identity: IdentityId,
        code: Option<&str>,
        now: Timestamp,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<(), AuthError> {
        if let Some(by) = self.second_factor_required_by(identity).await? {
            return Err(AuthError::SecondFactorKept(by));
        }

        if self.has_second_factor(identity).await? {
            let Some(code) = code else {
                return Err(AuthError::SecondFactorRequired);
            };
            self.verify_second_factor(identity, code, now, sealing)
                .await?;
        }

        sqlx::query!(
            "DELETE FROM authenticator
              WHERE identity_id = $1 AND kind IN ('totp', 'totp_pending', 'recovery')",
            identity.as_uuid(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// **Whether this identity must keep a second factor, and who says so.**
    /// The one answer [`Self::disable_second_factor`] asks.
    ///
    /// An account with a password and no factor gets its next factor from
    /// whoever enrols first, which may be somebody holding only the password.
    /// So removal is refused wherever that would matter:
    ///
    /// - **staff** — any live platform row, whatever its role; `grant_staff`
    ///   refuses to make one without a factor, and this keeps it that way;
    /// - **a live membership of a tenant that requires one.** A suspended
    ///   tenant counts: it is reinstated with its requirement, and an account
    ///   that dropped its factor meanwhile would walk back in through the gap.
    ///   So does one still provisioning. A deleted one does not — nobody
    ///   enters it again, which is also why `tenants_for_identity` leaves it
    ///   out.
    ///
    /// Staff is the answer when both hold. Read from the database, not the
    /// tenant cache: a requirement just switched on binds the next request
    /// here, on every node.
    ///
    /// # Errors
    /// If the database does.
    pub async fn second_factor_required_by(
        &self,
        identity: IdentityId,
    ) -> Result<Option<FactorRequiredBy>, AuthError> {
        let row = sqlx::query!(
            r#"SELECT EXISTS (SELECT 1 FROM membership
                               WHERE identity_id = $1 AND scope_kind = 'platform'
                                 AND revoked_at IS NULL) as "staff!",
                      EXISTS (SELECT 1 FROM membership m
                                JOIN tenant t ON t.id = m.tenant_id
                               WHERE m.identity_id = $1 AND m.revoked_at IS NULL
                                 AND t.requires_second_factor
                                 AND t.status <> 'deleted') as "tenant!""#,
            identity.as_uuid(),
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(if row.staff {
            Some(FactorRequiredBy::Staff)
        } else if row.tenant {
            Some(FactorRequiredBy::Tenant)
        } else {
            None
        })
    }

    /// **May this account enrol a second factor at all right now?**
    ///
    /// Yes, unless somebody else reset its factor — then only the link they
    /// were mailed gets through, and that holds **after the link expires**.
    /// The state is a fact about the account (`identity.second_factor_reset_at`)
    /// rather than the life of a row, because a timer would make waiting the
    /// hour out the attacker's move: the link goes, the account is
    /// password-only again, and whoever holds the password enrols.
    ///
    /// An account that never had a factor has the column NULL and is
    /// unaffected — first enrolment by whoever holds the password stays the
    /// accepted gap (Round 3 decision B).
    ///
    /// # Errors
    /// [`AuthError::EnrolmentLinkRequired`] when it is link-only and the token
    /// is missing, wrong, spent or expired — one answer for all four, for the
    /// reason `PasswordError::NotValid` gives.
    pub async fn enrolment_permitted(
        &self,
        identity: IdentityId,
        link: Option<&str>,
    ) -> Result<(), AuthError> {
        let link_only = sqlx::query_scalar!(
            r#"SELECT (second_factor_reset_at IS NOT NULL) as "link_only!"
                 FROM identity WHERE id = $1"#,
            identity.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(false);
        if !link_only {
            return Ok(());
        }

        let Some(link) = link else {
            return Err(AuthError::EnrolmentLinkRequired);
        };
        // **Not spent here.** `confirm_second_factor` spends it, in the
        // transaction that writes the factor, so a begin that goes nowhere —
        // the QR scanned into the wrong phone — does not cost the link.
        let live = sqlx::query_scalar!(
            "SELECT 1 FROM second_factor_reset
              WHERE token_hash = $1 AND identity_id = $2
                AND used_at IS NULL AND expires_at > now()",
            crate::auth::EnrolmentToken::digest_of(link),
            identity.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await?;
        if live.is_some() {
            Ok(())
        } else {
            Err(AuthError::EnrolmentLinkRequired)
        }
    }

    /// **Resets a member's second factor, for their company.**
    ///
    /// `by` must already have been judged permitted by the caller — the tenant
    /// route asks whether they own the tenant or hold `hr:reset_second_factor`,
    /// which needs the tenant's own database and so cannot be asked here. What
    /// *is* here is everything about the **target**, because all of it is
    /// control-plane fact:
    ///
    /// 1. not a live member of this tenant — the same 404 as every other route
    ///    about a member, and it goes first so nothing below says anything
    ///    about somebody the caller cannot already list;
    /// 2. themselves — removing your own is `disable_second_factor`;
    /// 3. the owner — nobody in a tenant is above its owner;
    /// 4. platform staff — the platform's own route, with `ManageStaff`;
    /// 5. **a live membership of any other tenant.** The factor is the
    ///    account's everywhere, so one company must not be able to weaken
    ///    somebody's sign-in at another. Support resets those.
    ///
    /// # Errors
    /// [`ResetError`], one variant per refusal above, or the database.
    pub async fn reset_member_second_factor(
        &self,
        tenant: TenantId,
        by: IdentityId,
        target: IdentityId,
        locale: erp_i18n::Locale,
        link_base: &str,
    ) -> Result<crate::auth::EnrolmentToken, ResetError> {
        let rows = sqlx::query!(
            r#"SELECT m.tenant_id as "tenant!: TenantId", m.role
                 FROM membership m
                 JOIN tenant t ON t.id = m.tenant_id
                WHERE m.identity_id = $1 AND m.revoked_at IS NULL
                  AND t.status <> 'deleted'"#,
            target.as_uuid(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(AccessError::Database)?;

        let here = rows
            .iter()
            .find(|row| row.tenant == tenant)
            .ok_or(ResetError::NotAMember)?;
        if by == target {
            return Err(ResetError::Yourself);
        }
        // A stored role this build does not know is corrupt data, not a guess
        // — the same rule `parse_platform_role` states.
        let role = here
            .role
            .parse::<crate::Role>()
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;
        if role == crate::Role::Owner {
            return Err(ResetError::TheOwner);
        }
        if self.platform_role(target).await?.is_some() {
            return Err(ResetError::PlatformStaff);
        }
        if rows.iter().any(|row| row.tenant != tenant) {
            return Err(ResetError::AnotherCompany);
        }

        self.reset_second_factor_by(
            target,
            Actor::identity(by),
            Some(tenant),
            serde_json::json!({ "by": "member" }),
            locale,
            link_base,
        )
        .await
    }

    /// **Resets anybody's second factor, for the platform.**
    ///
    /// The route has already proved `by` is staff who may
    /// [`PlatformPower::ResetSecondFactors`](crate::PlatformPower::ResetSecondFactors),
    /// which is support and superadmin. Two things are decided here:
    ///
    /// - **resetting your own is refused**, at this route as at the tenant's,
    ///   so no staff member can shed the factor `disable_second_factor` refuses
    ///   to let them drop;
    /// - **resetting platform staff needs `ManageStaff`**, so support cannot
    ///   reset a superadmin's — otherwise the narrower role would be the way to
    ///   the wider one.
    ///
    /// The reason is required and is what the audit entry carries; this is the
    /// route for somebody who works for two companies, and the record of why is
    /// the only thing either company will ever see.
    ///
    /// # Errors
    /// [`ResetError::Yourself`], [`ResetError::Reason`], [`AccessError::StaffOnly`]
    /// naming `manage_staff`, or the database.
    pub async fn reset_any_second_factor(
        &self,
        by: IdentityId,
        target: IdentityId,
        reason: &str,
        locale: erp_i18n::Locale,
        link_base: &str,
    ) -> Result<crate::auth::EnrolmentToken, ResetError> {
        if by == target {
            return Err(ResetError::Yourself);
        }
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(ResetError::Reason);
        }
        if self.platform_role(target).await?.is_some() {
            self.staff_may(by, crate::PlatformPower::ManageStaff)
                .await?;
        }

        self.reset_second_factor_by(
            target,
            Actor::identity(by),
            None,
            serde_json::json!({ "by": "platform", "reason": reason }),
            locale,
            link_base,
        )
        .await
    }

    /// **The reset itself**, which both routes reach and nothing else does.
    ///
    /// One transaction takes the factor away, marks the account link-only,
    /// writes the link, and ends every session; the mail goes in the same one
    /// (D9), so a rollback mails nobody and a crash loses no promised send. The
    /// other nodes' session caches are cleared after the commit, the way
    /// `log_out` and `confirm_second_factor` do it.
    ///
    /// **This is not [`Self::disable_second_factor`] and must not be confused
    /// with it.** That one is somebody dropping their *own* factor, costs a
    /// code, and is refused outright wherever a factor is required of them
    /// ([`AuthError::SecondFactorKept`]). This is an authorised removal by
    /// **somebody else**, which is why it takes no code — the person it is for
    /// has lost the thing a code comes from — and why it does not ask
    /// [`Self::second_factor_required_by`]: a member of a tenant that requires
    /// a factor is exactly who needs this, and they are refused entry until
    /// they enrol again, which is the same answer a new member gets.
    ///
    /// The account it leaves behind is **not** password-only: it is link-only,
    /// so the window a removal would otherwise open is closed in the same
    /// transaction that opens it.
    ///
    /// # Errors
    /// [`ResetError::NoLogin`] when there is no address to mail to — refused
    /// rather than resetting into a lockout — or the database.
    async fn reset_second_factor_by(
        &self,
        target: IdentityId,
        actor: Actor,
        tenant: Option<TenantId>,
        detail: serde_json::Value,
        locale: erp_i18n::Locale,
        link_base: &str,
    ) -> Result<crate::auth::EnrolmentToken, ResetError> {
        // Before anything is written: a reset with nowhere to send the link is
        // a lockout with extra steps.
        let handle = self.password_handle(target).await.map_err(no_address)?;

        let (token, digest) = crate::auth::EnrolmentToken::mint().map_err(AccessError::Auth)?;
        let id = uuid::Uuid::now_v7();

        let mut tx = self.pool.begin().await.map_err(AccessError::Database)?;
        sqlx::query!(
            "DELETE FROM authenticator
              WHERE identity_id = $1 AND kind IN ('totp', 'totp_pending', 'recovery')",
            target.as_uuid(),
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;
        sqlx::query!(
            "UPDATE identity SET second_factor_reset_at = now() WHERE id = $1",
            target.as_uuid(),
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;
        sqlx::query!(
            "INSERT INTO second_factor_reset (id, token_hash, identity_id, expires_at)
             VALUES ($1, $2, $3, now() + ($4::BIGINT * INTERVAL '1 second'))",
            id,
            digest,
            target.as_uuid(),
            ENROLMENT_LIFETIME_SECONDS,
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;
        // **Every session, including the ones this person is holding.** Whoever
        // took the phone may also be holding a session minted through it.
        sqlx::query!(
            "DELETE FROM session WHERE identity_id = $1",
            target.as_uuid(),
        )
        .execute(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        let (subject, body) =
            crate::mail::enrolment_messages(&format!("{link_base}{}", token.expose()));
        let email = crate::mail::Email::rendered(&crate::CATALOG, locale, handle, &subject, &body);
        erp_eventlog::enqueue(&mut tx, None, &[email.promised(format!("enrolment:{id}"))])
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;
        self.record(
            &mut tx,
            actor,
            tenant,
            "second_factor.reset",
            "identity",
            &target.to_string(),
            detail,
        )
        .await?;
        tx.commit().await.map_err(AccessError::Database)?;

        // After the commit, for the reason `log_out` gives.
        if let Some(shared) = &self.shared {
            shared.forget_sessions_of(target).await;
        }
        Ok(token)
    }

    /// Collects spent and expired enrolment links. Registered beside the other
    /// sweeps.
    ///
    /// **It reopens nothing.** The link-only state is
    /// `identity.second_factor_reset_at`, not the row, so an account whose link
    /// expired unused is still link-only afterwards and still needs a fresh one.
    ///
    /// # Errors
    /// If the database does.
    pub async fn sweep_enrolment_links(&self) -> Result<u64, AccessError> {
        Ok(
            sqlx::query!("DELETE FROM second_factor_reset WHERE expires_at < now()")
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?
                .rows_affected(),
        )
    }

    /// How many recovery codes are left, so somebody can be told to make more.
    ///
    /// # Errors
    /// If the database does.
    pub async fn recovery_codes_left(&self, identity: IdentityId) -> Result<i64, AuthError> {
        Ok(sqlx::query_scalar!(
            r#"SELECT count(*) as "n!" FROM authenticator
                WHERE identity_id = $1 AND kind = 'recovery'"#,
            identity.as_uuid(),
        )
        .fetch_one(&self.pool)
        .await?)
    }

    async fn stored_secret(
        &self,
        identity: IdentityId,
        kind: &str,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<Option<Vec<u8>>, AuthError> {
        let row = sqlx::query!(
            "SELECT secret, sealed_with FROM authenticator WHERE identity_id = $1 AND kind = $2",
            identity.as_uuid(),
            kind,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let bytes = unbase64(sealed_part(&row.secret)).ok_or(AuthError::InvalidCredentials)?;
        let secret = sealing
            .unseal(row.sealed_with.as_deref(), &binding(identity), &bytes)
            .map_err(|e| AuthError::Hash(e.to_string()))?;
        Ok(Some(secret))
    }

    /// **Moves every second factor onto the current sealing key**, or with
    /// `apply` false, only looks — see [`erp_eventlog::secrets::reseal`], which
    /// this is for the control plane. Platform staff's factors are the same
    /// rows as everybody else's, so they move too.
    ///
    /// A row with no recorded key — enrolled before `0020` — is opened under
    /// any held key and gets the current id. The swap compares the sealed part
    /// only and keeps the spent-code marker behind it, so a login between the
    /// read and the write neither loses its marker nor loses the row.
    ///
    /// # Errors
    /// If the database or sealing does. A secret nothing opens is reported in
    /// [`erp_eventlog::Census::unsealable`] by its binding and left alone.
    pub async fn reseal_second_factors(
        &self,
        sealing: &erp_eventlog::SealingKey,
        apply: bool,
    ) -> Result<erp_eventlog::Census, AuthError> {
        let mut census = erp_eventlog::Census::default();
        let rows = sqlx::query!(
            "SELECT id, identity_id, secret, sealed_with FROM authenticator
              WHERE kind IN ('totp', 'totp_pending')"
        )
        .fetch_all(&self.pool)
        .await?;

        // Every row is opened, those under the current id included — see
        // `secrets::reseal` for why — and only the stale ones are written.
        for row in rows {
            let identity = IdentityId::from_uuid(row.identity_id);
            let old = sealed_part(&row.secret);
            let Some(secret) = unbase64(old).and_then(|bytes| {
                sealing
                    .unseal(row.sealed_with.as_deref(), &binding(identity), &bytes)
                    .ok()
            }) else {
                census.unsealable.push(binding(identity));
                continue;
            };
            if apply && row.sealed_with.as_deref() != Some(sealing.id()) {
                let sealed = sealing
                    .seal(&binding(identity), &secret)
                    .map_err(|e| AuthError::Hash(e.to_string()))?;
                let moved = sqlx::query!(
                    "UPDATE authenticator
                        SET secret = $2 || substr(secret, length(split_part(secret, '|', 1)) + 1),
                            sealed_with = $3
                      WHERE id = $1 AND split_part(secret, '|', 1) = $4",
                    row.id,
                    base64(&sealed),
                    sealing.id(),
                    old,
                )
                .execute(&self.pool)
                .await?;
                census.resealed += moved.rows_affected();
            }
        }

        census.under = sqlx::query!(
            r#"SELECT coalesce(sealed_with, '(unrecorded)') as "sealed_with!", count(*) as "n!"
                 FROM authenticator
                WHERE kind IN ('totp', 'totp_pending')
                GROUP BY 1"#
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| (row.sealed_with, row.n.unsigned_abs()))
        .collect();

        Ok(census)
    }

    async fn remember_spent(&self, identity: IdentityId, code: &str) -> Result<(), AuthError> {
        sqlx::query!(
            "UPDATE authenticator
                SET secret = split_part(secret, '|', 1) || '|' || $2
              WHERE identity_id = $1 AND kind = 'totp'",
            identity.as_uuid(),
            digest(code),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// The sealed secret, without the spent-code marker `remember_spent` appends
/// after a pipe.
fn sealed_part(stored: &str) -> &str {
    stored.split('|').next().unwrap_or(stored)
}

/// **What the secret is sealed against.** Binding the ciphertext to the identity
/// means a row moved to another identity's id will not open, so a database
/// write cannot transplant somebody's second factor onto another account.
fn binding(identity: IdentityId) -> String {
    format!("second_factor:{identity}")
}

fn digest(code: &str) -> String {
    hex::encode(sha2::Sha256::digest(code.trim().as_bytes()))
}

/// Ten characters from an alphabet with no `0`/`O` or `1`/`l`, because these
/// are read off paper and typed by somebody who has already lost their phone.
fn recovery_code() -> Result<String, crate::totp::TotpError> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 10];
    getrandom::fill(&mut bytes).map_err(|e| crate::totp::TotpError::Crypto(e.to_string()))?;
    let mut code: String = bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect();
    code.insert(5, '-');
    Ok(code)
}

fn seconds(now: Timestamp) -> u64 {
    u64::try_from(now.timestamp()).unwrap_or(0)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from(buffer[0]) << 16 | u32::from(buffer[1]) << 8 | u32::from(buffer[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> (18 - i * 6)) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut out = Vec::new();
    for c in text.chars().filter(|c| *c != '=') {
        let index = u32::try_from(B64.iter().position(|b| char::from(*b) == c)?).ok()?;
        bits = (bits << 6) | index;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(((bits >> held) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A fault is not a fact about the account.** `NoLogin` tells the caller
    /// their colleague has no email login and that nothing was changed; a
    /// connection that dropped while looking the address up must not be able to
    /// say that.
    #[test]
    fn a_database_fault_is_not_an_account_with_nowhere_to_mail() {
        assert!(
            matches!(
                no_address(AuthError::InvalidCredentials),
                ResetError::NoLogin
            ),
            "a missing password row is the one thing NoLogin means"
        );
        assert!(
            matches!(
                no_address(AuthError::Database(sqlx::Error::PoolClosed)),
                ResetError::Auth(AuthError::Database(_))
            ),
            "a database fault was reported as an account with no email login"
        );
    }

    #[test]
    fn base64_round_trips() {
        for input in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0u8, 255, 128, 1, 2, 3][..],
        ] {
            assert_eq!(unbase64(&base64(input)).unwrap(), input, "{input:?}");
        }
    }

    #[test]
    fn base64_matches_the_published_vectors() {
        // RFC 4648 §10.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "encoding {input:?}");
        }
    }

    #[test]
    fn a_recovery_code_avoids_the_characters_people_misread() {
        for _ in 0..50 {
            let code = recovery_code().unwrap();
            assert_eq!(code.len(), 11, "ten characters and a dash");
            assert!(code.contains('-'));
            for c in code.chars().filter(|c| *c != '-') {
                assert!(
                    !matches!(c, '0' | 'O' | '1' | 'I' | 'L'),
                    "{c} is misread off paper, in {code}"
                );
            }
        }
    }

    #[test]
    fn two_recovery_codes_are_not_the_same_code() {
        let a = recovery_code().unwrap();
        let b = recovery_code().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_sealing_binding_names_the_identity() {
        let one = IdentityId::new();
        let two = IdentityId::new();
        assert_ne!(
            binding(one),
            binding(two),
            "a secret must not open under another identity"
        );
    }
}
