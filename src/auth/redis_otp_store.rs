//! Redis-backed OTP store. See otp_store.rs for the semantic contract.
//!
//! The two operations that MUST be atomic are Lua scripts, because
//! naive pipelines break the contract in subtle ways:
//!
//! - request: the resend-window check and the challenge write must be
//!   one step, or two concurrent requests both pass the check and both
//!   send codes (replacement still keeps "one live challenge", but the
//!   user gets two SMSes and one dead code - sloppy).
//!
//! - verify: `HINCRBY attempts` on a MISSING key would CREATE a ghost
//!   challenge (attempts=1, no code_hash) - so existence check +
//!   attempt burn + hash fetch must be one atomic script. This is the
//!   Redis translation of the Pg impl's UPDATE..RETURNING.
//!
//! Expiry is native TTL on the key - expired/consumed/never-issued are
//! indistinguishable (key absent) exactly as the contract requires.
//!
//! Key layout: otp:{authenticator_id} -> HASH {code_hash, attempts,
//! issued_at}, TTL = policy.ttl.

use super::crypto::{argon2id_hash, argon2id_verify, generate_otp};
use super::otp_store::{OTP_RESEND_WINDOW, OtpError, OtpPolicy, OtpRequestOutcome, OtpStore};
use async_trait::async_trait;
use redis::aio::ConnectionManager;

pub struct RedisOtpStore {
    conn: ConnectionManager,
    request_script: redis::Script,
    verify_script: redis::Script,
}

fn otp_key(authenticator_id: &str) -> String {
    format!("otp:{authenticator_id}")
}

impl RedisOtpStore {
    pub async fn connect(redis_url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            // ARGV: [now, resend_window_secs, code_hash, ttl_secs]
            // Returns 0 = inside resend window (challenge untouched),
            //         1 = new challenge written.
            request_script: redis::Script::new(
                r#"
                local issued = redis.call('HGET', KEYS[1], 'issued_at')
                if issued and (tonumber(ARGV[1]) - tonumber(issued)) < tonumber(ARGV[2]) then
                    return 0
                end
                redis.call('HSET', KEYS[1],
                    'code_hash', ARGV[3],
                    'attempts', 0,
                    'issued_at', ARGV[1])
                redis.call('EXPIRE', KEYS[1], ARGV[4])
                return 1
                "#,
            ),
            // Returns nil if no live challenge; otherwise
            // {code_hash, attempts_after_increment}. The increment
            // happens BEFORE the caller verifies - races burn attempts,
            // per the contract.
            verify_script: redis::Script::new(
                r#"
                if redis.call('EXISTS', KEYS[1]) == 0 then
                    return nil
                end
                local attempts = redis.call('HINCRBY', KEYS[1], 'attempts', 1)
                local hash = redis.call('HGET', KEYS[1], 'code_hash')
                return {hash, attempts}
                "#,
            ),
        })
    }
}

#[async_trait]
impl OtpStore for RedisOtpStore {
    async fn request(
        &self,
        authenticator_id: &str,
        policy: OtpPolicy,
    ) -> Result<OtpRequestOutcome, OtpError> {
        let now = chrono::Utc::now().timestamp();
        // Generate + hash BEFORE the script: if we lose the write race
        // to a concurrent request, this code is simply never stored and
        // never sent - no harm.
        let code = generate_otp(policy.digits);
        let code_hash = argon2id_hash(&code).map_err(OtpError::Other)?;

        let mut conn = self.conn.clone();
        let written: i64 = self
            .request_script
            .key(otp_key(authenticator_id))
            .arg(now)
            .arg(OTP_RESEND_WINDOW.as_secs())
            .arg(&code_hash)
            .arg(policy.ttl.as_secs())
            .invoke_async(&mut conn)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;

        if written == 0 {
            return Ok(OtpRequestOutcome::RecentlySent);
        }
        Ok(OtpRequestOutcome::Send { code })
    }

    async fn verify(
        &self,
        authenticator_id: &str,
        presented_code: &str,
        policy: OtpPolicy,
    ) -> Result<(), OtpError> {
        let mut conn = self.conn.clone();
        let result: Option<(String, i64)> = self
            .verify_script
            .key(otp_key(authenticator_id))
            .invoke_async(&mut conn)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;

        let Some((code_hash, attempts)) = result else {
            return Err(OtpError::Invalid); // absent = expired/consumed/never - indistinguishable
        };
        if attempts > policy.max_attempts as i64 {
            return Err(OtpError::TooManyAttempts); // key left to die by TTL
        }
        if !argon2id_verify(presented_code, &code_hash) {
            return Err(OtpError::Invalid);
        }
        // Single use.
        let _: () = redis::AsyncCommands::del(&mut conn, otp_key(authenticator_id))
            .await
            .map_err(|e| OtpError::Other(e.into()))?;
        Ok(())
    }
}
