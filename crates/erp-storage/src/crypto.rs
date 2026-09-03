//! SHA-256 and HMAC-SHA256, for `SigV4`, from the OpenSSL this build already has.
//!
//! # Why this file exists at all
//!
//! `object_store` needs a [`CryptoProvider`] to sign requests, and it ships two
//! bundled ones — `aws-lc-rs` and `ring`. Taking either would put a second
//! cryptography library in a process that already links OpenSSL for Postgres
//! TLS, SMTP, Redis, key generation, the ZATCA CSR and sealing secrets at rest,
//! and would drag `rustls` in behind it.
//!
//! So the provider is supplied instead. It is two primitives, both of which
//! OpenSSL has: a streaming SHA-256 and an HMAC over it. There is no
//! hand-rolled cryptography here — every byte is computed by the same library
//! that computes every other byte in this system.
//!
//! # What is deliberately not implemented
//!
//! [`CryptoProvider::sign`], which is RS256 over a PEM key. It exists for
//! Google Cloud Storage service accounts and nothing on the S3 path calls it,
//! so it refuses rather than being written blind and untested.

use object_store::client::{
    CryptoProvider, DigestAlgorithm, DigestContext, HmacContext, Signer, SigningAlgorithm,
};
use openssl::error::ErrorStack;
use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::{PKey, Private};

/// The cryptography `object_store` signs with.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Openssl;

impl CryptoProvider for Openssl {
    fn digest(&self, algorithm: DigestAlgorithm) -> object_store::Result<Box<dyn DigestContext>> {
        match algorithm {
            DigestAlgorithm::Sha256 => Ok(Box::new(Sha256::new().map_err(failed)?)),
            other => Err(unsupported(&format!("{other:?} digests"))),
        }
    }

    fn hmac(
        &self,
        algorithm: DigestAlgorithm,
        secret: &[u8],
    ) -> object_store::Result<Box<dyn HmacContext>> {
        match algorithm {
            DigestAlgorithm::Sha256 => Ok(Box::new(HmacSha256 {
                key: PKey::hmac(secret).map_err(failed)?,
                message: Vec::new(),
                out: Vec::new(),
            })),
            other => Err(unsupported(&format!("{other:?} HMACs"))),
        }
    }

    fn sign(
        &self,
        _algorithm: SigningAlgorithm,
        _pem: &[u8],
    ) -> object_store::Result<Box<dyn Signer>> {
        // Google Cloud Storage's service accounts. Nothing on the S3 path
        // reaches this, and an untested signature is worse than a refusal.
        Err(unsupported("PEM signing"))
    }
}

/// A streaming SHA-256.
///
/// Streaming rather than buffering because this one runs over the **payload**:
/// a twenty-five megabyte upload hashed by copying it into a second buffer
/// first is fifty megabytes for no reason.
struct Sha256 {
    hasher: Hasher,
    /// The first error `update` hit, reported by `finish`.
    ///
    /// `DigestContext::update` cannot fail in its signature and OpenSSL's can,
    /// so the failure is carried instead of dropped. A digest that silently
    /// omitted part of its input would come back as a `403 SignatureDoesNotMatch`
    /// with nothing to say why.
    failed: Option<ErrorStack>,
    out: Vec<u8>,
}

impl Sha256 {
    fn new() -> Result<Self, ErrorStack> {
        Ok(Self {
            hasher: Hasher::new(MessageDigest::sha256())?,
            failed: None,
            out: Vec::new(),
        })
    }
}

impl DigestContext for Sha256 {
    fn update(&mut self, data: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        if let Err(e) = self.hasher.update(data) {
            self.failed = Some(e);
        }
    }

    fn finish(&mut self) -> object_store::Result<&[u8]> {
        if let Some(e) = self.failed.take() {
            return Err(failed(e));
        }
        self.out = self.hasher.finish().map_err(failed)?.to_vec();
        Ok(&self.out)
    }
}

