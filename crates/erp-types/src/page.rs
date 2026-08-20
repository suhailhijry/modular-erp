//! Paging that does not lose rows.
//!
//! # The bug this exists to stop
//!
//! Every list in this system took a `limit` and returned that many rows. A
//! tenant with 201 invoices saw 200 and was told nothing — the response looked
//! exactly like a complete one. That is the worst shape a bug can have: it
//! reads as working software, and the missing row is only noticed when somebody
//! reconciles a total by hand.
//!
//! # Keyset, not offset
//!
//! `LIMIT … OFFSET …` counts rows from the start on every page, so page 40 of a
//! busy tenant's invoices is a scan of 8,000 rows to return 200. Worse, it is
//! *wrong* under concurrent writes: an invoice issued while somebody pages
//! shifts every later row by one, so a row can be skipped or seen twice.
//!
//! Keyset paging asks for "the rows after this one", using the same columns the
//! list is ordered by. It reads one index range whatever page it is on, and a
//! row inserted meanwhile cannot displace anything — the position is a value,
//! not a count.
//!
//! # Why the cursor is opaque
//!
//! Because what it holds is the query's business. Today an invoice cursor is a
//! tax point and an id; the day a list is ordered differently it is something
//! else, and a client that had parsed the old one breaks. Hex rather than
//! anything cleverer keeps this file free of dependencies — `erp-types` is
//! shared with the frontend and stays that way.

use std::fmt;

/// **Where a page left off.** Opaque by construction: a client passes back what
/// it was given and never builds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(Vec<String>);

/// The separator between a cursor's parts.
///
/// ASCII unit separator: it cannot appear in a timestamp, an id, or anything
/// else this system puts in a cursor, so no escaping is needed and there is no
/// escaping to get wrong.
const SEPARATOR: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("that is not a cursor from this API")]
pub struct NotACursor;

impl Cursor {
    /// A cursor over the values a list is ordered by, in that order.
    #[must_use]
    pub fn over(parts: &[&str]) -> Self {
        Self(parts.iter().map(|part| (*part).to_owned()).collect())
    }

    /// The parts, for the query that resumes from here.
    #[must_use]
    pub fn parts(&self) -> &[String] {
        &self.0
    }

    /// The part at `index`, or `None` — a cursor from an older build may have
    /// fewer parts than this one expects, and that is a cursor to refuse rather
    /// than to guess at.
    #[must_use]
    pub fn part(&self, index: usize) -> Option<&str> {
        self.0.get(index).map(String::as_str)
    }

    /// Reads one back.
    pub fn decode(text: &str) -> Result<Self, NotACursor> {
        let bytes = hex(text)?;
        let text = String::from_utf8(bytes).map_err(|_| NotACursor)?;
        Ok(Self(text.split(SEPARATOR).map(str::to_owned).collect()))
    }
}

impl fmt::Display for Cursor {
    /// Hex, so it survives a query string, a URL and a copy-paste without
    /// anything having to know what is inside it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.join(&SEPARATOR.to_string()).bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn hex(text: &str) -> Result<Vec<u8>, NotACursor> {
    if !text.len().is_multiple_of(2) || text.is_empty() {
        return Err(NotACursor);
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| NotACursor))
        .collect()
}

/// One page of a list, and where the next one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// `Some` when there may be more. **Absent means the list ended**, which is
    /// the statement every one of these responses used to be missing.
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    /// A page that fills its limit is one that may have more behind it.
    ///
    /// "May" and not "does": the alternative is asking for `limit + 1` rows to
    /// know for certain, which costs a row on every page to save one empty
    /// request on the last. A caller that follows cursors to the end gets one
    /// empty page, which is what every paging client already handles.
    pub fn of(items: Vec<T>, limit: i64, cursor: impl Fn(&T) -> Cursor) -> Self {
        let full = i64::try_from(items.len()).unwrap_or(i64::MAX) >= limit;
        let next = full.then(|| items.last().map(&cursor)).flatten();
        Self { items, next }
    }

    /// A page that is all there is.
    #[must_use]
    pub fn complete(items: Vec<T>) -> Self {
        Self { items, next: None }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_survives_the_round_trip() {
        let cursor = Cursor::over(&["2026-02-10T00:00:00Z", "INV-00003"]);
        let encoded = cursor.to_string();
        assert_eq!(Cursor::decode(&encoded), Ok(cursor.clone()));
        assert_eq!(cursor.part(0), Some("2026-02-10T00:00:00Z"));
        assert_eq!(cursor.part(1), Some("INV-00003"));
        assert_eq!(cursor.part(2), None);
    }

    /// Opaque: nothing a client could read a date out of and start
    /// constructing.
    #[test]
    fn a_cursor_does_not_advertise_what_is_in_it() {
        let encoded = Cursor::over(&["2026-02-10T00:00:00Z", "INV-00003"]).to_string();
        assert!(!encoded.contains("2026"));
        assert!(!encoded.contains("INV"));
        assert!(encoded.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn something_that_is_not_a_cursor_is_refused() {
        for bad in ["", "zz", "abc", "not a cursor", "INV-00003"] {
            assert_eq!(Cursor::decode(bad), Err(NotACursor), "{bad:?} was accepted");
        }
    }

    /// **The statement every list response used to be missing.**
    #[test]
    fn a_full_page_says_there_may_be_more_and_a_short_one_says_there_is_not() {
        let cursor = |n: &i64| Cursor::over(&[&n.to_string()]);

        let full = Page::of(vec![1, 2, 3], 3, cursor);
        assert!(full.next.is_some(), "a full page ended the list silently");
        assert_eq!(
            full.next,
            Some(Cursor::over(&["3"])),
            "resumes after the last"
        );

        let short = Page::of(vec![1, 2], 3, cursor);
        assert_eq!(short.next, None);

        // The last page of an exactly-divisible list is full, so a caller gets
        // one empty page. That is the trade `Page::of` documents.
        let empty: Page<i64> = Page::of(Vec::new(), 3, cursor);
        assert_eq!(empty.next, None);
        assert!(empty.is_empty());
    }
}
