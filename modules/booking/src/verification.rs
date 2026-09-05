//! Proving a phone number belongs to whoever is booking with it.
//!
//! # What this is for, and what it is not
//!
//! It is for a business that wants to know a booking came from a real number
//! they can ring. It is **not** a login: nothing here mints a session or an
//! identity, and a code issued here cannot sign anybody into anything. See
//! `migrations/tenant/0012_booking_verification.sql` for why that separation is
//! the point rather than an omission.
//!
//! # It is not the anti-abuse control either
//!
//! The deposit is. A slot that cannot be held without paying for it cannot be
//! spammed, whoever is asking. What a verified number buys is the ability to
//! **reach** somebody — to send the reminder, to ring when the stylist is ill —
//! which is a real need and a different one. That is why it is a setting and
//! why the default is off: a business taking deposits already has what it needs.
//!
//! # Two limiters, because they fail differently
//!
//! **Asking** for a code is limited by a cooldown per number: the failure is
//! somebody using a booking form to send texts, which costs the business money
//! and annoys whoever owns the number. **Claiming** one is limited by attempts
//! on the code itself: the failure is guessing. The same split
//! `erp_control::otp` makes, for the same reason.

use erp_types::Timestamp;
use sha2::Digest as _;

/// How long a code lives. Long enough to read a text on a phone in another
/// room; short enough that one dictated over the phone has stopped working.
pub const CODE_LIFETIME_SECONDS: i64 = 300;

/// How soon another may be asked for. A resend button that works instantly is
/// one somebody holds down.
pub const REQUEST_INTERVAL_SECONDS: i64 = 60;

/// How many wrong guesses a code survives.
pub const MAX_ATTEMPTS: i32 = 5;

/// Why a code was not issued, or not accepted.
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("{0} is not a phone number")]
    NotANumber(String),
    #[error("a code was sent recently; wait before asking for another")]
    TooSoon,
    /// **One answer for every way a code can fail.** Wrong, expired, used, or
    /// never issued — telling them apart tells somebody guessing which half of
    /// the pair they got right.
    #[error("that code is not valid")]
    NotValid,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A code, and the number it was issued for.
///
/// **The plaintext is returned once and never stored.** The caller sends it and
/// forgets it; what stays behind is a digest that cannot be turned back into
/// the six digits without guessing them, which is what the attempt limit is for.
#[derive(Debug, Clone)]
pub struct Issued {
    pub handle: String,
    pub code: String,
}

/// Issues a code for a number, in the caller's transaction.
///
/// **The transaction is the point.** Whoever calls this promises the text in the
/// same one (D9), so a code that was stored and never sent, or sent and never
/// stored, is not a state this can reach.
pub async fn issue(
    conn: &mut sqlx::PgConnection,
    raw: &str,
    now: Timestamp,
) -> Result<Issued, VerificationError> {
    let handle = normalise(raw).ok_or_else(|| VerificationError::NotANumber(raw.to_owned()))?;

    // **The cooldown, in the database.** Per number, so a second pod does not
    // double the rate — which is the whole reason it is not a counter in memory.
    let recent = sqlx::query_scalar!(
        "SELECT count(*) FROM booking_verification
          WHERE handle = $1 AND created_at > $2",
        handle,
        now - chrono::Duration::seconds(REQUEST_INTERVAL_SECONDS),
    )
    .fetch_one(&mut *conn)
    .await?;
    if recent.unwrap_or(0) > 0 {
        return Err(VerificationError::TooSoon);
    }

    let code = six_digits()?;
    sqlx::query!(
        "INSERT INTO booking_verification (id, handle, code_hash, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
        uuid::Uuid::now_v7(),
        handle,
        digest(&code).to_vec(),
        now,
        now + chrono::Duration::seconds(CODE_LIFETIME_SECONDS),
    )
    .execute(&mut *conn)
    .await?;

    Ok(Issued { handle, code })
}

