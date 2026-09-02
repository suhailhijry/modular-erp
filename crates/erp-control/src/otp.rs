//! Signing in with a phone number.
//!
//! # Why this exists in this market
//!
//! A phone number is the identity here and an email address often is not. A
//! login that insists on one excludes people who have a phone, a bank account
//! and a business, and no inbox they read.
//!
//! # Two limiters, because they fail differently
//!
//! **Requesting** a code is limited by a cooldown per number: the failure is
//! somebody using this system to send texts, which costs money and annoys the
//! person whose number it is. **Verifying** one is limited by attempts on the
//! code itself: the failure is guessing, and a million guesses against twenty
//! bits is minutes.
//!
//! One limiter would have to be the stricter of the two everywhere, which makes
//! the ordinary case worse in order to defend against the rarer one.
//!
//! # What the code is worth
//!
//! Six digits, five minutes, single use, and dead after a handful of wrong
//! attempts. The stored digest stops a casual read of the table and nothing
//! more — see `0013_one_time_codes.sql`, which says so rather than implying
//! otherwise with an expensive hash.

use erp_types::{IdentityId, Timestamp};
use sha2::Digest as _;

use crate::auth::{AuthError, Session, SessionToken};
use crate::model::Actor;
use crate::{AccessError, ControlPlane};

/// How long a code lives.
///
/// Five minutes: long enough to read a text on a phone that is charging in
/// another room, short enough that a code somebody dictated over the phone has
/// stopped working by the time they regret it.
pub const CODE_LIFETIME_SECONDS: i64 = 300;

/// How soon another code may be asked for.
///
/// Sixty seconds. Every one of these costs a message, and a "resend" button
/// that works instantly is a "resend" button somebody holds down.
pub const REQUEST_INTERVAL_SECONDS: i64 = 60;

/// How many wrong guesses a code survives.
///
/// Five. Twenty bits of entropy against unlimited guesses is minutes; against
/// five it is one in two hundred thousand, and the next code is a fresh five.
pub const MAX_ATTEMPTS: i32 = 5;

/// What the code is.
///
/// Six digits, because it is read off a screen and typed by a person. The
/// entropy that matters is not in the code — see the module docs.
const DIGITS: u32 = 6;

/// Why a code was not sent, or not accepted.
#[derive(Debug, thiserror::Error)]
pub enum OtpError {
    #[error("{0} is not a phone number")]
    NotANumber(String),
    /// A code went to this number less than [`REQUEST_INTERVAL_SECONDS`] ago.
    #[error("a code was sent to this number {sent} seconds ago; retry in {retry_in}")]
    TooSoon { sent: i64, retry_in: i64 },
    /// **One answer for every way a verification fails.**
    ///
    /// Wrong code, expired code, spent code, no code at all, and too many
    /// attempts are one message. Telling somebody which they got says whether
    /// the number is known and whether a code is outstanding, which is two
    /// things they should have to already know.
    #[error("that code is not valid")]
    NotValid,
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl erp_i18n::Localize for OtpError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NotANumber(raw) => {
                Message::new(messages::NOT_A_PHONE_NUMBER).with("number", MessageArg::text(raw))
            }
            Self::TooSoon { retry_in, .. } => {
                Message::new(messages::CODE_TOO_SOON).with("seconds", MessageArg::Count(*retry_in))
            }
            Self::NotValid => Message::new(messages::CODE_NOT_VALID),
            Self::Access(e) => e.message(),
            Self::Auth(e) => e.message(),
        }
    }
}

/// A code that was minted, and where it goes.
///
/// The code itself is here because the caller has to put it in a message. It is
/// never returned over HTTP — `crates/erp-api/tests/http.rs` asserts that — and
/// there is no `Debug` that prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Requested {
    pub handle: String,
    code: String,
    pub expires_at: Timestamp,
}

