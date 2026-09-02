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
    fn digest(token: &str) -> Vec<u8> {
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
    /// The link that proves a signup's email address.
    ///
    /// Worth more than it looks: holding it is what turns a request into an
    /// account, a tenant and a database, so it is treated exactly as an
    /// invitation link is.
    SignupToken
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
        self.start_session(identity).await
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
    pub async fn start_session(
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
