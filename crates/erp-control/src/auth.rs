//! Proving an identity, and staying proved.

use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use erp_types::{IdentityId, Timestamp};

/// How long a new session lasts.
///
/// One value, not a setting, until someone asks for a different one.
pub const SESSION_LIFETIME: Duration = Duration::from_hours(12);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Wrong handle, wrong password, unknown handle, suspended identity — all
    /// one error on purpose. Telling them apart is a free account-enumeration
    /// oracle.
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("the session is expired or unknown")]
    NoSession,
    /// **The password was right and it is not enough.**
    ///
    /// Deliberately *not* folded into `InvalidCredentials`, unlike every other
    /// login failure: this one is only ever reached by somebody who has already
    /// proved the password, so it tells an attacker nothing they did not
    /// already know, and a client that cannot tell it apart cannot ask for the
    /// code.
    #[error("this account needs its second factor")]
    SecondFactorRequired,
    /// **A second factor that is required cannot be turned off** — by platform
    /// staff, or by a member of a tenant that requires one. An account with a
    /// password and no factor gets its next factor from whoever enrols first,
    /// which may be somebody holding only the password. Replacing the factor
    /// is still open; to drop it, come off the staff or out of the tenant, or
    /// have its owner stop requiring it. See
    /// [`ControlPlane::second_factor_required_by`](crate::ControlPlane::second_factor_required_by).
    #[error("a second factor is required of this account ({0:?})")]
    SecondFactorKept(crate::FactorRequiredBy),
    /// **Somebody else reset this account's factor, so only the link they were
    /// mailed may enrol the next one.** A password alone is not enough, and it
    /// stays that way after the link expires — waiting the hour out must not
    /// reopen the door. See
    /// [`ControlPlane::reset_second_factor_by`](crate::ControlPlane::reset_second_factor_by).
    #[error("this account can only enrol a second factor through its enrolment link")]
    EnrolmentLinkRequired,
    /// That login handle already belongs to somebody.
    ///
    /// Deliberately *not* folded into `InvalidCredentials`: this one reaches a
    /// caller that has already established who it is talking to, and the answer
    /// they need is "pick another address", not "wrong password".
    #[error("{0} already has an account")]
    HandleTaken(String),
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for AuthError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        match self {
            Self::InvalidCredentials => erp_i18n::Message::new(messages::INVALID_CREDENTIALS),
            Self::HandleTaken(handle) => erp_i18n::Message::new(messages::HANDLE_TAKEN)
                .with("handle", erp_i18n::MessageArg::text(handle.clone())),
            Self::NoSession => erp_i18n::Message::new(messages::SESSION_EXPIRED),
            Self::SecondFactorRequired => erp_i18n::Message::new(messages::SECOND_FACTOR_REQUIRED),
            Self::SecondFactorKept(crate::FactorRequiredBy::Staff) => {
                erp_i18n::Message::new(messages::STAFF_KEEPS_SECOND_FACTOR)
            }
            Self::SecondFactorKept(crate::FactorRequiredBy::Tenant) => {
                erp_i18n::Message::new(messages::TENANT_KEEPS_SECOND_FACTOR)
            }
            Self::EnrolmentLinkRequired => {
                erp_i18n::Message::new(messages::ENROLMENT_LINK_REQUIRED)
            }
            Self::Hash(_) | Self::Database(_) => erp_i18n::Message::new(messages::INTERNAL),
        }
    }
}

/// A session token. Only ever printed once, at login.
///
/// `Debug` is redacted: a token in a log line is a working credential, and log
/// lines outlive the sessions they mention.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// What is stored. See `0004_authentication.sql` for why this is not slow.
    pub fn digest(token: &str) -> Vec<u8> {
        use sha2::Digest;
        sha2::Sha256::digest(token.as_bytes()).to_vec()
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionToken(***)")
    }
}