/// An HMAC-SHA256.
///
/// Buffered, unlike the digest above, because `openssl::sign::Signer` borrows
/// its key and the two cannot live in one struct. That costs nothing here:
/// `SigV4` HMACs the string-to-sign and the four short strings of the key
/// derivation chain, never the payload — the payload goes through [`Sha256`].
struct HmacSha256 {
    key: PKey<Private>,
    message: Vec<u8>,
    out: Vec<u8>,
}

impl HmacContext for HmacSha256 {
    fn update(&mut self, data: &[u8]) {
        self.message.extend_from_slice(data);
    }

    fn finish(&mut self) -> object_store::Result<&[u8]> {
        let mut signer =
            openssl::sign::Signer::new(MessageDigest::sha256(), &self.key).map_err(failed)?;
        signer.update(&self.message).map_err(failed)?;
        self.out = signer.sign_to_vec().map_err(failed)?;
        Ok(&self.out)
    }
}

fn failed(error: ErrorStack) -> object_store::Error {
    object_store::Error::Generic {
        store: "S3",
        source: Box::new(error),
    }
}

fn unsupported(what: &str) -> object_store::Error {
    object_store::Error::NotSupported {
        source: format!("{what} are not implemented by this CryptoProvider").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(chunks: &[&[u8]]) -> String {
        let mut context = Openssl
            .digest(DigestAlgorithm::Sha256)
            .expect("sha-256 is supported");
        for chunk in chunks {
            context.update(chunk);
        }
        hex::encode(context.finish().expect("digests"))
    }

    fn hmac(secret: &[u8], message: &[u8]) -> String {
        let mut context = Openssl
            .hmac(DigestAlgorithm::Sha256, secret)
            .expect("hmac-sha-256 is supported");
        context.update(message);
        hex::encode(context.finish().expect("signs"))
    }

    /// The empty string's SHA-256, which anybody can look up — so this is
    /// checkable without running it.
    #[test]
    fn the_digest_is_sha256() {
        assert_eq!(
            digest(&[b""]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(&[b"abc"]),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// **The one that matters for a signature.** `SigV4` feeds the canonical
    /// request in whatever chunks the caller had; a digest that depended on how
    /// the bytes arrived would sign correctly in a test and fail in production.
    #[test]
    fn the_digest_does_not_care_how_the_bytes_arrive() {
        assert_eq!(digest(&[b"abc"]), digest(&[b"a", b"b", b"c"]));
        assert_eq!(digest(&[b"abc"]), digest(&[b"", b"ab", b"", b"c"]));
    }

    /// RFC 4231, cases 1, 2 and 4 — HMAC-SHA-256 test vectors, published,
    /// checkable against the RFC without trusting this code.
    #[test]
    fn the_hmac_matches_the_published_vectors() {
        assert_eq!(
            hmac(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let counting: Vec<u8> = (0x01..=0x19).collect();
        assert_eq!(
            hmac(&counting, &[0xcd; 50]),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
    }

    /// RFC 4231 case 6: a key longer than the 64-byte block, which the
    /// specification says is hashed first. It is the case a hand-rolled HMAC
    /// gets wrong, and the reason this one is not hand-rolled.
    #[test]
    fn a_key_longer_than_the_block_is_hashed_first() {
        assert_eq!(
            hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_hmac_does_not_care_how_the_bytes_arrive() {
        let mut split = Openssl
            .hmac(DigestAlgorithm::Sha256, b"Jefe")
            .expect("hmac-sha-256 is supported");
        split.update(b"what do ya want ");
        split.update(b"for nothing?");
        assert_eq!(
            hex::encode(split.finish().expect("signs")),
            hmac(b"Jefe", b"what do ya want for nothing?")
        );
    }

    /// Not written blind: nothing on the S3 path calls it, so it refuses.
    #[test]
    fn pem_signing_refuses_rather_than_guessing() {
        assert!(matches!(
            Openssl.sign(SigningAlgorithm::RS256, b"-----BEGIN PRIVATE KEY-----"),
            Err(object_store::Error::NotSupported { .. })
        ));
    }
}
