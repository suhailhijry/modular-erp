//! A second factor, as an authenticator app computes it.
//!
//! # Why TOTP and not another text message
//!
//! `otp.rs` already sends a code to a phone, and that is the right primitive
//! for *signing in without a password* in a market where a phone number is the
//! identity. It is the wrong primitive for a **second** factor: a text costs
//! money on every login, arrives late or not at all on a foreign SIM, and is
//! defeated by the SIM swap that this market has more of than most. An
//! authenticator app costs nothing per login, works with no signal, and the
//! secret never travels after enrolment.
//!
//! # Why SHA-1, deliberately
//!
//! RFC 6238 permits SHA-256 and SHA-512, and every authenticator worth naming —
//! Google Authenticator, 1Password, Authy — computes SHA-1 and ignores the
//! `algorithm` parameter in the provisioning URI. A build that chose SHA-256
//! would be more modern and would produce codes nobody's phone agrees with.
//! HMAC-SHA1 is not broken for this: the attack on SHA-1 is collisions, and
//! HMAC does not depend on collision resistance.
//!
//! # Why the arithmetic is here rather than in a dependency
//!
//! It is thirty lines and it is specified to the byte, with published test
//! vectors this module runs. §24 made the same call for webhook HMAC. A
//! dependency for this is a supply chain for thirty lines.

use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};

/// How long one code is good for. Thirty seconds is what every authenticator
/// app assumes and is not configurable for that reason.
pub const STEP: u64 = 30;

/// Digits in a code. Six is what apps show.
pub const DIGITS: u32 = 6;

/// **How far out of step a clock may be and still be believed.**
///
/// One step either side — thirty seconds. A phone whose clock is a minute out
/// is a support call rather than a wider window: every extra step multiplies
/// the codes an attacker may guess, and the guessing limiter is what actually
/// defends this.
pub const DRIFT: i64 = 1;

/// Bytes in a secret. Twenty is what RFC 4226 recommends and what apps expect.
const SECRET: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum TotpError {
    #[error("a secret must be {SECRET} bytes")]
    SecretLength,
    #[error("that is not base32")]
    NotBase32,
    #[error("could not compute a code: {0}")]
    Crypto(String),
}

/// A fresh shared secret.
///
/// # Errors
/// If the system random source fails.
pub fn generate() -> Result<Vec<u8>, TotpError> {
    let mut bytes = [0u8; SECRET];
    getrandom::fill(&mut bytes).map_err(|e| TotpError::Crypto(e.to_string()))?;
    Ok(bytes.to_vec())
}

/// The code for one instant.
///
/// # Errors
/// If HMAC fails, which means the crypto provider is broken rather than the
/// input being wrong.
pub fn code_at(secret: &[u8], unix_seconds: u64, digits: u32) -> Result<String, TotpError> {
    let counter = unix_seconds / STEP;
    let key = PKey::hmac(secret).map_err(|e| TotpError::Crypto(e.to_string()))?;
    let mut signer =
        Signer::new(MessageDigest::sha1(), &key).map_err(|e| TotpError::Crypto(e.to_string()))?;
    signer
        .update(&counter.to_be_bytes())
        .map_err(|e| TotpError::Crypto(e.to_string()))?;
    let mac = signer
        .sign_to_vec()
        .map_err(|e| TotpError::Crypto(e.to_string()))?;

    // RFC 4226 §5.4 dynamic truncation: the low nibble of the last byte picks
    // where to read four bytes from, and the top bit is masked off so the
    // result is positive in languages without unsigned integers.
    let offset = usize::from(mac[mac.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        mac[offset] & 0x7f,
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ]);
    let modulo = 10_u32.pow(digits);
    Ok(format!(
        "{:0width$}",
        binary % modulo,
        width = digits as usize
    ))
}

