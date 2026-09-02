//! Where a file actually lives.
//!
//! # What this crate knows, and what it must never learn
//!
//! It knows: bytes, a key, a checksum, and how to put and get them. It does not
//! know what a file is *for*, who it belongs to, or who may read it — those are
//! a module's, and the day this crate learns any of them is the day it stops
//! being swappable for a different engine.
//!
//! # An event stores a key, never a URL
//!
//! **A URL is where a file is today; a key is what it is.** A tenant who moves
//! from local disk to object storage, or from one bucket to another, has not
//! changed any of their documents — and an event log full of
//! `https://…/bucket-2019/…` would say otherwise for ever.
//!
//! So what a module writes down is `(engine, key, checksum, size, media_type)`,
//! which is [`Stored`]. Turning that into somewhere a browser can fetch is a
//! *read-time* concern, and it happens in the handler that already knows who is
//! asking.
//!
//! # The checksum is verified on read (L6)
//!
//! [`fetch`] recomputes it and refuses on a mismatch. A document that comes
//! back different from what was stored is a **failure**, not a warning: it means
//! the object store lost a write, a disk went bad, or something wrote over it —
//! and handing a customer a corrupted invoice with a note attached is worse than
//! handing them nothing.
//!
//! # Why the tenant chooses (D15)
//!
//! A module may ship to a customer's own cloud, and a business that keeps its
//! documents on its own hardware is not a configuration detail — it is the
//! reason some of them can buy this at all. So the engine is a trait, and which
//! one a tenant uses is theirs.

pub mod messages;

mod local;

pub use local::Local;

use erp_i18n::{Localize, Message, MessageArg, StaticCatalog};
use serde::{Deserialize, Serialize};

/// This crate's messages, in every supported language.
///
/// Composed into `erp_api::CATALOG` the way `erp_occupancy`'s and
/// `erp_links`' are: these are refusals the API renders, and a code with no
/// sentence behind it reaches a client as a bare string.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

/// The largest file this system will take.
///
/// Twenty-five mebibytes, which is a scanned twenty-page contract with room to
/// spare and small enough that a request holding one in memory is not a way to
/// take the process down. A tenant who needs more needs streaming, which is a
/// different shape rather than a bigger number.
pub const MAX_BYTES: usize = 25 * 1024 * 1024;

/// What a module writes down about a file.
///
/// **No URL.** See the crate docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    /// Which engine holds it — `local`, and whatever else is configured.
    pub engine: String,
    /// Where it is, in that engine's own terms.
    pub key: String,
    /// SHA-256, hex. Verified on every read.
    pub checksum: String,
    pub size: i64,
    /// What it is, as the uploader declared it.
    ///
    /// **Not sniffed.** Guessing a type from the first few bytes is how an
    /// HTML file becomes a "document" a browser renders in the tenant's own
    /// origin; a declared type that a handler serves with
    /// `Content-Disposition: attachment` cannot.
    pub media_type: String,
}

/// Why a file could not be stored or fetched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("no such file")]
    NoSuchFile,
    /// **The checksum did not match.** See the crate docs.
    #[error("{key} came back different from what was stored")]
    Corrupt { key: String },
    #[error("a file may not be larger than {MAX_BYTES} bytes")]
    TooLarge,
    #[error("{0} is not somewhere a file may be kept")]
    NotAKey(String),
    #[error("storage could not be reached: {0}")]
    Unavailable(String),
}

impl Localize for StorageError {
    fn message(&self) -> Message {
        match self {
            Self::NoSuchFile => Message::new(messages::NO_SUCH_FILE),
            Self::Corrupt { .. } => Message::new(messages::CORRUPT),
            Self::TooLarge => Message::new(messages::TOO_LARGE).with(
                "n",
                MessageArg::Count(i64::try_from(MAX_BYTES).unwrap_or(i64::MAX)),
            ),
            Self::NotAKey(key) => {
                Message::new(messages::NOT_A_KEY).with("key", MessageArg::text(key))
            }
            Self::Unavailable(_) => Message::new(messages::UNAVAILABLE),
        }
    }
}

