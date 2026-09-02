//! Letting somebody else's system in.
//!
//! # The first inbound surface, and what that changes
//!
//! Every integration so far has been this system talking. A provider talks
//! back, and **a callback that is trusted without being verified is somebody
//! else's command executed under your authority** — anybody who can reach the
//! URL can say a payment succeeded.
//!
//! So verification is not a slow path, it is the *only* path: the signature is
//! checked before the body is treated as meaning anything.
//!
//! # What a webhook is, mechanically
//!
//! A **command with the provider's id as its idempotency key**. It will be
//! delivered more than once — every provider retries, and most say so — it will
//! arrive out of order, and it can be replayed by anybody who kept a copy. All
//! three are handled by the same two decisions: verify the signature, and
//! record the id.
//!
//! # Why HMAC is written here rather than pulled in
//!
//! It is fifteen lines of a 1997 specification and it is verified against RFC
//! 4231's published vectors below, including the one nobody gets right — a key
//! longer than the block size, which is hashed first. A dependency for this
//! would be a dependency whose correctness is checked exactly as much.

use sha2::Digest as _;

/// SHA-256's block size, which is what the key is padded to.
const BLOCK: usize = 64;

/// How far out a timestamp may be.
///
/// Five minutes each way. **This is what stops a replay**: a signature is valid
/// for ever, so the timestamp is what makes a copy somebody kept useless — and
/// it has to be inside the signature or an attacker simply changes it.
///
/// Both directions, because a provider's clock is not ours and one that is a
/// minute ahead is a provider whose every callback would be refused.
pub const TOLERANCE_SECONDS: i64 = 300;

/// Why a callback was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("no signature")]
    Unsigned,
    #[error("the signature does not match")]
    BadSignature,
    #[error("the timestamp is not one")]
    BadTimestamp,
    #[error("the timestamp is {0} seconds out")]
    TooOld(i64),
    #[error("this deployment has no secret for that provider")]
    NoSecret,
}

impl erp_i18n::Localize for WebhookError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages as m;
        match self {
            // **One message for three of them, deliberately.** "No signature",
            // "wrong signature" and "expired" are the same answer to somebody
            // who should not be here, and telling them which they got is an
            // oracle for guessing the rest.
            Self::Unsigned | Self::BadSignature | Self::BadTimestamp | Self::TooOld(_) => {
                erp_i18n::Message::new(m::WEBHOOK_NOT_VERIFIED)
            }
            Self::NoSecret => erp_i18n::Message::new(m::WEBHOOK_NO_SECRET),
        }
    }
}

/// HMAC-SHA256, hex.
#[must_use]
pub fn sign(secret: &[u8], message: &[u8]) -> String {
    hex(&mac(secret, message))
}

/// **Verifies a callback before its body means anything.**
///
/// The signed message is `<timestamp>.<body>`, so the timestamp cannot be
/// changed by whoever kept a copy — which is what makes [`TOLERANCE_SECONDS`] a
/// replay window rather than a suggestion.
pub fn verify(
    secret: &[u8],
    timestamp: &str,
    body: &[u8],
    signature: &str,
    now: i64,
) -> Result<(), WebhookError> {
    if signature.trim().is_empty() {
        return Err(WebhookError::Unsigned);
    }
    let sent: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| WebhookError::BadTimestamp)?;

    let drift = now - sent;
    if drift.abs() > TOLERANCE_SECONDS {
        return Err(WebhookError::TooOld(drift));
    }

    let mut message = Vec::with_capacity(timestamp.len() + 1 + body.len());
    message.extend_from_slice(timestamp.trim().as_bytes());
    message.push(b'.');
    message.extend_from_slice(body);

    let expected = sign(secret, &message);
    // Constant time, because a comparison that stops at the first wrong byte is
    // a comparison an attacker can walk one byte at a time.
    if constant_time_eq(expected.as_bytes(), signature.trim().as_bytes()) {
        Ok(())
    } else {
        Err(WebhookError::BadSignature)
    }
}