/// **Whether a code is one this secret produced**, allowing for a clock that
/// drifts.
///
/// Compared in constant time so a timing difference cannot narrow the search.
#[must_use]
pub fn verify(secret: &[u8], code: &str, unix_seconds: u64, drift: i64) -> bool {
    let code = code.trim();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let mut matched = false;
    for step in -drift..=drift {
        let Some(at) = unix_seconds.checked_add_signed(step.saturating_mul(STEP.cast_signed()))
        else {
            continue;
        };
        let Ok(expected) = code_at(secret, at, DIGITS) else {
            continue;
        };
        // **Every step is computed, and no loop exits early.** Returning on the
        // first match would leak which step matched through timing, which is a
        // clock oracle rather than a code oracle but is still free to close.
        matched |= constant_time_eq(expected.as_bytes(), code.as_bytes());
    }
    matched
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Base32 as RFC 4648 writes it, which is what a provisioning URI carries and
/// what a person types when a camera will not read the QR.
#[must_use]
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(5) {
        let mut buffer = [0u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([
            0, 0, 0, buffer[0], buffer[1], buffer[2], buffer[3], buffer[4],
        ]);
        // Five bytes are exactly eight base32 characters; a short chunk pads
        // with '=' so the length always divides by eight.
        let produced = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for i in 0..8 {
            if i < produced {
                let index = (bits >> (35 - i * 5)) & 0b1_1111;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Reads back what [`base32`] wrote.
///
/// # Errors
/// If a character is not in the alphabet. Case is ignored and padding is
/// optional, because a person retyping a secret from a screen will do both.
pub fn unbase32(text: &str) -> Result<Vec<u8>, TotpError> {
    let mut bits = 0u64;
    let mut held = 0u32;
    let mut out = Vec::new();
    for c in text.chars().filter(|c| *c != '=' && !c.is_whitespace()) {
        let upper = c.to_ascii_uppercase();
        let index = ALPHABET
            .iter()
            .position(|a| char::from(*a) == upper)
            .ok_or(TotpError::NotBase32)?;
        bits = (bits << 5) | index as u64;
        held += 5;
        if held >= 8 {
            held -= 8;
            out.push(((bits >> held) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// **What the QR encodes.** `otpauth://` as Google's key-uri-format defines it.
///
/// The issuer appears twice — as a path prefix and as a parameter — because
/// apps disagree about which they read, and one that reads neither shows the
/// account with no idea which business it belongs to.
#[must_use]
pub fn provisioning_uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    let issuer_escaped = percent_encode(issuer);
    format!(
        "otpauth://totp/{issuer_escaped}:{}?secret={}&issuer={issuer_escaped}&algorithm=SHA1&digits={DIGITS}&period={STEP}",
        percent_encode(account),
        base32(secret),
    )
}

/// Enough of percent-encoding for a label: everything outside the unreserved
/// set goes as `%XX`. Not a general encoder, and not used as one.
fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **RFC 4648 §10.** Hand-rolled encoders fail on the padding, which is
    /// exactly what these cover.
    #[test]
    fn base32_matches_the_published_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "MY======"),
            ("fo", "MZXQ===="),
            ("foo", "MZXW6==="),
            ("foob", "MZXW6YQ="),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI======"),
        ] {
            assert_eq!(base32(input.as_bytes()), expected, "encoding {input:?}");
            assert_eq!(
                unbase32(expected).unwrap(),
                input.as_bytes(),
                "decoding {expected:?}"
            );
        }
    }

    #[test]
    fn base32_round_trips_a_real_secret() {
        let secret = generate().unwrap();
        assert_eq!(unbase32(&base32(&secret)).unwrap(), secret);
    }

    #[test]
    fn a_retyped_secret_is_read_in_any_case_and_without_padding() {
        assert_eq!(unbase32("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(unbase32("MZXW 6YTB OI").unwrap(), b"foobar");
    }

    /// **RFC 6238 Appendix B**, the SHA-1 rows. If this build ever disagrees
    /// with these, every authenticator app in the world disagrees with it.
    #[test]
    fn totp_matches_the_published_vectors() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59_u64, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
            (20_000_000_000, "65353130"),
        ] {
            assert_eq!(code_at(secret, time, 8).unwrap(), expected, "at T={time}");
        }
    }

    #[test]
    fn a_code_is_accepted_one_step_either_side_and_not_two() {
        let secret = generate().unwrap();
        let now = 1_700_000_000_u64;
        let code = code_at(&secret, now, DIGITS).unwrap();

        assert!(verify(&secret, &code, now, DRIFT));
        assert!(verify(&secret, &code, now + STEP, DRIFT), "a slow clock");
        assert!(verify(&secret, &code, now - STEP, DRIFT), "a fast clock");
        assert!(
            !verify(&secret, &code, now + STEP * 2, DRIFT),
            "a minute out is a support call, not a wider window"
        );
    }

    #[test]
    fn nothing_that_is_not_six_digits_is_a_code() {
        let secret = generate().unwrap();
        let now = 1_700_000_000_u64;
        for bad in ["", "12345", "1234567", "abcdef", "12 34 56", "١٢٣٤٥٦"] {
            assert!(!verify(&secret, bad, now, DRIFT), "{bad:?} is not a code");
        }
    }

    #[test]
    fn another_secret_does_not_produce_the_same_code() {
        let now = 1_700_000_000_u64;
        let mine = generate().unwrap();
        let theirs = generate().unwrap();
        let code = code_at(&mine, now, DIGITS).unwrap();
        assert!(!verify(&theirs, &code, now, DRIFT));
    }

    #[test]
    fn a_provisioning_uri_names_the_business_twice_and_carries_the_secret() {
        let secret = b"12345678901234567890";
        let uri = provisioning_uri("Bassat Salon", "sara@bassat.test", secret);
        assert!(uri.starts_with("otpauth://totp/Bassat%20Salon:sara%40bassat.test?"));
        assert!(uri.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"));
        assert!(uri.contains("issuer=Bassat%20Salon"));
        assert!(uri.contains("algorithm=SHA1"), "apps ignore anything else");
        assert!(uri.contains("digits=6") && uri.contains("period=30"));
    }
}
