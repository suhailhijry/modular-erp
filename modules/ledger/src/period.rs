//! When the books stopped taking entries.
//!
//! # What this is for
//!
//! A VAT return is filed for a period, and the tax on it is paid. A journal
//! entry back-dated into that period afterwards changes the numbers behind a
//! declaration that has already been made — and nothing anywhere records that it
//! happened. The same goes for an invoice with a back-dated tax point, and for a
//! credit note dated into a quarter that was closed months ago.
//!
//! Closing the books is the accountant saying "these numbers are final". After
//! it, corrections go into the period that is open, which is where an auditor
//! expects to find them.
//!
//! # Why one instant rather than a table of periods
//!
//! Books close in order. January, then February, then March — a business does
//! not close March while February is still open, because the March numbers are
//! built on the February ones. So the whole state is a single watermark: the
//! instant everything before which is final.
//!
//! ponytail: a non-contiguous close — a locked prior year with one adjustment
//! period left open inside it — is a table of ranges rather than a scalar, and
//! this becomes the newest row of it. Worth building when somebody has a prior
//! year to lock; guessing at the shape now would be guessing.
//!
//! # Why exclusive
//!
//! `closed_before` is the first instant that is **still open**, so closing
//! January is `2026-02-01T00:00:00Z`. The same convention as the VAT return's
//! `until`, and for the same reason: "closed through 31 January" is a comparison
//! somebody gets wrong once a month, and gets wrong by exactly one day.
//!
//! # Where the check is
//!
//! One place: [`post_entry_in`](crate::post_entry_in). Every posting in the
//! system routes through it — hand-written entries, reversals, and everything
//! sales does, because an invoice and its journal entry commit together. A check
//! per caller would be a check somebody forgets, and the one forgotten would be
//! the one that mattered.

use erp_eventlog::ConfigError;
use erp_types::Timestamp;

/// The books, as a business has closed them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Books {
    /// The first instant still open. Everything strictly before it is final.
    ///
    /// `None` on a tenant that has never closed a period, which is every tenant
    /// until their first month end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_before: Option<Timestamp>,
}

impl Books {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "ledger.books";

    /// Whether an entry dated `occurred_on` may still be posted.
    #[must_use]
    pub fn accepts(&self, occurred_on: Timestamp) -> bool {
        self.closed_before
            .is_none_or(|closed_before| occurred_on >= closed_before)
    }
}

/// How the books stand, **read inside the caller's transaction**.
///
/// Not cached and not read once at startup: a period closed a second ago has to
/// refuse the next posting, and a check against a stale copy is a check that
/// lets exactly the entries through that somebody just closed the books to keep
/// out.
pub async fn books(conn: &mut sqlx::PgConnection) -> Result<Books, ConfigError> {
    Ok(erp_eventlog::configuration::get::<Books>(conn, Books::KEY)
        .await?
        .map(|configured| configured.value)
        .unwrap_or_default())
}

/// Closes the books before `closed_before`, or reopens them by moving it back.
///
/// **Reopening is allowed on purpose.** An accountant who closes the wrong month
/// has to be able to put it right, and a system that refuses would be one they
/// route around by editing the database. What it must not be is quiet, which is
/// what `set_by` and `set_at` on the stored value are for.
///
/// ponytail: the store keeps the current value and who set it, not a history. A
/// tenant who wants "every close and reopen, in order" needs a history table
/// beside `configuration` serving every key — a better shape than one per
/// consumer, and not worth building for a single one.
pub async fn close(
    conn: &mut sqlx::PgConnection,
    closed_before: Option<Timestamp>,
    by: Option<&str>,
) -> Result<Books, ConfigError> {
    let books = Books { closed_before };
    erp_eventlog::configuration::set(conn, Books::KEY, &books, by).await?;
    Ok(books)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> Timestamp {
        rfc3339.parse().unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn nothing_is_closed_until_something_is() {
        let open = Books::default();
        assert!(open.accepts(at("2020-01-01T00:00:00Z")));
        assert!(open.accepts(at("2099-01-01T00:00:00Z")));
    }

    /// The boundary, which is the whole reason the field is named `before`.
    #[test]
    fn the_instant_named_is_the_first_one_still_open() {
        let january_closed = Books {
            closed_before: Some(at("2026-02-01T00:00:00Z")),
        };

        assert!(
            !january_closed.accepts(at("2026-01-31T23:59:59Z")),
            "the last moment of January is closed"
        );
        assert!(
            january_closed.accepts(at("2026-02-01T00:00:00Z")),
            "and the first moment of February is not — off by one here is off by \
             a day in a filed return"
        );
        assert!(january_closed.accepts(at("2026-03-15T00:00:00Z")));
    }
}
