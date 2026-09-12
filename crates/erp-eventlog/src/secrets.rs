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
//!   seal:    key + name + plaintext ──► 0x02 ‖ nonce ‖ AES-256-GCM(plaintext, aad = name) ‖ tag
//!   unseal:  key + name + that      ──► plaintext, or an error. Never a guess.
//! ```
//!
//! AES-256-GCM through OpenSSL, which this workspace already links for
//! Postgres TLS. The nonce is 12 random bytes and lives in the ciphertext, so a
//! row is self-describing and there is no second column to fall out of step
//! with the first.
//!
//! # A value is bound to the name it was sealed under
//!
//! The row's `key` — `payments.card.A`, `tax_sa.csid` — is the associated data.
//! The first version sealed with none, so anybody with SQL write access could
//! copy the blob from one row onto another and it would unseal under the new
//! name: a tenant's card token presented as a different tenant's, a retired
//! signing key presented as the current one. GCM authenticates the associated
//! data along with the ciphertext, so a blob moved to another row now fails to
//! unseal, at no cost. The leading version byte is what lets a row sealed
//! before this still be read — see [`SealingKey::unseal`].
//!
//! # What it does not protect against
//!
//! Somebody who has the sealing key **and** the database. That is the trade:
//! this turns "a leaked backup exposes every tenant's signing key" into "a
//! leaked backup is useless without the deployment's environment", which is the
//! difference worth having. Splitting the key into an HSM or KMS is the next
//! step up and changes only [`SealingKey`].
//!
//! # Rotation
//!
//! A deployment holds a list of keys: the first seals, the rest are only read
//! (`SEALING_KEY=<id>:<hex>[,<id>:<hex>…]`, see [`SealingKey::parse`]). Every
//! row records the id it was sealed under — `module_secret.sealed_with` here,
//! `authenticator.sealed_with` in the control plane — and [`SealingKey::unseal`]
//! opens a row with **that key and no other**, refusing an id it does not hold
//! with [`SecretError::UnknownKey`] rather than trying its luck (L6).
//!
//! [`reseal`] moves one tenant's rows from whatever key they are under to the
//! current one, and counts what is left under each id; `migrator reseal` runs it
//! over the control plane and every tenant, and `migrator reseal check` is the
//! gate an old key is retired behind. The procedure is in `docs/RUNNING.md`.
//!
//! **The id lives in the column, not in the envelope.** The envelope stays
//! `0x02`, so a build from before rotation existed, given the same single key,
//! still reads everything written since; a new format byte would also collide
//! with first-format values whose random first byte happened to match it.

use std::collections::BTreeMap;

use openssl::rand::rand_bytes;
use openssl::symm::Cipher;
use sqlx::PgConnection;

/// Bytes of nonce, at the front of every sealed value.
const NONCE: usize = 12;
/// Bytes of GCM tag, at the back.
const TAG: usize = 16;
/// AES-256.
const KEY: usize = 32;
/// The leading byte of a value sealed with its row name as associated data.
/// The first format had no version byte; `0x02` says "this is the second".
const BOUND: u8 = 0x02;

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
    /// The row names a key this deployment does not hold. Not folded into
    /// [`Self::Unsealable`], because the answer is different — put the key back
    /// in `SEALING_KEY` — and the id is in the row for anybody who can read it.
    #[error("the secret {key} was sealed under {id}, which this deployment does not hold")]
    UnknownKey { key: String, id: String },
    #[error("sealing failed: {0}")]
    Crypto(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for SecretError {
    fn message(&self) -> erp_i18n::Message {
        // Every one of these is a deployment fault or an attack, never
        // something a user typed.
        erp_i18n::Message::new(crate::messages::INTERNAL)
    }
}

/// The keys a deployment holds: one it seals with, and any it still reads.
///
/// Held as bytes and never printed: the `Debug` impl shows the identifiers,
/// which is what a log line needs and all it may have.
#[derive(Clone)]
pub struct SealingKey {
    /// What [`Self::seal`] uses, and the id [`put`] records.
    current: Key,
    /// Keys a rotation is moving away from. Read, never sealed with.
    previous: Vec<Key>,
}

#[derive(Clone)]
struct Key {
    /// Which key this is, recorded beside every row sealed under it. An
    /// identifier, never the key.
    id: String,
    bytes: [u8; KEY],
}

impl std::fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let previous: Vec<&str> = self.previous.iter().map(|k| k.id.as_str()).collect();
        f.debug_struct("SealingKey")
            .field("current", &self.current.id)
            .field("previous", &previous)
            .field("bytes", &"<withheld>")
            .finish()
    }
}