/// Serializable because it is cached in Redis when a deployment has one — see
/// [`crate::shared`]. It carries no token, only what the token proved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub identity: IdentityId,
    pub expires_at: Timestamp,
}

/// A one-time link.
///
/// Each one is a separate type from [`SessionToken`] and from the others, never
/// a reuse: they are all opaque strings, none is interchangeable with another,
/// and the compiler is the cheapest place to find that out. A signup link
/// presented where an invitation link belongs would otherwise look up cleanly
/// against the wrong table and answer `NotValid`, which is a correct-looking
/// refusal for the wrong reason.
///
/// `Debug` is redacted on all of them. A one-time link in a log line is a
/// working credential, and log lines outlive the links they mention.
macro_rules! link_token {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub struct $name(String);

        impl $name {
            /// Mints one, returning it with what should be stored.
            pub(crate) fn mint() -> Result<(Self, Vec<u8>), AuthError> {
                let token = hex(&random_bytes()?);
                let digest = SessionToken::digest(&token);
                Ok((Self(token), digest))
            }

            /// What to look a presented token up by.
            pub(crate) fn digest_of(token: &str) -> Vec<u8> {
                SessionToken::digest(token)
            }

            #[must_use]
            pub fn expose(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(***)"))
            }
        }
    };
}

link_token! {
    /// The link that takes somebody into a tenant they were invited to.
    InvitationToken
}

link_token! {
    /// The link that lets somebody who has forgotten their password choose a
    /// new one.
    ///
    /// Worth what a password is worth and live for a great deal less time:
    /// holding it rewrites the password of the account it names. It does not
    /// touch that account's second factor and it issues no session — see
    /// `crate::passwords`, where both of those are the whole design.
    ResetToken
}

link_token! {
    /// The link that proves a signup's email address.
    ///
    /// Worth more than it looks: holding it is what turns a request into an
    /// account, a tenant and a database, so it is treated exactly as an
    /// invitation link is.
    SignupToken
}

link_token! {
    /// The link that lets somebody enrol a second factor after theirs was
    /// reset by their company's owner or by platform support.
    ///
    /// **Not a way in.** It issues no session and rewrites no password; the
    /// only thing it does is let its holder past
    /// [`ControlPlane::begin_second_factor`](crate::ControlPlane::begin_second_factor)
    /// and [`ControlPlane::confirm_second_factor`](crate::ControlPlane::confirm_second_factor),
    /// which still need the account's password to reach at all. See
    /// `crate::second_factor`.
    EnrolmentToken
}

/// 32 bytes from the OS. The one random source in this file.
/// A token a tenant publishes to prove they own a domain.
///
/// **Not a credential**, which is what makes it different from everything else
/// this module mints: it goes into a public DNS record, so it is not secret and
/// is stored in the clear rather than digested. What it has to be is
/// *unguessable*, so an attacker cannot publish the token a victim will be
/// issued before the victim asks for it.
///
/// Minted here rather than passed in, so no caller can supply a predictable
/// one — the same reason `sales` stopped taking a journal entry's id.
pub(crate) fn verification_token() -> Result<String, AuthError> {
    Ok(format!("erp-verify-{}", hex(&random_bytes()?)))
}

/// 32 bytes as hex, for an API key's two halves.
///
/// Minted here for the same reason every other credential in this file is: a
/// caller cannot supply a predictable one.
pub(crate) fn key_token() -> Result<String, AuthError> {
    Ok(hex(&random_bytes()?))
}

fn random_bytes() -> Result<[u8; 32], AuthError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(bytes)
}

