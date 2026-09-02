//! Short links.
//!
//! A token, where it points, whether it has expired, and who has followed it.
//! That is the whole of it — and the whole of it is the point.
//!
//! # Why this is a crate and not a module
//!
//! It holds no business meaning (D11). This crate does not know whether the
//! thing on the other end is a booking, an invoice, an export somebody asked
//! for or a page on a supplier's website; it knows a string. Every module may
//! make a link in a line, nobody enables `links`, and the day this file learns
//! what a reservation is, is the day it stops being usable by the next module.
//!
//! It is also not a projection, for the reason [`erp_occupancy`] is not: **a
//! read model can be rebuilt; a link somebody has already been sent cannot be
//! un-issued.** A token in a text message on a customer's phone is a fact about
//! the world, so these rows live in the tenant migration chain where
//! `rebuild_swap` cannot reach them.
//!
//! # The practical reason it exists
//!
//! SMS is billed by length, and a segment boundary at 160 characters is a real
//! cost per message per customer. A booking link is otherwise a tenant
//! subdomain, a versioned path and an aggregate id, which is most of a segment
//! before the message has said anything.
//!
//! # How a caller uses it
//!
//! ```text
//! let token = links::shorten(&mut tx, &links::New {
//!     key: format!("booking.reminder.{booking}"),   // the caller's identity
//!     target: format!("/v1/booking/public/reservations/{booking}"),
//!     external: false,
//!     expires_at: Some(starts_at),
//!     single_use: false,
//!     at: now,
//! }).await?;
//! ```
//!
//! **The key is the caller's and the token is the database's**, and they answer
//! different questions. Shortening the same key twice returns the same token,
//! which is what makes a retried reminder one link rather than two (L8).
//! Deriving the token *from* the key would make it guessable — a key reads
//! `booking.reminder.BK-1041` — and a link is often the only thing standing
//! between a stranger and what it points at.

pub mod messages;

use erp_i18n::{Localize, Message, MessageArg, StaticCatalog};

/// This crate's messages, in every supported language.
///
/// Composed into `erp_api::CATALOG` the way `erp_occupancy`'s is, and for the
/// same reason: these are refusals the API renders, and a code with no sentence
/// behind it reaches a client as a bare string.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

use erp_types::Timestamp;
use sqlx::PgConnection;

/// The longest a target may be. Matches the column.
const MAX_TARGET: usize = 2000;

/// A link to make.
#[derive(Debug, Clone)]
pub struct New {
    /// **The caller's identity for this link.** Shortening the same key twice
    /// gives the same token back.
    pub key: String,
    /// An absolute URL when `external`, otherwise a path inside this tenant's
    /// own API.
    pub target: String,
    pub external: bool,
    /// When it stops working. `None` is most links.
    pub expires_at: Option<Timestamp>,
    /// Whether following it once is the last time.
    pub single_use: bool,
    /// When it was made. Not a clock reading — the caller's own instant, for
    /// the reason every other timestamp in this system is the caller's.
    pub at: Timestamp,
}

/// Where a token points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub target: String,
    /// Whether it leaves this system. **What decides whether a redirect is
    /// safe**: an internal target is a path this API serves, and an external
    /// one is somewhere else entirely.
    pub external: bool,
}

/// A link, as somebody managing them reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub token: String,
    pub key: String,
    pub target: String,
    pub external: bool,
    pub expires_at: Option<Timestamp>,
    pub single_use: bool,
    pub visits: i32,
    pub first_visit_at: Option<Timestamp>,
    pub last_visit_at: Option<Timestamp>,
    pub created_at: Timestamp,
}

/// Why a link did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("no such link")]
    NoSuchLink,
    #[error("that link has expired")]
    Expired,
    #[error("that link has already been used")]
    AlreadyUsed,
    /// Refused at creation, not at resolution.
    #[error("{0} is not somewhere a link may point")]
    NotATarget(String),
}

impl Localize for LinkError {
    fn message(&self) -> Message {
        match self {
            Self::NoSuchLink => Message::new(messages::NO_SUCH_LINK),
            Self::Expired => Message::new(messages::EXPIRED),
            Self::AlreadyUsed => Message::new(messages::ALREADY_USED),
            Self::NotATarget(target) => {
                Message::new(messages::NOT_A_TARGET).with("target", MessageArg::text(target))
            }
        }
    }
}

