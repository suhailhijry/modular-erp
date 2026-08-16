//! The hash chain: every invoice points at the one before it.
//!
//! ZATCA's tamper-evidence. Each document carries the hash of the previous
//! document (the **PIH**) and a counter that never resets (the **ICV**), so
//! removing an invoice from the middle of a year breaks every hash after it.
//!
//! # Why this is reproducible
//!
//! Both are functions of the log and nothing else. The order is the log's
//! order, which is gapless and commit-ordered (L1); the counter is the position
//! in that order; the hash is over bytes this build renders deterministically.
//! Rebuild the projection and every document comes out byte-identical — which it
//! has to, because the hashes were submitted to a tax authority.
//!
//! That is the whole reason [`crate::taxpayer`] is an event: a chain built over
//! anything that can change out from under a replay is a chain that breaks on
//! the first rebuild, and breaks silently.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The base64 alphabet ZATCA uses, which is the standard one with padding.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// The hash of an invoice: SHA-256 of the canonical XML, base64.
///
/// # What is hashed
///
/// The document **without** its signature, its `UBLExtensions`, and its QR
/// reference — ZATCA strips those before hashing, and this build never puts them
/// in the bytes it hashes in the first place. Same result, no XSL transform, and
/// nothing to get subtly wrong.
///
/// # Canonicalisation
///
/// The bytes are rendered already canonical (C14N 1.1): UTF-8, no declaration,
/// no comments, no empty-element tags, `\n` line endings, attributes in order.
/// So hashing them directly and hashing their canonical form are the same
/// operation — see [`crate::zatca::ubl`], which is where that is kept true.
#[must_use]
pub fn invoice_hash(canonical_xml: &str) -> String {
    B64.encode(Sha256::digest(canonical_xml.as_bytes()))
}

/// What the first document in a chain points at.
///
/// # The odd one out
///
/// It is `base64(hex(sha256("0")))` — the base64 of the sixty-four *characters*
/// `5feceb66…`, not of the thirty-two bytes they spell. Every subsequent PIH is
/// `base64(sha256(bytes))`, forty-four characters. The two are encoded
/// differently and that is not a mistake here: it is what ZATCA's own
/// documentation specifies, and a chain that "fixes" it is rejected at the first
/// invoice.
///
/// Spelled out rather than pasted as a constant so the derivation is checkable —
/// `the_first_link_is_zatcas_odd_one_out` pins the literal value.
#[must_use]
pub fn genesis() -> String {
    B64.encode(hex::encode(Sha256::digest(b"0")))
}

/// Where a document sits in the chain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Link {
    /// The invoice counter value. Starts at 1 and never resets, including across
    /// years — it counts documents through this solution, not through a period.
    pub icv: i64,
    /// The previous document's hash, or [`genesis`] for the first.
    pub previous: String,
}

impl Link {
    /// The first link in a chain.
    #[must_use]
    pub fn first() -> Self {
        Self {
            icv: 1,
            previous: genesis(),
        }
    }

    /// The link after a document with this hash.
    #[must_use]
    pub fn after(previous_icv: i64, previous_hash: &str) -> Self {
        Self {
            icv: previous_icv + 1,
            previous: previous_hash.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value ZATCA documents for the first invoice. If this changes, every
    /// first invoice a tenant ever issues is rejected — so it is pinned to the
    /// literal rather than to its own derivation.
    #[test]
    fn the_first_link_is_zatcas_odd_one_out() {
        assert_eq!(
            genesis(),
            "NWZlY2ViNjZmZmM4NmYzOGQ5NTI3ODZjNmQ2OTZjNzljMmRiYzIzOWRkNGU5MWI0NjcyOWQ3M2EyN2ZiNTdlOQ=="
        );
        // 88 characters, because it encodes hex text. Every other link is 44,
        // because it encodes the digest. That asymmetry is the whole comment
        // above, asserted.
        assert_eq!(genesis().len(), 88);
        assert_eq!(invoice_hash("<Invoice></Invoice>").len(), 44);
    }

    #[test]
    fn the_hash_is_sha256_of_exactly_the_bytes_given() {
        // Checked against `printf '<a></a>' | sha256sum | xxd -r -p | base64`.
        assert_eq!(
            invoice_hash("<a></a>"),
            "qBKmm6aFilTO/bL8OILnzrfWaqHteSViCChy3W7U+SE="
        );
        // A single byte's difference is a different hash — which is the point of
        // the chain.
        assert_ne!(invoice_hash("<a></a>"), invoice_hash("<a></a> "));
    }

    #[test]
    fn a_chain_counts_up_and_points_back() {
        let first = Link::first();
        assert_eq!(first.icv, 1);
        assert_eq!(first.previous, genesis());

        let second = Link::after(first.icv, "aGFzaA==");
        assert_eq!(second.icv, 2);
        assert_eq!(second.previous, "aGFzaA==");
    }
}
