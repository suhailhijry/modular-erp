//! Postgres-backed OTP store. See otp_store.rs for the semantic
//! contract every implementor must honor.

use super::crypto::{argon2id_hash, argon2id_verify, generate_otp};
use super::otp_store::{OTP_RESEND_WINDOW, OtpError, OtpPolicy, OtpRequestOutcome, OtpStore};
use async_trait::async_trait;

// =======================================================================
// Postgres implementation.
//
// CREATE UNLOGGED TABLE otp_challenges (
//     authenticator_id TEXT PRIMARY KEY,
//     code_hash TEXT NOT NULL,
//     attempts INT NOT NULL DEFAULT 0,
//     expires_at BIGINT NOT NULL,
//     issued_at BIGINT NOT NULL
// );
// =======================================================================

pub struct PgOtpStore {
    pub pool: sqlx::PgPool,
}

#[async_trait]
impl OtpStore for PgOtpStore {
    async fn request(
        &self,
        authenticator_id: &str,
        policy: OtpPolicy,
    ) -> Result<OtpRequestOutcome, OtpError> {
        let now = chrono::Utc::now().timestamp();

        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT issued_at FROM otp_challenges WHERE authenticator_id = $1 AND expires_at > $2",
        )
        .bind(authenticator_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OtpError::Other(e.into()))?;
        if let Some((issued_at,)) = existing {
            if now - issued_at < OTP_RESEND_WINDOW.as_secs() as i64 {
                return Ok(OtpRequestOutcome::RecentlySent);
            }
        }

        let code = generate_otp(policy.digits);
        let code_hash = argon2id_hash(&code).map_err(OtpError::Other)?;
        sqlx::query(
            "INSERT INTO otp_challenges (authenticator_id, code_hash, attempts, expires_at, issued_at)
             VALUES ($1, $2, 0, $3, $4)
             ON CONFLICT (authenticator_id)
             DO UPDATE SET code_hash = EXCLUDED.code_hash, attempts = 0,
                           expires_at = EXCLUDED.expires_at, issued_at = EXCLUDED.issued_at",
        )
        .bind(authenticator_id)
        .bind(&code_hash)
        .bind(now + policy.ttl.as_secs() as i64)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| OtpError::Other(e.into()))?;

        Ok(OtpRequestOutcome::Send { code })
    }

    async fn verify(
        &self,
        authenticator_id: &str,
        presented_code: &str,
        policy: OtpPolicy,
    ) -> Result<(), OtpError> {
        let now = chrono::Utc::now().timestamp();
        // Attempt burned FIRST, atomically with the fetch, so concurrent
        // verifies each consume an attempt.
        let row: Option<(String, i32)> = sqlx::query_as(
            "UPDATE otp_challenges SET attempts = attempts + 1
             WHERE authenticator_id = $1 AND expires_at > $2
             RETURNING code_hash, attempts",
        )
        .bind(authenticator_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| OtpError::Other(e.into()))?;

        let Some((code_hash, attempts)) = row else {
            return Err(OtpError::Invalid);
        };
        if attempts > policy.max_attempts {
            return Err(OtpError::TooManyAttempts);
        }
        if !argon2id_verify(presented_code, &code_hash) {
            return Err(OtpError::Invalid);
        }
        sqlx::query("DELETE FROM otp_challenges WHERE authenticator_id = $1")
            .bind(authenticator_id)
            .execute(&self.pool)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;
        Ok(())
    }
}
