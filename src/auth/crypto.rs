//! Secret handling utilities. ONE deliberate split to understand:
//!
//! - ARGON2ID for LOW-ENTROPY secrets (passwords, OTP codes): the
//!   attacker's offline search space is small (human passwords, 10^6
//!   OTP codes), so the defense is making each guess expensive.
//!
//! - SHA-256 for HIGH-ENTROPY tokens (256-bit session ids, API key
//!   secrets we generate ourselves): the search space is 2^256 - a slow
//!   KDF adds nothing an attacker could ever overcome anyway, and these
//!   are verified on EVERY REQUEST, where Argon2's ~50-100ms would be a
//!   self-inflicted DoS. Fast hash at rest is exactly right here.
//!
//! Cargo: argon2 = "0.5", sha2, rand, hex, subtle = "2"

use argon2::Argon2;
use argon2::password_hash::rand_core::RngCore;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

// =======================================================================
// Argon2id - passwords & OTP codes
// =======================================================================

/// Hash a low-entropy secret (password, OTP code). Default Argon2id
/// parameters (m=19MiB, t=2, p=1 per the 0.5 crate) are the OWASP
/// baseline; tune upward per deployment hardware if login latency
/// budget allows.
pub fn argon2id_hash(secret: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?;
    Ok(hash.to_string())
}

/// Verify against a PHC-format hash string. Wrong secret and malformed
/// hash both return false - callers never branch differently on "user
/// exists but wrong password" vs "no such hash" (username enumeration).
pub fn argon2id_verify(secret: &str, phc_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_ok()
}

// =======================================================================
// SHA-256 - session ids, API key secrets
// =======================================================================

pub fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// Constant-time equality for hex digests / CSRF tokens. Ordinary `==`
/// on strings short-circuits at the first differing byte, leaking
/// position via timing; token comparisons must not.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

// =======================================================================
// Generation
// =======================================================================

/// 256-bit random token, hex-encoded (session ids, API key secrets,
/// CSRF tokens, refresh tokens). OS CSPRNG, always.
pub fn generate_token() -> String {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    hex::encode(raw)
}

/// N-digit OTP (4..=9), uniform via rejection sampling - a bare
/// `random % 10^n` biases low codes. NOTE: digit count is a security
/// parameter, not cosmetics - shorter codes MUST ship with tighter
/// attempt caps / TTL (see OtpPolicy), per NIST 800-63B's throttling
/// requirement for low-entropy secrets.
pub fn generate_otp(digits: u32) -> String {
    assert!(
        (4..=9).contains(&digits),
        "OTP digits out of supported range"
    );
    let space = 10u32.pow(digits);
    // Largest multiple of `space` that fits in u32: rejecting samples at
    // or above it makes the modulo uniform.
    let limit = u32::MAX - (u32::MAX % space);
    loop {
        let mut raw = [0u8; 4];
        OsRng.fill_bytes(&mut raw);
        let n = u32::from_le_bytes(raw);
        if n < limit {
            return format!("{:0width$}", n % space, width = digits as usize);
        }
    }
}

/// API key wire format: "{prefix}.{secret}". Prefix is the public
/// aggregate id (safe to log, index, revoke by); secret is shown ONCE
/// at issuance and stored only as SHA-256.
pub struct GeneratedApiKey {
    pub prefix: String,
    pub secret: String,
    pub secret_sha256: String,
    /// What the caller presents: "{prefix}.{secret}".
    pub presentable: String,
}

pub fn generate_api_key(env_tag: &str) -> GeneratedApiKey {
    let mut short = [0u8; 6];
    OsRng.fill_bytes(&mut short);
    let prefix = format!("ak_{env_tag}_{}", hex::encode(short));
    let secret = generate_token();
    let secret_sha256 = sha256_hex(&secret);
    let presentable = format!("{prefix}.{secret}");
    GeneratedApiKey {
        prefix,
        secret,
        secret_sha256,
        presentable,
    }
}