impl Key {
    fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, SecretError> {
        let bytes: [u8; KEY] = bytes
            .try_into()
            .map_err(|_| SecretError::KeyLength { found: bytes.len() })?;
        Ok(Self {
            id: id.into(),
            bytes,
        })
    }

    /// Bound to `key` first, then the unbound first format — see
    /// [`SealingKey::unseal`].
    fn unseal(&self, key: &str, sealed: &[u8]) -> Option<Vec<u8>> {
        if let Some((&BOUND, bound)) = sealed.split_first()
            && let Ok(plaintext) = self.open(key.as_bytes(), bound)
        {
            return Some(plaintext);
        }
        self.open(&[], sealed).ok()
    }

    /// One attempt at `nonce ‖ ciphertext ‖ tag` under `aad`.
    fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, ()> {
        if sealed.len() <= NONCE + TAG {
            return Err(());
        }
        let (nonce, rest) = sealed.split_at(NONCE);
        let (ciphertext, tag) = rest.split_at(rest.len() - TAG);
        openssl::symm::decrypt_aead(
            Cipher::aes_256_gcm(),
            &self.bytes,
            Some(nonce),
            aad,
            ciphertext,
            tag,
        )
        .map_err(|_| ())
    }
}

impl SealingKey {
    /// A key from raw bytes, and no other.
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, SecretError> {
        Ok(Self {
            current: Key::new(id, bytes)?,
            previous: Vec::new(),
        })
    }

    /// Keys from `<id>:<64 hex characters>[,<id>:<64 hex characters>…]`, which
    /// is how a deployment configures them. **The first seals**; the rest are
    /// read, so a rotation can hold the old key while it moves rows off it.
    ///
    /// The identifier is in the same string deliberately: a deployment that
    /// carries ids and keys separately has two lists to keep in step.
    ///
    /// Refused: an empty entry or id, a repeated id (which key would it be?),
    /// and the same bytes under two ids — a "rotation" that only renamed the
    /// key, after which the compromised one is still sealing. Errors name the
    /// entry or its id, never any hex.
    pub fn parse(configured: &str) -> Result<Self, SecretError> {
        let mut keys: Vec<Key> = Vec::new();
        for (at, entry) in configured.split(',').enumerate() {
            let place = || format!("entry {}", at + 1);
            let (id, hex) = entry.split_once(':').ok_or_else(|| {
                SecretError::KeyFormat(place(), "expected <id>:<64 hex characters>".to_owned())
            })?;
            let id = id.trim();
            if id.is_empty() {
                return Err(SecretError::KeyFormat(
                    place(),
                    "the id is empty".to_owned(),
                ));
            }
            let bytes = hex::decode(hex.trim())
                .map_err(|e| SecretError::KeyFormat(id.to_owned(), e.to_string()))?;
            let key = Key::new(id, &bytes)?;
            if let Some(clash) = keys
                .iter()
                .find(|held| held.id == key.id || held.bytes == key.bytes)
            {
                return Err(SecretError::KeyFormat(
                    id.to_owned(),
                    format!(
                        "repeats {}: a rotation needs a new id and new bytes",
                        clash.id
                    ),
                ));
            }
            keys.push(key);
        }
        // `split` yields at least one entry, and each was pushed or refused.
        let current = keys.remove(0);
        Ok(Self {
            current,
            previous: keys,
        })
    }

    /// A fresh random key, for a deployment that is generating its first one and
    /// for tests.
    pub fn generate(id: impl Into<String>) -> Result<Self, SecretError> {
        let mut bytes = [0u8; KEY];
        rand_bytes(&mut bytes).map_err(|e| SecretError::Crypto(e.to_string()))?;
        Self::new(id, &bytes)
    }