/// What can go wrong that is not the link's fault.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Refused(#[from] LinkError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Makes a link, or gives back the one this key already has.
///
/// Idempotent on `key` (L8): a retried reminder does not send a second URL.
/// **The target is not re-read on a repeat** — the first one wins, because a
/// token already on somebody's phone must keep meaning what it meant.
pub async fn shorten(conn: &mut PgConnection, new: &New) -> Result<String, StoreError> {
    check(new)?;

    // `DO UPDATE` rather than `DO NOTHING`, because `DO NOTHING` returns no row
    // on a conflict and would need a second query to find the token. Setting a
    // column to itself is the idiom for "return what is already there", and it
    // is what keeps the first target winning.
    let token = sqlx::query_scalar!(
        r#"INSERT INTO short_link
               (key, target, external, created_at, expires_at, single_use)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (key) DO UPDATE SET key = short_link.key
           RETURNING token as "token!""#,
        new.key,
        new.target,
        new.external,
        new.at,
        new.expires_at,
        new.single_use,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(token)
}

/// Follows a link, and records that it was followed.
///
/// # Why this is one statement
///
/// Expiry, single use and the visit count are decided and written together, so
/// two people tapping a single-use link at the same instant cannot both be told
/// yes. Checking first and updating second is the shape of that bug.
pub async fn follow(
    conn: &mut PgConnection,
    token: &str,
    at: Timestamp,
) -> Result<Target, StoreError> {
    let row = sqlx::query!(
        r#"UPDATE short_link
              SET visits = visits + 1,
                  first_visit_at = COALESCE(first_visit_at, $2),
                  last_visit_at = $2
            WHERE token = $1
              AND (expires_at IS NULL OR expires_at > $2)
              AND (NOT single_use OR visits = 0)
        RETURNING target as "target!", external as "external!""#,
        token,
        at,
    )
    .fetch_optional(&mut *conn)
    .await?;

    if let Some(row) = row {
        return Ok(Target {
            target: row.target,
            external: row.external,
        });
    }

    // Nothing was updated, and **which of the three reasons matters to the
    // person holding the phone**: "ask for a new one" and "check you copied it
    // whole" are different instructions.
    Err(why(conn, token, at).await?.into())
}

/// Which refusal a failed [`follow`] was.
async fn why(
    conn: &mut PgConnection,
    token: &str,
    at: Timestamp,
) -> Result<LinkError, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT expires_at, single_use as "single_use!", visits as "visits!"
             FROM short_link WHERE token = $1"#,
        token,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(LinkError::NoSuchLink);
    };
    if row.expires_at.is_some_and(|until| until <= at) {
        return Ok(LinkError::Expired);
    }
    if row.single_use && row.visits > 0 {
        return Ok(LinkError::AlreadyUsed);
    }
    // The row exists, has not expired and is not spent, so it resolved between
    // the two statements. Saying it does not exist would be a lie; the honest
    // answer is that this attempt did not get it, and a retry will.
    Ok(LinkError::NoSuchLink)
}

/// One link, by its token, without following it.
pub async fn link(conn: &mut PgConnection, token: &str) -> Result<Option<Link>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT token as "token!", key as "key!", target as "target!",
                  external as "external!", expires_at, single_use as "single_use!",
                  visits as "visits!", first_visit_at, last_visit_at,
                  created_at as "created_at!"
             FROM short_link WHERE token = $1"#,
        token,
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|row| Link {
        token: row.token,
        key: row.key,
        target: row.target,
        external: row.external,
        expires_at: row.expires_at,
        single_use: row.single_use,
        visits: row.visits,
        first_visit_at: row.first_visit_at,
        last_visit_at: row.last_visit_at,
        created_at: row.created_at,
    }))
}

/// What a link may point at.
///
/// # Why an external target must be `https`
///
/// Because this is an **open redirect** otherwise, and an open redirect on a
/// tenant's own domain is a phishing primitive: a link that reads
/// `bassat.erp.com/l/a1b2` and lands on somebody else's login page borrows the
/// tenant's credibility to do it. Restricting the scheme does not stop that on
/// its own — nothing can, once a tenant may name any host — but `javascript:`
/// and `data:` targets are the ones that turn a redirect into script execution
/// in the tenant's own origin, and those are refusable.
///
/// An internal target must be a rooted path for the same reason: `//evil.test`
/// is a protocol-relative URL, not a path, and a browser follows it off-site.
fn check(new: &New) -> Result<(), LinkError> {
    let target = new.target.trim();
    let refuse = || LinkError::NotATarget(new.target.clone());

    if target.is_empty() || target.len() > MAX_TARGET {
        return Err(refuse());
    }
    if new.external {
        if !target.starts_with("https://") {
            return Err(refuse());
        }
    } else if !target.starts_with('/') || target.starts_with("//") {
        return Err(refuse());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> Timestamp {
        "2026-05-01T00:00:00Z".parse().expect("a valid instant")
    }

    fn new(target: &str, external: bool) -> New {
        New {
            key: "k".to_owned(),
            target: target.to_owned(),
            external,
            expires_at: None,
            single_use: false,
            at: at(),
        }
    }

    #[test]
    fn an_internal_target_is_a_rooted_path() {
        assert!(check(&new("/v1/booking/public/x", false)).is_ok());
        assert!(check(&new("v1/booking", false)).is_err());
        assert!(check(&new("", false)).is_err());
    }

    /// **The open-redirect cases**, which are the reason this function exists.
    #[test]
    fn a_target_that_leaves_the_origin_is_refused_unless_it_says_so() {
        // Protocol-relative: a path to a reader, another host to a browser.
        assert!(check(&new("//evil.test/login", false)).is_err());
        // Script in the tenant's own origin.
        assert!(check(&new("javascript:alert(1)", true)).is_err());
        assert!(check(&new("data:text/html,<script>", true)).is_err());
        // Cleartext, which a link carrying access must not be.
        assert!(check(&new("http://supplier.test", true)).is_err());
        assert!(check(&new("https://supplier.test/catalogue", true)).is_ok());
    }

    #[test]
    fn a_target_longer_than_the_column_is_refused_here_rather_than_by_postgres() {
        assert!(check(&new(&format!("/{}", "x".repeat(MAX_TARGET)), false)).is_err());
    }
}
