//! OTP challenge storage behind a trait, mirroring SessionStore: login
//! flows depend on `Arc<dyn OtpStore>`, so the backing can move
//! (Postgres UNLOGGED today; Redis with native TTLs, or a dedicated
//! store, tomorrow) without touching a flow.
//!
//! The semantic contract EVERY implementor must honor - these are
//! security properties, not implementation details:
//! - at most ONE live challenge per authenticator; re-issue replaces
//! - codes stored hashed (Argon2id - 4-6 digits is low entropy)
//! - an attempt is burned BEFORE verification, so races still count
//! - success consumes the challenge (single use)
//! - expired / never-issued / consumed are indistinguishable (Invalid)
//! - re-request inside the resend window returns RecentlySent with the
//!   existing challenge untouched (UX dedup, never a new secret)

use super::audience::Audience;
use async_trait::async_trait;
use std::time::Duration;

/// Re-requests inside this window resend the SAME challenge instead of
/// minting a new one.
pub const OTP_RESEND_WINDOW: Duration = Duration::from_secs(30);

/// Per-audience OTP policy. The knobs move TOGETHER: fewer digits means
/// a smaller guessing space, so the attempt cap compensates.
///
///   client:   4 digits, 3 attempts, 3 min -> 3/10^4 = 0.03% per challenge
///   employee: 6 digits, 3 attempts, 5 min -> 3/10^6 = 0.0003% per challenge
#[derive(Debug, Clone, Copy)]
pub struct OtpPolicy {
    pub digits: u32,
    pub max_attempts: i32,
    pub ttl: Duration,
}

pub fn otp_policy_for(audience: Audience) -> OtpPolicy {
    match audience {
        Audience::Client => OtpPolicy {
            digits: 4,
            max_attempts: 3,
            ttl: Duration::from_secs(3 * 60),
        },
        Audience::Employee | Audience::Admin => OtpPolicy {
            digits: 6,
            max_attempts: 3,
            ttl: Duration::from_secs(5 * 60),
        },
    }
}

pub enum OtpRequestOutcome {
    /// Send `code` to the identifier via SMS/email. The code exists only
    /// in memory here and in the user's inbox - never logged, never
    /// stored plain.
    Send { code: String },
    /// Inside the resend window - tell the user to check their messages.
    RecentlySent,
}

#[derive(Debug, thiserror::Error)]
pub enum OtpError {
    #[error("no such login method")] // deliberately vague to callers
    Unusable,
    #[error("code invalid or expired")]
    Invalid,
    #[error("too many attempts - request a new code")]
    TooManyAttempts,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait OtpStore: Send + Sync {
    async fn request(
        &self,
        authenticator_id: &str,
        policy: OtpPolicy,
    ) -> Result<OtpRequestOutcome, OtpError>;
    async fn verify(
        &self,
        authenticator_id: &str,
        presented_code: &str,
        policy: OtpPolicy,
    ) -> Result<(), OtpError>;
}