    /// The id [`Self::seal`] seals under.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.current.id
    }

    /// `0x02 ‖ nonce ‖ ciphertext ‖ tag`, with `key` — the row's name — as the
    /// associated data, so the result unseals under that name and no other.
    pub fn seal(&self, key: &str, plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
        let mut nonce = [0u8; NONCE];
        rand_bytes(&mut nonce).map_err(|e| SecretError::Crypto(e.to_string()))?;

        let mut tag = [0u8; TAG];
        let ciphertext = openssl::symm::encrypt_aead(
            Cipher::aes_256_gcm(),
            &self.current.bytes,
            Some(&nonce),
            key.as_bytes(),
            plaintext,
            &mut tag,
        )
        .map_err(|e| SecretError::Crypto(e.to_string()))?;

        let mut sealed = Vec::with_capacity(1 + NONCE + ciphertext.len() + TAG);
        sealed.push(BOUND);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        sealed.extend_from_slice(&tag);
        Ok(sealed)
    }

    /// The plaintext, or an error. Never a guess: GCM authenticates, so a
    /// tampered value — or one sealed under another name — fails rather than
    /// decrypting to something.
    ///
    /// **Opened with the key the row names.** `sealed_with` is the id recorded
    /// beside it; only that key is tried, and an id this ring does not hold is
    /// [`SecretError::UnknownKey`]. `None` is a value sealed before its id was
    /// recorded — a second factor enrolled before `authenticator.sealed_with`
    /// existed — and is tried under every held key, current first, which GCM
    /// makes a refusal rather than a guess when the key is wrong.
    ///
    /// **Reads both formats.** A value that begins with [`BOUND`] is tried as
    /// bound to `key`; failing that, or without the byte, it is tried as the
    /// unbound first format. Trying both is safe because GCM refuses a wrong
    /// parse outright: a legacy value whose random first nonce byte happens to
    /// be `0x02` fails the bound attempt and passes the legacy one, and a
    /// bound value can never pass as legacy. What this cannot do is protect a
    /// legacy row from being moved; `put` writes the bound format, so any row
    /// written or rotated since is protected, and a rotation over every row
    /// finishes the job.
    pub fn unseal(
        &self,
        sealed_with: Option<&str>,
        key: &str,
        sealed: &[u8],
    ) -> Result<Vec<u8>, SecretError> {
        let mut held = std::iter::once(&self.current).chain(&self.previous);
        let opened = match sealed_with {
            Some(id) => held
                .find(|k| k.id == id)
                .ok_or_else(|| SecretError::UnknownKey {
                    key: key.to_owned(),
                    id: id.to_owned(),
                })?
                .unseal(key, sealed),
            // ponytail: legacy-only. Delete once a `migrator reseal check`
            // across every deployment shows no `(unrecorded)` row.
            None => held.find_map(|k| k.unseal(key, sealed)),
        };
        opened.ok_or_else(|| SecretError::Unsealable {
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
    let sealed = sealing.seal(key, plaintext)?;
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
        "SELECT sealed, sealed_with FROM module_secret WHERE key = $1",
        key,
    )
    .fetch_optional(&mut *conn)
    .await?;

    row.map(|row| sealing.unseal(Some(&row.sealed_with), key, &row.sealed))
        .transpose()
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

/// Removes a secret. Used by a module replacing credentials it has finished
/// with, and by anything undoing an onboarding that failed half way.
pub async fn forget(conn: &mut PgConnection, key: &str) -> Result<(), SecretError> {
    sqlx::query!("DELETE FROM module_secret WHERE key = $1", key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// What [`reseal`] found, and what it did — for one database, or summed over a
/// fleet with [`Census::absorb`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    /// How many sealed values are under each key id, counted after the sweep.
    pub under: BTreeMap<String, u64>,
    /// How many values the sweep moved to the current key.
    pub resealed: u64,
    /// Values no held key opens, by name and never by content. Left exactly as
    /// they were: a row the sweep cannot read is not one it may overwrite.
    pub unsealable: Vec<String>,
}

impl Census {
    /// Adds another database's census, naming `place` on its unsealable rows.
    pub fn absorb(&mut self, other: Self, place: &str) {
        for (id, n) in other.under {
            *self.under.entry(id).or_default() += n;
        }
        self.resealed += other.resealed;
        self.unsealable.extend(
            other
                .unsealable
                .into_iter()
                .map(|name| format!("{place}: {name}")),
        );
    }

    /// **Whether a key other than `current` can be retired**: every value is
    /// under `current`, and none failed to open.
    #[must_use]
    pub fn is_settled(&self, current: &str) -> bool {
        self.unsealable.is_empty() && self.under.keys().all(|id| id == current)
    }
}

/// **Moves one tenant's secrets onto the current key**, or with `apply` false,
/// only looks. Either way it unseals **every** row, those already under the
/// current id included: a row under the current id that does not open — the id
/// reused for new bytes, say — is as lost as one under a retired key, and a
/// gate that skipped it would wave the only key that opens it out of the ring.
///
/// Resumable and idempotent. Each row is its own compare-and-swap on the value
/// read, so a `put` or `forget` racing the sweep wins and the row is left for
/// the next run, and a rerun picks up whatever is left. `updated_at` is not
/// touched: it says when the secret was replaced, and this does not replace it.
///
/// # Errors
/// If the database does, or sealing fails. A row that will not unseal is not an
/// error: it goes in [`Census::unsealable`] and the sweep carries on.
pub async fn reseal(
    conn: &mut PgConnection,
    sealing: &SealingKey,
    apply: bool,
) -> Result<Census, SecretError> {
    let mut census = Census::default();

    // ponytail: every row at once; page it if one tenant ever holds ~10^5.
    let rows = sqlx::query!("SELECT key, sealed, sealed_with FROM module_secret")
        .fetch_all(&mut *conn)
        .await?;

    for row in rows {
        let Ok(plaintext) = sealing.unseal(Some(&row.sealed_with), &row.key, &row.sealed) else {
            census.unsealable.push(row.key);
            continue;
        };
        if apply && row.sealed_with != sealing.id() {
            let moved = sqlx::query!(
                "UPDATE module_secret SET sealed = $1, sealed_with = $2
                  WHERE key = $3 AND sealed = $4",
                &sealing.seal(&row.key, &plaintext)?,
                sealing.id(),
                row.key,
                row.sealed,
            )
            .execute(&mut *conn)
            .await?;
            census.resealed += moved.rows_affected();
        }
    }

    census.under = sqlx::query!(
        r#"SELECT sealed_with, count(*) as "n!" FROM module_secret GROUP BY sealed_with"#
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| (row.sealed_with, row.n.unsigned_abs()))
    .collect();

    Ok(census)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SealingKey {
        SealingKey::new("test", &[7u8; KEY]).expect("32 bytes")
    }

    #[test]
    fn a_secret_survives_the_round_trip() {
        let sealed = key().seal("k", b"a private key").expect("seals");
        assert_eq!(
            key().unseal(Some("test"), "k", &sealed).expect("unseals"),
            b"a private key"
        );
    }

    /// **A value unseals under the name it was sealed for and no other.** The
    /// first version let anybody with SQL write access copy the blob from
    /// `payments.card.A` onto `payments.card.B` and read it there.
    #[test]
    fn a_secret_moved_to_another_row_does_not_unseal() {
        let key = key();
        let sealed = key.seal("payments.card.A", b"tok_a").expect("seals");
        assert!(
            matches!(
                key.unseal(Some("test"), "payments.card.B", &sealed),
                Err(SecretError::Unsealable { .. })
            ),
            "a blob sealed for A unsealed under B"
        );
        assert_eq!(
            key.unseal(Some("test"), "payments.card.A", &sealed)
                .expect("unseals"),
            b"tok_a"
        );
    }

    /// A row written before names were bound still reads, so a deployment does
    /// not lose every signing key on upgrade; it is re-sealed bound on the
    /// next `put`.
    #[test]
    fn a_value_sealed_by_the_first_format_still_unseals() {
        let key = key();
        // The first format, built by hand: nonce ‖ ciphertext ‖ tag, no
        // version byte, no associated data.
        let mut legacy = vec![0u8; NONCE];
        rand_bytes(&mut legacy).expect("nonce");
        let mut tag = [0u8; TAG];
        let ciphertext = openssl::symm::encrypt_aead(
            Cipher::aes_256_gcm(),
            &key.current.bytes,
            Some(&legacy),
            &[],
            b"an old key",
            &mut tag,
        )
        .expect("encrypts");
        legacy.extend_from_slice(&ciphertext);
        legacy.extend_from_slice(&tag);

        assert_eq!(
            key.unseal(Some("test"), "tax_sa.csid", &legacy)
                .expect("unseals"),
            b"an old key"
        );

        // Including one whose random first byte is the version byte: the bound
        // attempt fails authentication and the legacy attempt is still made.
        legacy[0] = BOUND;
        let mut tag = [0u8; TAG];
        let ciphertext = openssl::symm::encrypt_aead(
            Cipher::aes_256_gcm(),
            &key.current.bytes,
            Some(&legacy[..NONCE]),
            &[],
            b"an old key",
            &mut tag,
        )
        .expect("encrypts");
        legacy.truncate(NONCE);
        legacy.extend_from_slice(&ciphertext);
        legacy.extend_from_slice(&tag);
        assert_eq!(
            key.unseal(Some("test"), "tax_sa.csid", &legacy)
                .expect("unseals"),
            b"an old key"
        );
    }

    /// The plaintext must not be recoverable from the row, which is the entire
    /// point of the column being `BYTEA` and not `TEXT`.
    #[test]
    fn the_sealed_bytes_do_not_contain_the_plaintext() {
        let plaintext = b"-----BEGIN EC PRIVATE KEY-----";
        let sealed = key().seal("k", plaintext).expect("seals");
        assert!(
            !sealed.windows(plaintext.len()).any(|w| w == plaintext),
            "the plaintext is sitting in the ciphertext"
        );
        assert_eq!(sealed.len(), 1 + NONCE + plaintext.len() + TAG);
        assert_eq!(sealed[0], BOUND);
    }

    /// Two seals of the same value differ, or the nonce is not doing its job and
    /// equal ciphertexts leak that two tenants share a secret.
    #[test]
    fn sealing_twice_gives_two_different_ciphertexts() {
        let key = key();
        assert_ne!(
            key.seal("k", b"same").expect("seals"),
            key.seal("k", b"same").expect("seals")
        );
    }

    #[test]
    fn another_key_cannot_unseal_it() {
        let sealed = key().seal("k", b"a private key").expect("seals");
        let other = SealingKey::new("other", &[9u8; KEY]).expect("32 bytes");
        assert!(matches!(
            other.unseal(Some("test"), "k", &sealed),
            Err(SecretError::UnknownKey { id, .. }) if id == "test"
        ));
        // The right id over the wrong bytes is the one answer GCM gives.
        let impostor = SealingKey::new("test", &[9u8; KEY]).expect("32 bytes");
        assert!(matches!(
            impostor.unseal(Some("test"), "k", &sealed),
            Err(SecretError::Unsealable { .. })
        ));
    }

    fn ring(configured: &[(&str, u8)]) -> SealingKey {
        let list: Vec<String> = configured
            .iter()
            .map(|(id, byte)| format!("{id}:{}", hex::encode([*byte; KEY])))
            .collect();
        SealingKey::parse(&list.join(",")).expect("parses")
    }

    /// **The rotation itself.** A value sealed under the old key still opens
    /// once the new one is current, and what the ring seals opens under the new
    /// key alone — so the old one can go once nothing is left under it.
    #[test]
    fn a_value_sealed_under_the_previous_key_unseals_after_rotation() {
        let sealed = ring(&[("old", 1)]).seal("k", b"csid").expect("seals");
        let rotating = ring(&[("new", 2), ("old", 1)]);
        assert_eq!(
            rotating.unseal(Some("old"), "k", &sealed).expect("unseals"),
            b"csid"
        );

        let resealed = rotating.seal("k", b"csid").expect("seals");
        assert_eq!(rotating.id(), "new");
        assert_eq!(
            ring(&[("new", 2)])
                .unseal(Some("new"), "k", &resealed)
                .expect("unseals"),
            b"csid"
        );
        assert!(matches!(
            ring(&[("old", 1)]).unseal(Some("new"), "k", &resealed),
            Err(SecretError::UnknownKey { id, .. }) if id == "new"
        ));
    }

    /// **The recorded id decides, not whichever key happens to open it.** The
    /// same bytes under another name are not the key the row names; trying
    /// every key would open it and hide that the deployment lost `old`.
    #[test]
    fn a_value_under_a_key_the_ring_does_not_hold_is_refused_by_name() {
        let sealed = ring(&[("old", 1)]).seal("k", b"csid").expect("seals");
        assert!(matches!(
            ring(&[("renamed", 1)]).unseal(Some("old"), "k", &sealed),
            Err(SecretError::UnknownKey { key, id }) if key == "k" && id == "old"
        ));
    }

    /// A value sealed before its id was recorded opens under any key the ring
    /// holds, and under none it does not.
    #[test]
    fn an_unrecorded_value_opens_under_any_held_key() {
        let sealed = ring(&[("old", 1)]).seal("k", b"totp").expect("seals");
        assert_eq!(
            ring(&[("new", 2), ("old", 1)])
                .unseal(None, "k", &sealed)
                .expect("unseals"),
            b"totp"
        );
        assert!(matches!(
            ring(&[("new", 2)]).unseal(None, "k", &sealed),
            Err(SecretError::Unsealable { .. })
        ));
    }

    #[test]
    fn a_configured_ring_is_the_current_key_first() {
        let hex = |byte: u8| hex::encode([byte; KEY]);
        let parsed =
            SealingKey::parse(&format!(" new:{} , old:{} ", hex(2), hex(1))).expect("parses");
        assert_eq!(parsed.id(), "new", "the first entry seals");
        assert_eq!(parsed.previous.len(), 1);
        assert_eq!(parsed.previous[0].id, "old");

        for (refused, why) in [
            (format!("a:{},a:{}", hex(2), hex(1)), "a repeated id"),
            (
                format!("new:{},old:{}", hex(2), hex(2)),
                "the same bytes under two ids",
            ),
            (format!("new:{},", hex(2)), "an empty entry"),
            (format!(":{}", hex(2)), "an empty id"),
        ] {
            assert!(
                matches!(SealingKey::parse(&refused), Err(SecretError::KeyFormat(..))),
                "{why} was accepted"
            );
        }
        assert!(
            matches!(
                SealingKey::parse(&format!("new:{},old:{}", hex(2), "ab".repeat(16))),
                Err(SecretError::KeyLength { found: 16 })
            ),
            "a short previous key must not be padded into a long one"
        );
    }

    /// The gate an old key is retired behind: everything under the current key
    /// and nothing that failed to open.
    #[test]
    fn a_census_is_settled_only_when_nothing_is_left_behind() {
        let mut census = Census::default();
        assert!(census.is_settled("new"), "nothing sealed is nothing left");
        census.under.insert("new".to_owned(), 3);
        assert!(census.is_settled("new"));

        let mut behind = census.clone();
        behind.under.insert("old".to_owned(), 1);
        assert!(!behind.is_settled("new"), "a value is still under old");

        let mut stuck = census.clone();
        stuck.absorb(
            Census {
                unsealable: vec!["tax_sa.csid".to_owned()],
                ..Census::default()
            },
            "acme",
        );
        assert_eq!(stuck.unsealable, ["acme: tax_sa.csid"]);
        assert!(!stuck.is_settled("new"), "a value nothing opens");
    }

    /// GCM authenticates. A flipped bit is refused rather than decrypted into
    /// something that is not what was stored.
    #[test]
    fn a_tampered_value_is_refused_rather_than_decrypted() {
        let key = key();
        for at in [1, NONCE + 2] {
            let mut sealed = key.seal("k", b"a private key").expect("seals");
            sealed[at] ^= 0x01;
            assert!(
                matches!(
                    key.unseal(Some("test"), "k", &sealed),
                    Err(SecretError::Unsealable { .. })
                ),
                "a bit flipped at {at} was accepted"
            );
        }
        // And so is a value too short to be one, in either format.
        assert!(key.unseal(Some("test"), "k", &[0u8; NONCE + TAG]).is_err());
        assert!(
            key.unseal(Some("test"), "k", &[BOUND; 1 + NONCE + TAG])
                .is_err()
        );
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

    /// A key that is never printed is one that cannot be logged by accident,
    /// the one being retired included.
    #[test]
    fn the_key_is_not_in_its_own_debug_output() {
        let key = ring(&[("2026-09", 0xAB), ("2026-03", 0xCD)]);
        let shown = format!("{key:?}");
        assert!(
            shown.contains("2026-09") && shown.contains("2026-03"),
            "the identifiers are useful in a log: {shown}"
        );
        for byte in ["171", "ab", "205", "cd"] {
            assert!(!shown.contains(byte), "{shown}");
        }
        assert!(shown.contains("withheld"));
    }
}
