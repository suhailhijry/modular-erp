//! Sealed storage, for the things a module must keep and must not reveal.
//!
//! # What this is for
//!
//! One thing, so far: ZATCA onboarding hands a tenant an ECDSA private key and a
//! CSID secret. Neither is derived from anything, both must survive a projection
//! rebuild, both must be rotatable, and neither may be readable by everything
//! that can read the tenant. See `migrations/tenant/0006_module_secret.sql` for
//! why none of the three places a module already had could hold them.
//!
//! # The shape
//!
//! ```text
//!   seal:    key + plaintext ──► nonce ‖ AES-256-GCM(plaintext) ‖ tag
//!   unseal:  key + that      ──► plaintext, or an error. Never a guess.
//! ```
//!
//! AES-256-GCM through OpenSSL, which this workspace already links for
//! Postgres TLS. The nonce is 12 random bytes and lives in the ciphertext, so a
//! row is self-describing and there is no second column to fall out of step
//! with the first.
//!
//! # What it does not protect against
//!
//! Somebody who has the sealing key **and** the database. That is the trade:
//! this turns "a leaked backup exposes every tenant's signing key" into "a
//! leaked backup is useless without the deployment's environment", which is the
//! difference worth having. Splitting the key into an HSM or KMS is the next
//! step up and changes only [`SealingKey`].

use openssl::rand::rand_bytes;
use openssl::symm::Cipher;
use sqlx::PgConnection;

/// Bytes of nonce, at the front of every sealed value.
const NONCE: usize = 12;
/// Bytes of GCM tag, at the back.
const TAG: usize = 16;
/// AES-256.
const KEY: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The configured key is not 32 bytes. Refused at startup rather than
    /// padded, because a padded key is a key somebody thinks is 256 bits.
    #[error("a sealing key is {found} bytes; it must be {KEY}")]
    KeyLength { found: usize },
    #[error("{0} is not a sealing key: {1}")]
    KeyFormat(String, String),
    /// The value did not decrypt. **Never distinguished further** — a wrong key,
    /// a truncated row and a tampered row are one answer, because telling them
    /// apart is telling an attacker which they achieved.
    #[error("the secret {key} could not be unsealed with this key")]
    Unsealable { key: String },
    #[error("sealing failed: {0}")]
    Crypto(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl spa_i18n::Localize for SecretError {
    fn message(&self) -> spa_i18n::Message {
        // Every one of these is a deployment fault or an attack, never
        // something a user typed.
        spa_i18n::Message::new(crate::messages::INTERNAL)
    }
}

/// The key a deployment seals with.
///
/// Held as bytes and never printed: the `Debug` impl shows the identifier and
/// the length, which is what a log line needs and all it may have.
#[derive(Clone)]
pub struct SealingKey {
    /// Which key this is, recorded beside every row it seals so a rotation can
    /// find what it has not re-sealed. An identifier, never the key.
    id: String,
    bytes: [u8; KEY],
}

impl std::fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealingKey")
            .field("id", &self.id)
            .field("bytes", &"<32 bytes, withheld>")
            .finish()
    }
}

impl SealingKey {
    /// A key from raw bytes.
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, SecretError> {
        let bytes: [u8; KEY] = bytes
            .try_into()
            .map_err(|_| SecretError::KeyLength { found: bytes.len() })?;
        Ok(Self {
            id: id.into(),
            bytes,
        })
    }

    /// A key from `<id>:<64 hex characters>`, which is how a deployment
    /// configures one.
    ///
    /// The identifier is in the same string deliberately: a rotation means two
    /// keys existing at once, and a deployment that carries them separately has
    /// two things to keep in step.
    pub fn parse(configured: &str) -> Result<Self, SecretError> {
        let (id, hex) = configured.split_once(':').ok_or_else(|| {
            SecretError::KeyFormat(
                configured.chars().take(8).collect(),
                "expected <id>:<64 hex characters>".to_owned(),
            )
        })?;
        let bytes = hex::decode(hex.trim())
            .map_err(|e| SecretError::KeyFormat(id.to_owned(), e.to_string()))?;
        Self::new(id, &bytes)
    }