impl Requested {
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

impl std::fmt::Debug for Requested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Requested")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// The authenticator kind a phone number signs in under.
const PHONE: &str = "phone";

impl ControlPlane {
    /// Mints a code for a number, or refuses because one went recently.
    ///
    /// **Nothing is created.** No identity, no membership, no account — a
    /// request is somebody typing a number, and creating an account on the
    /// strength of that would let anybody fill the table by typing numbers. The
    /// identity is made when a code is verified.
    ///
    /// The text is promised in the same transaction as the code (D9), so a code
    /// that was recorded and never sent cannot exist and neither can the
    /// reverse.
    pub async fn request_code(
        &self,
        raw: &str,
        locale: erp_i18n::Locale,
    ) -> Result<Requested, OtpError> {
        let handle = normalise(raw).ok_or_else(|| OtpError::NotANumber(raw.to_owned()))?;

        // **The cooldown, in the database.** Per number and fleet-wide, so a
        // second pod does not double the rate — the same shape signup's
        // confirmation resend uses.
        let live = sqlx::query!(
            r#"SELECT EXTRACT(EPOCH FROM (now() - created_at))::BIGINT as "age!"
                 FROM one_time_code
                WHERE handle = $1
                ORDER BY created_at DESC
                LIMIT 1"#,
            handle,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?;

        if let Some(row) = live
            && row.age < REQUEST_INTERVAL_SECONDS
        {
            return Err(OtpError::TooSoon {
                sent: row.age,
                retry_in: REQUEST_INTERVAL_SECONDS - row.age,
            });
        }

        let code = mint()?;
        let identity = self.identity_for(&handle).await?;

        let id = uuid::Uuid::now_v7();
        let mut tx = self.pool.begin().await.map_err(AccessError::Database)?;

        let expires_at = sqlx::query_scalar!(
            r#"INSERT INTO one_time_code
                   (id, handle, code_hash, identity_id, expires_at)
               VALUES ($1, $2, $3, $4, now() + ($5::BIGINT * INTERVAL '1 second'))
               RETURNING expires_at"#,
            id,
            handle,
            digest(&code),
            identity.as_ref().map(IdentityId::as_uuid),
            CODE_LIFETIME_SECONDS,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(AccessError::Database)?;

        // **In the same transaction as the row it is about** (D9), and the
        // reason the control plane has an outbox at all: sending inline would
        // either text somebody about a code that rolled back, or lose the send
        // to a crash with nothing recording it was owed.
        let text = crate::mail::Text::rendered(
            &crate::CATALOG,
            locale,
            handle.clone(),
            &crate::mail::code_message(&code),
        );
        erp_eventlog::enqueue(&mut tx, None, &[text.promised(format!("code:{id}"))])
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        tx.commit().await.map_err(AccessError::Database)?;

        Ok(Requested {
            handle,
            code,
            expires_at,
        })
    }

    /// Checks a code and starts a session.
    ///
    /// # Why this is one statement
    ///
    /// Marking the code used and deciding it was valid happen together, so two
    /// requests racing with the same code resolve to one. Checking first and
    /// updating second is the shape of that bug, and for a login it is the
    /// shape that matters most.
    pub async fn verify_code(
        &self,
        raw: &str,
        code: &str,
    ) -> Result<(SessionToken, Session), OtpError> {
        let handle = normalise(raw).ok_or_else(|| OtpError::NotANumber(raw.to_owned()))?;

        let claimed = sqlx::query!(
            r#"UPDATE one_time_code
                  SET used_at = now()
                WHERE id = (
                        SELECT id FROM one_time_code
                         WHERE handle = $1
                           AND used_at IS NULL
                           AND expires_at > now()
                           AND attempts < $3
                         ORDER BY created_at DESC
                         LIMIT 1
                      )
                  AND code_hash = $2
            RETURNING identity_id as "identity: IdentityId""#,
            handle,
            digest(code),
            MAX_ATTEMPTS,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)?;

        let Some(row) = claimed else {
            // A wrong guess against the live code, which is what the attempt
            // limit counts. **Only the live one**: incrementing every code for
            // the number would let one wrong guess kill a code that has not
            // been sent yet.
            let _ = sqlx::query!(
                "UPDATE one_time_code
                    SET attempts = attempts + 1
                  WHERE id = (
                          SELECT id FROM one_time_code
                           WHERE handle = $1 AND used_at IS NULL AND expires_at > now()
                           ORDER BY created_at DESC
                           LIMIT 1
                        )",
                handle,
            )
            .execute(&self.pool)
            .await;

            return Err(OtpError::NotValid);
        };

        // **The identity is created here**, on a code somebody proved they
        // received — not when they typed a number.
        let identity = match row.identity {
            Some(identity) => identity,
            None => self.register_phone(&handle).await?,
        };

        Ok(self.start_session(identity).await?)
    }

    /// The identity a number already signs in as, if any.
    async fn identity_for(&self, handle: &str) -> Result<Option<IdentityId>, AccessError> {
        sqlx::query_scalar!(
            r#"SELECT identity_id as "identity: IdentityId" FROM authenticator
                WHERE kind = $2 AND handle = $1"#,
            handle,
            PHONE,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AccessError::Database)
    }

    /// Creates one for a number nobody has signed in with.
    async fn register_phone(&self, handle: &str) -> Result<IdentityId, AccessError> {
        let identity = self.create_identity(Actor::system()).await?;

        sqlx::query!(
            // **`secret` is empty, and that is the honest record.** Possession
            // of the number is the credential; there is nothing to store, and a
            // placeholder hash would imply otherwise.
            "INSERT INTO authenticator (id, identity_id, kind, handle, secret)
             VALUES ($1, $2, $3, $4, '')
             ON CONFLICT (kind, handle) DO NOTHING",
            uuid::Uuid::now_v7(),
            identity.id.as_uuid(),
            PHONE,
            handle,
        )
        .execute(&self.pool)
        .await
        .map_err(AccessError::Database)?;

        // A race: two codes for a new number verified at once. Whichever lost
        // the insert reads the winner's identity rather than making a second.
        self.identity_for(handle)
            .await?
            .ok_or_else(|| AccessError::Corrupt("a phone authenticator vanished".to_owned()))
    }

    /// Removes codes that have expired.
    ///
    /// A row costs an index entry and nothing else, so this is unhurried; the
    /// reason it exists is that a table nobody deletes from grows for the life
    /// of the deployment. Returns how many went.
    pub async fn sweep_codes(&self) -> Result<u64, AccessError> {
        Ok(
            sqlx::query!("DELETE FROM one_time_code WHERE expires_at < now()")
                .execute(&self.pool)
                .await
                .map_err(AccessError::Database)?
                .rows_affected(),
        )
    }
}

/// A number, as E.164, or nothing.
///
/// Punctuation people type is stripped — spaces, dashes, brackets — and a
/// leading `00` becomes `+`, because that is how the rest of the world writes an
/// international prefix and refusing it is refusing a correct number.
#[must_use]
pub fn normalise(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.' | '\u{a0}'))
        .collect();

    let digits = cleaned
        .strip_prefix('+')
        .or_else(|| cleaned.strip_prefix("00"))?;

    if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') {
        // A country code never starts with zero, and `+0…` is somebody who
        // pasted a national number after a plus.
        return None;
    }
    Some(format!("+{digits}"))
}

/// Six digits, from the OS.
fn mint() -> Result<String, AuthError> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|e| AuthError::Hash(e.to_string()))?;

