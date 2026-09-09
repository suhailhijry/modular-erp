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

use erp_types::{IdentityId, Timestamp};
use sha2::Digest as _;

use crate::{ControlPlane, auth::AuthError};

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
    /// # Errors
    /// If the random source or the sealing key fails, or the database does.
    pub async fn begin_second_factor(
        &self,
        identity: IdentityId,
        issuer: &str,
        account: &str,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<Enrolment, AuthError> {
        let secret = crate::totp::generate().map_err(|e| AuthError::Hash(e.to_string()))?;
        let sealed = sealing
            .seal(&binding(identity), &secret)
            .map_err(|e| AuthError::Hash(e.to_string()))?;

        sqlx::query!(
            "INSERT INTO authenticator (id, identity_id, kind, handle, secret)
             VALUES ($1, $2, 'totp_pending', $3, $4)
             ON CONFLICT (kind, handle) DO UPDATE SET secret = EXCLUDED.secret",
            uuid::Uuid::new_v4(),
            identity.as_uuid(),
            identity.to_string(),
            base64(&sealed),
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
    /// # Errors
    /// [`AuthError::InvalidCredentials`] if the code is wrong or there is no
    /// pending enrolment — one error, because telling them apart tells an
    /// attacker whether somebody is mid-enrolment.
    pub async fn confirm_second_factor(
        &self,
        identity: IdentityId,
        code: &str,
        now: Timestamp,
        sealing: &erp_eventlog::SealingKey,
    ) -> Result<Enrolled, AuthError> {
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
        sqlx::query!(
            "UPDATE authenticator SET kind = 'totp'
              WHERE identity_id = $1 AND kind = 'totp_pending'",
            identity.as_uuid(),
        )
        .execute(&mut *tx)
        .await?;

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

        tx.commit().await?;
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
    /// # Errors
    /// If the database does.
    pub async fn disable_second_factor(&self, identity: IdentityId) -> Result<(), AuthError> {
        sqlx::query!(
            "DELETE FROM authenticator
              WHERE identity_id = $1 AND kind IN ('totp', 'totp_pending', 'recovery')",
            identity.as_uuid(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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
        let row = sqlx::query_scalar!(
            "SELECT secret FROM authenticator WHERE identity_id = $1 AND kind = $2",
            identity.as_uuid(),
            kind,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(stored) = row else {
            return Ok(None);
        };
        // The spent-code marker is appended after a pipe; the secret is what
        // comes before it.
        let sealed = stored.split('|').next().unwrap_or(&stored);
        let bytes = unbase64(sealed).ok_or(AuthError::InvalidCredentials)?;
        let secret = sealing
            .unseal(&binding(identity), &bytes)
            .map_err(|e| AuthError::Hash(e.to_string()))?;
        Ok(Some(secret))
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
