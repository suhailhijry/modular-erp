//! Which version of this API a client was built against.
//!
//! # What this is not
//!
//! **Not app version gating.** Nothing here cares which build of a phone app is
//! calling; it cares which *contract* the caller was compiled against, which is
//! a different fact with a different answer. A mobile app three releases behind
//! is fine if the contract it uses is still served.
//!
//! # The shape, and where it comes from
//!
//! The same shape as `MIGRATION_FLOOR` refusing a tenant that is too far behind,
//! and the same reasoning as D17's two majors: a server that will serve
//! *anything* is a server whose old paths were never tested. So there is a
//! floor, a current, and a refusal outside them that **names what to build
//! against** — because "unsupported" with no number is a support ticket.
//!
//! # A version that is behind is served, and says so
//!
//! Deprecation that arrives as a surprise is an outage. A request inside the
//! range but below current is answered normally, with headers saying what is
//! current — so a client that logs its own responses finds out months before
//! the floor moves.
//!
//! # An absent header is current
//!
//! Deliberately, and it is the one permissive choice here. `curl`, a browser
//! and this crate's own tests send nothing, and refusing them would mean the
//! API could not be tried without reading the documentation first. A client
//! that *declares* a version gets the contract it asked for; one that does not
//! is asking for whatever is current, which is what it will get.

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::problem::Problem;
use erp_i18n::{Locale, MessageArg};

/// The contract this build serves.
pub const CURRENT: u32 = 1;

/// The oldest contract it still serves.
///
/// Equal to [`CURRENT`] while there has only ever been one. When a second
/// arrives, this is what moves — deliberately and visibly, in a release note,
/// rather than by a route quietly changing shape.
pub const FLOOR: u32 = 1;

/// The header a client declares its version in.
pub const HEADER: &str = "x-api-version";
/// What this build serves, on every response.
pub const CURRENT_HEADER: &str = "x-api-current";
/// The oldest it serves, on every response.
pub const MINIMUM_HEADER: &str = "x-api-minimum";
/// Set when the version asked for is served but behind.
pub const DEPRECATED_HEADER: &str = "x-api-deprecated";

/// What to do with a declared version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Serve it, and it is what this build speaks.
    Current,
    /// Serve it, and say it is behind.
    Behind,
    /// Refuse: older than the floor.
    TooOld,
    /// Refuse: newer than anything this build has.
    TooNew,
}

impl Verdict {
    #[must_use]
    pub const fn served(self) -> bool {
        matches!(self, Self::Current | Self::Behind)
    }
}

/// The whole decision, as a function of three numbers.
///
/// Separated from the middleware so it can be tested at versions this build does
/// not have — there has only ever been one contract, and a mechanism that is
/// only exercised at `1..=1` is a mechanism nobody has tested.
#[must_use]
pub const fn decide(requested: u32, floor: u32, current: u32) -> Verdict {
    if requested < floor {
        Verdict::TooOld
    } else if requested > current {
        Verdict::TooNew
    } else if requested < current {
        Verdict::Behind
    } else {
        Verdict::Current
    }
}

/// Refuses a client outside the range, and stamps every response with it.
pub async fn layer(request: Request, next: Next) -> Response {
    let locale = request
        .headers()
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Locale::DEFAULT, Locale::from_accept_language);

    let declared = request
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|raw| !raw.is_empty());

    let verdict = match declared {
        None => Verdict::Current,
        Some(raw) => match raw.parse::<u32>() {
            Ok(requested) => decide(requested, FLOOR, CURRENT),
            // Not a number. **A refusal and not "assume current"**: a client
            // that sends `v2` or `2.1` believes it is asking for something, and
            // serving it whatever we have is how it finds out at the worst
            // moment.
            Err(_) => Verdict::TooNew,
        },
    };

    if !verdict.served() {
        // **Stamped too.** A client that has just been told its version is
        // wrong is exactly the one that needs to know which is right, and the
        // numbers are in the body as well — a header is what a proxy and a log
        // can see without parsing JSON.
        let mut response = refuse(verdict, declared.unwrap_or_default(), locale).into_response();
        stamp(response.headers_mut(), false);
        return response;
    }

    let mut response = next.run(request).await;
    stamp(response.headers_mut(), verdict == Verdict::Behind);
    response
}

/// **A typed error a client can act on, not a 500.**
///
/// It names the version to build against, because that is the only actionable
/// part of the answer.
fn refuse(verdict: Verdict, declared: &str, locale: Locale) -> Problem {
    let code = if verdict == Verdict::TooOld {
        crate::messages::API_VERSION_TOO_OLD
    } else {
        crate::messages::API_VERSION_TOO_NEW
    };

    Problem::new(
        StatusCode::BAD_REQUEST,
        &erp_i18n::Message::new(code)
            .with("declared", MessageArg::text(declared))
            .with("minimum", MessageArg::Int(i64::from(FLOOR)))
            .with("current", MessageArg::Int(i64::from(CURRENT))),
        locale,
        &crate::CATALOG,
    )
}

/// What this build serves, on every response.
///
/// On **every** one, including refusals: a client that has just been told its
/// version is wrong is exactly the one that needs to know which is right.
fn stamp(headers: &mut axum::http::HeaderMap, behind: bool) {
    headers.insert(
        axum::http::HeaderName::from_static(CURRENT_HEADER),
        HeaderValue::from(CURRENT),
    );
    headers.insert(
        axum::http::HeaderName::from_static(MINIMUM_HEADER),
        HeaderValue::from(FLOOR),
    );
    if behind {
        headers.insert(
            axum::http::HeaderName::from_static(DEPRECATED_HEADER),
            HeaderValue::from_static("true"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mechanism, at versions this build does not have.
    #[test]
    fn a_version_inside_the_range_is_served_and_one_outside_is_not() {
        // A build serving 2..=4.
        assert_eq!(decide(1, 2, 4), Verdict::TooOld);
        assert_eq!(decide(2, 2, 4), Verdict::Behind);
        assert_eq!(decide(3, 2, 4), Verdict::Behind);
        assert_eq!(decide(4, 2, 4), Verdict::Current);
        assert_eq!(decide(5, 2, 4), Verdict::TooNew);

        assert!(decide(2, 2, 4).served());
        assert!(!decide(1, 2, 4).served());
    }

    /// The build as it stands: one contract, and it is both ends of the range.
    #[test]
    fn this_build_serves_exactly_one_contract() {
        assert_eq!(FLOOR, CURRENT, "the range grew and this test did not");
        assert_eq!(decide(CURRENT, FLOOR, CURRENT), Verdict::Current);
        assert_eq!(decide(CURRENT + 1, FLOOR, CURRENT), Verdict::TooNew);
    }

    /// **Two majors**, when there are three contracts. D17's rule, restated as
    /// arithmetic, so moving the floor is a decision rather than a drift.
    #[test]
    fn a_client_two_versions_back_is_still_served() {
        assert!(decide(1, 1, 3).served());
        assert!(!decide(0, 1, 3).served());
    }
}