/// Claims a code for a number, spending it.
///
/// **One statement**, so two bookings racing with the same code resolve to one:
/// the update that accepts it is the update that marks it used, and the second
/// finds nothing to accept.
///
/// A wrong guess costs the code an attempt whether or not it was the right
/// number, which is what stops a caller working through the digits.
pub async fn claim(
    conn: &mut sqlx::PgConnection,
    raw: &str,
    code: &str,
    now: Timestamp,
) -> Result<(), VerificationError> {
    let handle = normalise(raw).ok_or_else(|| VerificationError::NotANumber(raw.to_owned()))?;

    let claimed = sqlx::query_scalar!(
        "UPDATE booking_verification
            SET used_at = $4
          WHERE id = (
              SELECT id FROM booking_verification
               WHERE handle = $1
                 AND code_hash = $2
                 AND used_at IS NULL
                 AND expires_at > $4
                 AND attempts < $3
               ORDER BY created_at DESC
               LIMIT 1
               FOR UPDATE SKIP LOCKED
          )
      RETURNING id",
        handle,
        digest(code).to_vec(),
        MAX_ATTEMPTS,
        now,
    )
    .fetch_optional(&mut *conn)
    .await?;

    if claimed.is_some() {
        return Ok(());
    }

    // **Nothing matched, so every live code for this number pays for it.** A
    // caller who cannot be told which guess was close is a caller who has to
    // guess the whole thing.
    sqlx::query!(
        "UPDATE booking_verification
            SET attempts = attempts + 1
          WHERE handle = $1 AND used_at IS NULL AND expires_at > $2",
        handle,
        now,
    )
    .execute(&mut *conn)
    .await?;

    Err(VerificationError::NotValid)
}

/// Deletes codes that have expired. An expired code is evidence of nothing.
pub async fn sweep(conn: &mut sqlx::PgConnection, now: Timestamp) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "DELETE FROM booking_verification WHERE expires_at < $1",
        now
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// E.164, or nothing.
///
/// **Spaces, dashes and brackets are how people write numbers** and none of them
/// mean anything; a leading `00` is the international prefix spelled the old
/// way. Everything else is refused rather than repaired, because a number this
/// cannot read is one the message would not reach either.
#[must_use]
pub fn normalise(raw: &str) -> Option<String> {
    let stripped: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    let digits = stripped
        .strip_prefix('+')
        .or_else(|| stripped.strip_prefix("00"))?;
    if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') {
        return None;
    }
    Some(format!("+{digits}"))
}

fn digest(code: &str) -> [u8; 32] {
    sha2::Sha256::digest(code.as_bytes()).into()
}

/// Six digits, from the OS, because it is read off a screen and typed by a
/// person. The entropy that matters is not in the code — see the module docs.
///
/// Rejection-free and close enough to uniform: the bias from a 32-bit draw over
/// a million is one part in four thousand, which is not a lever anybody can pull
/// on a code that dies in five minutes after five guesses. The same call
/// `erp_control::otp` makes, for the same reason.
fn six_digits() -> Result<String, VerificationError> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|e| VerificationError::NotANumber(e.to_string()))?;
    Ok(format!("{:06}", u32::from_be_bytes(bytes) % 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_read_the_way_people_write_one() {
        assert_eq!(
            normalise("+966 50 000 0000").as_deref(),
            Some("+966500000000")
        );
        assert_eq!(
            normalise("00966500000000").as_deref(),
            Some("+966500000000")
        );
        assert_eq!(
            normalise("+966-50-000-0000").as_deref(),
            Some("+966500000000")
        );
    }

    /// **A national number is refused, not guessed at.** `0500000000` is a
    /// Saudi number to a Saudi reader and nothing at all to a message gateway,
    /// and inventing a country code sends somebody else's phone a code.
    #[test]
    fn what_is_not_a_number_is_refused_rather_than_repaired() {
        assert!(normalise("0500000000").is_none());
        assert!(normalise("500000000").is_none());
        assert!(normalise("+0966500000000").is_none());
        assert!(normalise("+96650000000a").is_none());
        assert!(normalise("+1234").is_none());
        assert!(normalise("").is_none());
    }

    #[test]
    fn a_code_is_six_digits() {
        for _ in 0..100 {
            let code = six_digits().expect("the OS has randomness");
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()), "{code}");
        }
    }
}