/// Somewhere bytes can be kept.
#[async_trait::async_trait]
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// What this engine is called. Recorded on the event, so a tenant that adds
    /// a second engine can still find what the first one holds.
    fn engine(&self) -> &'static str;

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StorageError>;
    async fn get(&self, key: &str) -> Result<Vec<u8>, StorageError>;

    /// Removes it, or reports that it was already gone as success.
    ///
    /// Deleting twice is the same world either way (L8), and a caller cleaning
    /// up after a failed upload should not have to care which half succeeded.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
}

/// SHA-256, hex.
#[must_use]
pub fn checksum(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Stores bytes and describes what was stored.
///
/// The checksum is computed **here**, from the bytes that went in, so it is a
/// fact about what was written rather than about what a caller claimed.
pub async fn store(
    storage: &dyn Storage,
    key: &str,
    bytes: &[u8],
    media_type: &str,
) -> Result<Stored, StorageError> {
    if bytes.len() > MAX_BYTES {
        return Err(StorageError::TooLarge);
    }
    storage.put(key, bytes).await?;

    Ok(Stored {
        engine: storage.engine().to_owned(),
        key: key.to_owned(),
        checksum: checksum(bytes),
        size: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        media_type: media_type.to_owned(),
    })
}

/// Fetches a file **and proves it is the one that was stored**.
///
/// A mismatch is [`StorageError::Corrupt`] and never a warning with the bytes
/// attached: handing somebody a document that is not the document is worse than
/// handing them nothing, and it is the failure a checksum exists to catch.
pub async fn fetch(storage: &dyn Storage, stored: &Stored) -> Result<Vec<u8>, StorageError> {
    let bytes = storage.get(&stored.key).await?;
    if checksum(&bytes) != stored.checksum {
        return Err(StorageError::Corrupt {
            key: stored.key.clone(),
        });
    }
    Ok(bytes)
}

/// Whether a string is usable as a key in any engine.
///
/// **The traversal check, in one place.** A key becomes a path on local disk and
/// an object name in a bucket, and `../` means something in the first and
/// nothing in the second — so refusing it here is what stops one engine's rules
/// leaking into the other's safety.
pub fn check_key(key: &str) -> Result<(), StorageError> {
    let refuse = || StorageError::NotAKey(key.to_owned());

    if key.is_empty() || key.len() > 512 {
        return Err(refuse());
    }
    if key.starts_with('/') || key.ends_with('/') || key.contains("//") {
        return Err(refuse());
    }
    if key.split('/').any(|part| part == "." || part == "..") {
        return Err(refuse());
    }
    // Anything a filesystem or a URL would treat specially. Deliberately a
    // whitelist: a key is generated by this system, not typed by a person, so
    // there is nothing to be permissive for.
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err(refuse());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_a_flat_generated_path_and_nothing_clever() {
        assert!(check_key("invoice/INV-1/a1b2c3.pdf").is_ok());
        assert!(check_key("a").is_ok());

        assert!(check_key("").is_err());
        assert!(check_key("/leading").is_err());
        assert!(check_key("trailing/").is_err());
        assert!(check_key("double//slash").is_err());
    }

    /// **The traversal cases.** A key becomes a path on disk.
    #[test]
    fn a_key_cannot_climb_out_of_where_it_belongs() {
        assert!(check_key("../etc/passwd").is_err());
        assert!(check_key("invoice/../../etc/passwd").is_err());
        assert!(check_key("invoice/./x").is_err());
        assert!(check_key("invoice/x\0y").is_err());
        assert!(check_key("invoice/x y").is_err());
        assert!(check_key("invoice/~root").is_err());
    }

    #[test]
    fn the_checksum_is_sha256_hex() {
        // The empty string's SHA-256, which is the one value everybody can look
        // up — so this test is checkable without running it.
        assert_eq!(
            checksum(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(checksum(b"a"), checksum(b"b"));
    }
}