/// Hashes a password for storage.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::encode_b64(&random_bytes()?[..16])
        .map_err(|e| AuthError::Hash(e.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Checks a password against a stored PHC string.
///
/// Runs even when the handle is unknown — see `log_in`.
fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// A hash of nothing, to spend the same time on an unknown handle as on a known
/// one.
///
/// Without it, "handle not found" returns in microseconds and "wrong password"
/// in ~50ms, which is an account-enumeration oracle that no amount of identical
/// error messages hides.
static DUMMY_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn dummy_hash() -> &'static str {
    DUMMY_HASH.get_or_init(|| {
        hash_password("this password is never anyone's").unwrap_or_else(|_| String::new())
    })
}

impl crate::ControlPlane {
    /// Registers a password login for an identity.
    ///
    /// # Why this insert has no `ON CONFLICT`
    ///
    /// It used to. `ON CONFLICT (kind, handle) DO UPDATE SET secret` is the
    /// right shape for *changing your own password* and a full account takeover
    /// for *registering a new one* — and this function had both callers. Signing
    /// up with somebody else's address overwrote their password, left the row
    /// pointing at their identity, and let the attacker log in as them. From an
    /// unauthenticated endpoint.
    ///
    /// So: a taken handle is an error, and every caller decides what that means.
    /// A future "change my password" gets its own function with
    /// `WHERE identity_id = $1` in it, which is the clause that makes the
    /// difference.
    ///
    /// Arguments are owned. Elided lifetimes on an `async fn` are what stop
    /// rustc proving a caller's future `Send`, and signup calls this — see
    /// `provision.rs`.
    pub async fn register_login(
        &self,
        identity: IdentityId,
        handle: String,
        password: String,
    ) -> Result<(), AuthError> {
        self.register_hashed_login(identity, handle, hash_password(&password)?)
            .await
    }

    /// [`Self::register_login`] with the hashing already done.
    ///
    /// For a flow that hashed the password earlier and has kept it somewhere
    /// other than `authenticator` since — which today means exactly one:
    /// `pending_signup` holds the hash until the address proves itself, because
    /// writing it here any sooner would claim the handle for an address nobody
    /// has answered from. See `migrations/control/0010_signups.sql`.
    ///
    /// `secret` must be a PHC string from [`hash_password`]. Nothing checks
    /// that, which is why this is not public: a caller that passed a plaintext
    /// would store a password that verifies against nothing, and the account
    /// would be unopenable rather than open.
    pub(crate) async fn register_hashed_login(
        &self,
        identity: IdentityId,
        handle: String,
        secret: String,
    ) -> Result<(), AuthError> {
        let handle = handle.trim().to_lowercase();

        let inserted = sqlx::query!(
            "INSERT INTO authenticator (id, identity_id, kind, handle, secret)
             VALUES ($1, $2, 'password', $3, $4)
             ON CONFLICT (kind, handle) DO NOTHING",
            uuid::Uuid::now_v7(),
            identity.as_uuid(),
            handle,
            secret,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if inserted == 0 {
            return Err(AuthError::HandleTaken(handle));
        }
        Ok(())
    }

    /// Exchanges a handle and password for a session token.
    ///
    /// Every failure is [`AuthError::InvalidCredentials`], and every failure
    /// costs the same time.
    pub async fn log_in(
        &self,
        handle: &str,
        password: &str,
    ) -> Result<(SessionToken, Session), AuthError> {
        let identity = self.authenticate(handle, password).await?;
        // The refusal is `start_session`'s now, so this is one call rather than
        // a check somebody could forget to copy. `log_in_with_second_factor` is
        // the way past it.
        self.start_session(identity).await
    }

    /// **A login that presents both factors at once.**
    ///
    /// See `second_factor.rs` on why there is no challenge token: nothing is
    /// created until both factors pass, so a half-authenticated session is not
    /// a state this system can be in.
    ///
    /// Accepts either a code from the authenticator app or one of the recovery
    /// codes. Works for an identity with no second factor too, ignoring the
    /// code — a client that always sends one does not need to know which
    /// accounts are enrolled.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] for a wrong password *or* a wrong
    /// code. The two are one error on the way in, for the reason the password
    /// failures are.
    pub async fn log_in_with_second_factor(
        &self,
        handle: &str,
        password: &str,
        code: &str,
        now: erp_types::Timestamp,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<(SessionToken, Session), AuthError> {
        let identity = self.authenticate(handle, password).await?;
        if self.has_second_factor(identity).await? {
            self.verify_second_factor(identity, code, now, sealing)
                .await?;
        }
        // **`issue_session`, because both factors are behind us.** Going back
        // through `start_session` would refuse the very identity that just
        // proved itself.
        self.issue_session(identity).await
    }

    /// Checks a password without issuing anything.
    ///
    /// The credential half of [`Self::log_in`], separated because two other
    /// flows need to know *who this is* without starting a session: signing up
    /// when the address already has an account, and accepting an invitation to
    /// one. Both must cost what a login costs — every failure is
    /// [`AuthError::InvalidCredentials`] and every failure takes the same time.
    pub async fn authenticate(
        &self,
        handle: &str,
        password: &str,
    ) -> Result<IdentityId, AuthError> {
        let row = sqlx::query!(
            r#"SELECT a.identity_id as "identity_id: IdentityId", a.secret, i.status
                 FROM authenticator a
                 JOIN identity i ON i.id = a.identity_id
                WHERE a.kind = 'password' AND a.handle = $1"#,
            handle.trim().to_lowercase(),
        )
        .fetch_optional(&self.pool)
        .await?;

        let stored = row.as_ref().map_or(dummy_hash(), |r| r.secret.as_str());
        let correct = verify_password(password, stored);

        // Both checks after the hash, so the timing is the same either way.
        let Some(row) = row else {
            return Err(AuthError::InvalidCredentials);
        };
        if !correct || row.status != "active" {
            return Err(AuthError::InvalidCredentials);
        }

        Ok(row.identity_id)
    }

    /// Issues a session without checking a credential.
    ///
    /// For flows that have already established who this is another way —
    /// signup, and later OIDC.
    /// **Mints a session, and cannot skip a second factor doing it.**
    ///
    /// The gate is here rather than in each caller because it *was* in each
    /// caller, and three of them did not have it. `log_in` carried the comment
    /// *"a path which never heard of a second factor cannot issue a session
    /// that skipped one"* while `otp::verify_code` and both signup paths for
    /// an address that already has an account minted sessions without ever
    /// asking — which made an enrolled identity takeable by anybody holding
    /// the password and the mailbox, the exact pair a second factor exists to
    /// survive.
    ///
    /// A promise every caller has to keep is one a caller eventually breaks.
    /// This is the same move the engine made when `evaluate` started taking
    /// its answer from `explain`: one implementation, so there is nothing to
    /// disagree with.
    ///
    /// # Errors
    /// [`AuthError::SecondFactorRequired`] when the identity has one. The way
    /// past it is to present it — [`Self::log_in_with_second_factor`].
    pub async fn start_session(
        &self,
        identity: IdentityId,
    ) -> Result<(SessionToken, Session), AuthError> {
        if self.has_second_factor(identity).await? {
            return Err(AuthError::SecondFactorRequired);
        }
        self.issue_session(identity).await
    }

    /// Mints a session for a caller that has **already** checked both factors.
    ///
    /// Private, and its two callers are both in this file, so the complete list
    /// of paths allowed to skip the gate is one screen long and stays that way.
    /// `issue_sessions_only_after_both_factors` is the test that keeps it one
    /// screen long.
    async fn issue_session(
        &self,
        identity: IdentityId,
    ) -> Result<(SessionToken, Session), AuthError> {
        let token = SessionToken(hex(&random_bytes()?));

        let expires_at = sqlx::query_scalar!(
            r#"INSERT INTO session (token_hash, identity_id, expires_at)
               VALUES ($1, $2, now() + ($3::BIGINT * INTERVAL '1 second'))
               RETURNING expires_at"#,
            SessionToken::digest(token.expose()),
            identity.as_uuid(),
            i64::try_from(SESSION_LIFETIME.as_secs()).unwrap_or(i64::MAX),
        )
        .fetch_one(&self.pool)
        .await?;

        Ok((
            token,
            Session {
                identity,
                expires_at,
            },
        ))
    }

    /// Resolves a token to the identity behind it.
    ///
    /// **Not cached.** Every other entry-path lookup is, because a five-second
    /// stale membership is survivable; a five-second stale *logout* is not.
    pub async fn session(&self, token: &str) -> Result<Session, AuthError> {
        let digest = SessionToken::digest(token);

        // **The shared cache, and only the shared one.**
        //
        // This lookup runs on every authenticated request and was the one hot
        // query with no cache in front of it, deliberately: an in-process cache
        // would make a logout take effect on the node that served it and
        // nowhere else, and a stale logout is not a survivable kind of stale.
        //
        // Shared, that objection goes away — a logout deletes the entry for
        // every node at once. See `crate::shared` for what a Redis outage costs
        // and what bounds it.
        if let Some(shared) = &self.shared
            && let Some(cached) = shared.session(&digest).await
        {
            // Expiry is still checked here rather than trusted to the key's TTL,
            // because the two are set from different clocks.
            if cached.expires_at > chrono::Utc::now() {
                return Ok(cached);
            }
            shared.forget_session(&digest).await;
            return Err(AuthError::NoSession);
        }

        let row = sqlx::query!(
            r#"SELECT identity_id as "identity_id: IdentityId", expires_at
                 FROM session
                WHERE token_hash = $1 AND expires_at > now()"#,
            digest,
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::NoSession)?;

        let session = Session {
            identity: row.identity_id,
            expires_at: row.expires_at,
        };
        if let Some(shared) = &self.shared {
            shared.remember_session(&digest, &session).await;
        }
        Ok(session)
    }

    /// Ends one session.
    pub async fn log_out(&self, token: &str) -> Result<(), AuthError> {
        let digest = SessionToken::digest(token);

        // **Postgres first.** It is the source of truth, and a cache cleared
        // before the record it caches would be re-populated by the next request
        // that arrived in between.
        sqlx::query!("DELETE FROM session WHERE token_hash = $1", digest)
            .execute(&self.pool)
            .await?;

        if let Some(shared) = &self.shared {
            shared.forget_session(&digest).await;
        }
        Ok(())
    }

    /// Ends every session for an identity. What "log out everywhere" and a
    /// suspension both call.
    pub async fn log_out_everywhere(&self, identity: IdentityId) -> Result<u64, AuthError> {
        let ended = sqlx::query!(
            "DELETE FROM session WHERE identity_id = $1",
            identity.as_uuid(),
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if let Some(shared) = &self.shared {
            shared.forget_sessions_of(identity).await;
        }
        Ok(ended)
    }

    /// Deletes expired sessions. For the reaper.
    pub async fn sweep_sessions(&self) -> Result<u64, AuthError> {
        Ok(
            sqlx::query!("DELETE FROM session WHERE expires_at <= now()")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let stored = hash_password("correct horse battery staple").expect("hashes");
        assert!(verify_password("correct horse battery staple", &stored));
        assert!(!verify_password("correct horse battery stapl", &stored));
        assert!(!verify_password("", &stored));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // Salted, so a stolen table does not reveal which accounts share a
        // password.
        let a = hash_password("hunter2").expect("hashes");
        let b = hash_password("hunter2").expect("hashes");
        assert_ne!(a, b);
        assert!(verify_password("hunter2", &a) && verify_password("hunter2", &b));
    }

    #[test]
    fn a_token_does_not_appear_in_debug_output() {
        let token = SessionToken("super-secret".to_owned());
        assert_eq!(format!("{token:?}"), "SessionToken(***)");
        assert!(!format!("{token:?}").contains("super-secret"));
    }

    #[test]
    fn verifying_against_a_corrupt_stored_hash_fails_rather_than_panics() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }
}