    // Rejection-free and close enough to uniform: the bias from a 32-bit draw
    // over a million is one part in four thousand, which is not a lever anybody
    // can pull on a code that dies in five minutes after five guesses.
    let value = u32::from_be_bytes(bytes) % 10_u32.pow(DIGITS);
    Ok(format!("{value:0width$}", width = DIGITS as usize))
}

fn digest(code: &str) -> Vec<u8> {
    sha2::Sha256::digest(code.trim().as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_normalised_the_way_people_write_one() {
        assert_eq!(
            normalise("+966 50 000 0000").as_deref(),
            Some("+966500000000")
        );
        assert_eq!(
            normalise("+966-50-000-0000").as_deref(),
            Some("+966500000000")
        );
        assert_eq!(
            normalise("00966500000000").as_deref(),
            Some("+966500000000")
        );
        assert_eq!(
            normalise(" +966500000000 ").as_deref(),
            Some("+966500000000")
        );
    }

    #[test]
    fn what_is_not_a_number_is_refused() {
        // A national number with no country code: the commonest mistake, and
        // accepting it would sign somebody into the wrong country's account.
        assert_eq!(normalise("0500000000"), None);
        assert_eq!(normalise("+0500000000"), None);
        assert_eq!(normalise("500000000"), None);
        assert_eq!(normalise("+966"), None);
        assert_eq!(normalise("+9665000000000000000"), None);
        assert_eq!(normalise("+966abc0000000"), None);
        assert_eq!(normalise(""), None);
    }

    #[test]
    fn a_code_is_six_digits() {
        for _ in 0..50 {
            let code = mint().expect("mints");
            assert_eq!(code.len(), 6, "{code}");
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }

    /// Two codes in a row are not the same, which is the least this can promise
    /// and the thing a broken generator gets wrong.
    #[test]
    fn codes_are_not_all_the_same() {
        let mut seen: Vec<String> = (0..20).filter_map(|_| mint().ok()).collect();
        seen.sort();
        seen.dedup();
        assert!(seen.len() > 15, "the generator repeats: {seen:?}");
    }

    #[test]
    fn a_requested_code_does_not_print_itself() {
        let requested = Requested {
            handle: "+966500000000".to_owned(),
            code: "123456".to_owned(),
            expires_at: chrono::Utc::now(),
        };
        let printed = format!("{requested:?}");
        assert!(printed.contains("+966500000000"));
        assert!(!printed.contains("123456"), "{printed}");
    }
}
