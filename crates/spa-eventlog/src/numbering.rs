//! Gapless document numbers.
//!
//! See `migrations/tenant/0005_numbering.sql` for why a sequence cannot do this
//! and what serializing costs.
//!
//! # How a caller uses it
//!
//! Two steps, in the transaction that writes the document:
//!
//! ```text
//! let number = numbering::reserve(&mut tx, "sales.invoice").await?;   // locks the series
//! let committed = try_execute::<Invoice, _, _>(&mut tx, id, …).await?; // decides
//! if committed.at.is_some() {
//!     numbering::consume(&mut tx, "sales.invoice").await?;            // it was used
//! }
//! ```
//!
//! # Why two calls rather than one
//!
//! Because the document might not be written. Every command here is idempotent
//! on a client-chosen key, so issuing the same invoice twice is a no-op — and a
//! single `nextval`-shaped call would burn a number on every retry. A retried
//! request is the *normal* case for a client that timed out, so burning one
//! there would put gaps in the sequence of a business that did nothing wrong.
//!
//! [`reserve`] takes the row lock without moving the counter; [`consume`] moves
//! it. Between them nobody else can be issuing in this series, so the number
//! [`reserve`] returned is still the one [`consume`] hands out.
//!
//! **Reserving and then not consuming is safe. Writing a document and then not
//! consuming is not** — the next document gets the same number. That pairing is
//! the one thing this module cannot enforce from here, so it is what
//! `re_issuing_does_not_move_the_series` in `modules/sales/tests/sales.rs`
//! tests directly.

use sqlx::PgConnection;

#[derive(Debug, thiserror::Error)]
pub enum NumberingError {
    /// A series ran past what a document number can hold.
    ///
    /// Refuses rather than wrapping. `i64::MAX` invoices is not a business
    /// anybody has, so this is a corrupted counter, and continuing from a
    /// corrupted counter is how a sequence silently repeats.
    #[error("the {series} series is exhausted")]
    Exhausted { series: String },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl spa_i18n::Localize for NumberingError {
    fn message(&self) -> spa_i18n::Message {
        // Both are ours. A tenant cannot cause either, and neither has an action
        // a user could take.
        spa_i18n::Message::new(crate::messages::INTERNAL)
    }
}

/// The number this series will hand out next, locked to this transaction.
///
/// Creates the series at 1 the first time it is asked for, so a tenant needs no
/// setup step and a module added later needs no migration.
///
/// **Holds a row lock until the transaction ends.** That is the point: it is
/// what makes the number this returns still correct when [`consume`] runs.
pub async fn reserve(conn: &mut PgConnection, series: &str) -> Result<i64, NumberingError> {
    // One statement rather than a select-then-insert: two concurrent first-ever
    // reservations would both find nothing and both insert. `ON CONFLICT DO
    // UPDATE` with a no-op assignment takes the row lock in the losing
    // transaction too, which a bare `DO NOTHING` would not — it would return no
    // row and leave the series unlocked.
    let next: i64 = sqlx::query_scalar(
        "INSERT INTO document_number (series, next) VALUES ($1, 1)
         ON CONFLICT (series) DO UPDATE SET next = document_number.next
         RETURNING next",
    )
    .bind(series)
    .fetch_one(&mut *conn)
    .await?;

    if next == i64::MAX {
        return Err(NumberingError::Exhausted {
            series: series.to_owned(),
        });
    }

    Ok(next)
}

/// Hands the reserved number out, so the next document gets the one after it.
///
/// Call this **only** when the document was actually written. The transaction
/// rolling back releases it, which is what makes the series gapless across a
/// crash as well as across a refusal.
pub async fn consume(conn: &mut PgConnection, series: &str) -> Result<(), NumberingError> {
    let updated = sqlx::query(
        "UPDATE document_number SET next = next + 1, updated_at = now() WHERE series = $1",
    )
    .bind(series)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    debug_assert_eq!(
        updated, 1,
        "consume({series}) matched no row — reserve was not called in this transaction"
    );
    Ok(())
}

/// Where a series stands, without locking it.
///
/// For a settings screen and for tests. Never for issuing: reading a counter you
/// do not hold the lock on tells you where it was, not where it is.
pub async fn peek(conn: &mut PgConnection, series: &str) -> Result<i64, NumberingError> {
    let next: Option<i64> =
        sqlx::query_scalar("SELECT next FROM document_number WHERE series = $1")
            .bind(series)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(next.unwrap_or(1))
}

/// Starts a series somewhere other than 1.
///
/// For a business arriving from another system that reached invoice 4,107 and
/// must not start again at one. Refuses to move a series **backwards**, which
/// would reissue numbers that are already on documents somebody holds.
pub async fn start_at(
    conn: &mut PgConnection,
    series: &str,
    next: i64,
) -> Result<i64, NumberingError> {
    let settled: i64 = sqlx::query_scalar(
        "INSERT INTO document_number (series, next) VALUES ($1, $2)
         ON CONFLICT (series) DO UPDATE
            SET next = GREATEST(document_number.next, EXCLUDED.next),
                updated_at = now()
         RETURNING next",
    )
    .bind(series)
    .bind(next.max(1))
    .fetch_one(&mut *conn)
    .await?;

    Ok(settled)
}