/// RFC 2104, over SHA-256.
fn mac(secret: &[u8], message: &[u8]) -> [u8; 32] {
    // **A key longer than the block is hashed first.** This is the line every
    // hand-written HMAC gets wrong, and RFC 4231's case 6 below is what proves
    // it is right here.
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&sha2::Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }

    let mut inner = sha2::Sha256::new();
    inner.update(key.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = sha2::Sha256::new();
    outer.update(key.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = u8::from(a.len() != b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **RFC 4231's published vectors.**
    ///
    /// The reason this is written here rather than pulled in: it is checkable
    /// against numbers anybody can look up, which is more than most
    /// dependencies offer.
    #[test]
    fn hmac_matches_the_published_vectors() {
        // Case 1.
        assert_eq!(
            sign(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Case 2 — a short, printable key.
        assert_eq!(
            sign(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 3.
        assert_eq!(
            sign(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // **Case 6 — a key longer than the block.** The one people get wrong.
        assert_eq!(
            sign(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn signed(secret: &[u8], timestamp: &str, body: &[u8]) -> String {
        let mut message = timestamp.as_bytes().to_vec();
        message.push(b'.');
        message.extend_from_slice(body);
        sign(secret, &message)
    }

    #[test]
    fn a_correctly_signed_callback_is_accepted() {
        let body = br#"{"id":"evt_1","type":"payment.succeeded"}"#;
        let signature = signed(b"whsec", "1800000000", body);
        assert_eq!(
            verify(b"whsec", "1800000000", body, &signature, 1_800_000_000),
            Ok(())
        );
    }

    /// **The body cannot be changed without the signature changing.** This is
    /// the whole point.
    #[test]
    fn a_body_that_moved_by_one_byte_is_refused() {
        let body = br#"{"amount":100}"#;
        let signature = signed(b"whsec", "1800000000", body);
        assert_eq!(
            verify(
                b"whsec",
                "1800000000",
                br#"{"amount":900}"#,
                &signature,
                1_800_000_000
            ),
            Err(WebhookError::BadSignature)
        );
    }

    /// **And neither can the timestamp**, which is what makes the window real.
    #[test]
    fn a_replay_with_a_fresh_timestamp_is_refused() {
        let body = br#"{"id":"evt_1"}"#;
        let signature = signed(b"whsec", "1800000000", body);

        // Somebody kept a copy and re-sent it an hour later, unchanged.
        assert!(matches!(
            verify(b"whsec", "1800000000", body, &signature, 1_800_003_600),
            Err(WebhookError::TooOld(_))
        ));

        // …and changing the timestamp to something current does not help,
        // because the timestamp is inside the signature.
        assert_eq!(
            verify(b"whsec", "1800003600", body, &signature, 1_800_003_600),
            Err(WebhookError::BadSignature)
        );
    }

    /// A provider whose clock is a minute ahead is not an attacker.
    #[test]
    fn a_clock_that_is_slightly_ahead_is_accepted() {
        let body = b"{}";
        let signature = signed(b"whsec", "1800000060", body);
        assert_eq!(
            verify(b"whsec", "1800000060", body, &signature, 1_800_000_000),
            Ok(())
        );
    }

    #[test]
    fn the_wrong_secret_is_refused() {
        let body = b"{}";
        let signature = signed(b"whsec", "1800000000", body);
        assert_eq!(
            verify(b"other", "1800000000", body, &signature, 1_800_000_000),
            Err(WebhookError::BadSignature)
        );
    }

    #[test]
    fn nothing_at_all_is_refused() {
        assert_eq!(
            verify(b"whsec", "1800000000", b"{}", "", 1_800_000_000),
            Err(WebhookError::Unsigned)
        );
        assert_eq!(
            verify(b"whsec", "not a time", b"{}", "abcd", 1_800_000_000),
            Err(WebhookError::BadTimestamp)
        );
    }

    /// Every refusal a stranger can provoke renders as one message. Telling
    /// them which they got is an oracle for guessing the rest.
    #[test]
    fn a_stranger_learns_nothing_from_which_refusal_they_got() {
        use erp_i18n::Localize as _;
        let same = [
            WebhookError::Unsigned,
            WebhookError::BadSignature,
            WebhookError::BadTimestamp,
            WebhookError::TooOld(9_999),
        ];
        for refusal in &same {
            assert_eq!(refusal.message().code, same[0].message().code);
        }
        assert_ne!(
            WebhookError::NoSecret.message().code,
            same[0].message().code,
            "a deployment with no secret is our problem, not theirs"
        );
    }
}