    /// A fresh random key, for a deployment that is generating its first one and
    /// for tests.
    pub fn generate(id: impl Into<String>) -> Result<Self, SecretError> {
        let mut bytes = [0u8; KEY];
        rand_bytes(&mut bytes).map_err(|e| SecretError::Crypto(e.to_string()))?;
        Ok(Self {
            id: id.into(),
            bytes,
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// `nonce ‖ ciphertext ‖ tag`.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
        let mut nonce = [0u8; NONCE];
        rand_bytes(&mut nonce).map_err(|e| SecretError::Crypto(e.to_string()))?;

        let mut tag = [0u8; TAG];
        let ciphertext = openssl::symm::encrypt_aead(
            Cipher::aes_256_gcm(),
            &self.bytes,
            Some(&nonce),
            &[],
            plaintext,
            &mut tag,
        )
        .map_err(|e| SecretError::Crypto(e.to_string()))?;

        let mut sealed = Vec::with_capacity(NONCE + ciphertext.len() + TAG);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        sealed.extend_from_slice(&tag);
        Ok(sealed)
    }

    /// The plaintext, or an error. Never a guess: GCM authenticates, so a
    /// tampered value fails rather than decrypting to something.
    pub fn unseal(&self, key: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError> {
        if sealed.len() <= NONCE + TAG {
            return Err(SecretError::Unsealable {
                key: key.to_owned(),
            });
        }
        let (nonce, rest) = sealed.split_at(NONCE);
        let (ciphertext, tag) = rest.split_at(rest.len() - TAG);

        openssl::symm::decrypt_aead(
            Cipher::aes_256_gcm(),
            &self.bytes,
            Some(nonce),
            &[],
            ciphertext,
            tag,
        )
        .map_err(|_| SecretError::Unsealable {
            key: key.to_owned(),
        })
    }
}

/// Stores a secret under a module's key, replacing whatever was there.
pub async fn put(
    conn: &mut PgConnection,
    sealing: &SealingKey,
    key: &str,
    plaintext: &[u8],
) -> Result<(), SecretError> {
    let sealed = sealing.seal(plaintext)?;
    sqlx::query!(
        "INSERT INTO module_secret (key, sealed, sealed_with)
         VALUES ($1, $2, $3)
         ON CONFLICT (key) DO UPDATE
            SET sealed = EXCLUDED.sealed,
                sealed_with = EXCLUDED.sealed_with,
                updated_at = now()",
        key,
        &sealed,
        sealing.id(),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The secret, unsealed. `None` when there is none — which is different from
/// one that will not unseal, and that difference is the whole reason this
/// returns a `Result<Option<_>>`.
pub async fn get(
    conn: &mut PgConnection,
    sealing: &SealingKey,
    key: &str,
) -> Result<Option<Vec<u8>>, SecretError> {
    let row = sqlx::query!(
        r#"SELECT sealed as "sealed!" FROM module_secret WHERE key = $1"#,
        key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|row| sealing.unseal(key, &row.sealed)).transpose()
}

/// Whether a secret is stored, without unsealing it.
///
/// For a status endpoint: "is this tenant onboarded?" must be answerable by
/// something that is not allowed to read the key.
pub async fn exists(conn: &mut PgConnection, key: &str) -> Result<bool, SecretError> {
    let found = sqlx::query_scalar!(
        r#"SELECT count(*) as "count!" FROM module_secret WHERE key = $1"#,
        key,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(found > 0)
}

/// Removes a secret. Used by a rotation that has finished with the old one, and
/// by anything undoing an onboarding that failed half way.
pub async fn forget(conn: &mut PgConnection, key: &str) -> Result<(), SecretError> {
    sqlx::query!("DELETE FROM module_secret WHERE key = $1", key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SealingKey {
        SealingKey::new("test", &[7u8; KEY]).expect("32 bytes")
    }

    #[test]
    fn a_secret_survives_the_round_trip() {
        let sealed = key().seal(b"a private key").expect("seals");
        assert_eq!(
            key().unseal("k", &sealed).expect("unseals"),
            b"a private key"
        );
    }

    /// The plaintext must not be recoverable from the row, which is the entire
    /// point of the column being `BYTEA` and not `TEXT`.
    #[test]
    fn the_sealed_bytes_do_not_contain_the_plaintext() {
        let plaintext = b"-----BEGIN EC PRIVATE KEY-----";
        let sealed = key().seal(plaintext).expect("seals");
        assert!(
            !sealed.windows(plaintext.len()).any(|w| w == plaintext),
            "the plaintext is sitting in the ciphertext"
        );
        assert_eq!(sealed.len(), NONCE + plaintext.len() + TAG);
    }

    /// Two seals of the same value differ, or the nonce is not doing its job and
    /// equal ciphertexts leak that two tenants share a secret.
    #[test]
    fn sealing_twice_gives_two_different_ciphertexts() {
        let key = key();
        assert_ne!(
            key.seal(b"same").expect("seals"),
            key.seal(b"same").expect("seals")
        );
    }

    #[test]
    fn another_key_cannot_unseal_it() {
        let sealed = key().seal(b"a private key").expect("seals");
        let other = SealingKey::new("other", &[9u8; KEY]).expect("32 bytes");
        assert!(matches!(
            other.unseal("k", &sealed),
            Err(SecretError::Unsealable { .. })
        ));
    }

    /// GCM authenticates. A flipped bit is refused rather than decrypted into
    /// something that is not what was stored.
    #[test]
    fn a_tampered_value_is_refused_rather_than_decrypted() {
        let key = key();
        for at in [0, NONCE + 1] {
            let mut sealed = key.seal(b"a private key").expect("seals");
            sealed[at] ^= 0x01;
            assert!(
                matches!(
                    key.unseal("k", &sealed),
                    Err(SecretError::Unsealable { .. })
                ),
                "a bit flipped at {at} was accepted"
            );
        }
        // And so is a value too short to be one.
        assert!(key.unseal("k", &[0u8; NONCE + TAG]).is_err());
    }

    #[test]
    fn a_key_is_thirty_two_bytes_or_it_is_refused() {
        assert!(matches!(
            SealingKey::new("short", &[1u8; 16]),
            Err(SecretError::KeyLength { found: 16 })
        ));
        assert!(SealingKey::generate("fresh").is_ok());
    }

    #[test]
    fn a_configured_key_is_id_then_hex() {
        let parsed = SealingKey::parse(&format!("2026-01:{}", "ab".repeat(KEY))).expect("parses");
        assert_eq!(parsed.id(), "2026-01");

        assert!(matches!(
            SealingKey::parse("no-colon-or-hex"),
            Err(SecretError::KeyFormat(..))
        ));
        assert!(
            matches!(
                SealingKey::parse("id:abcd"),
                Err(SecretError::KeyLength { found: 2 })
            ),
            "a short key must not be padded into a long one"
        );
    }

    /// A key that is never printed is one that cannot be logged by accident.
    #[test]
    fn the_key_is_not_in_its_own_debug_output() {
        let key = SealingKey::new("test", &[0xAB; KEY]).expect("32 bytes");
        let shown = format!("{key:?}");
        assert!(shown.contains("test"), "the identifier is useful in a log");
        assert!(!shown.contains("171") && !shown.contains("ab"), "{shown}");
        assert!(shown.contains("withheld"));
    }
}
