//! Which browsers may call a tenant's public API, and from where.
//!
//! # Why this is written here rather than configured from a library
//!
//! `tower-http`'s `CorsLayer` decides an origin with a **synchronous**
//! predicate. The answer here is per tenant and lives in the control database,
//! so deciding it is an `await` — and the only way to give a sync predicate an
//! async answer is a second cache, refreshed on some schedule, with its own
//! staleness story separate from the one the entry path already has.
//!
//! So the check goes through `ControlPlane::allows_origin`, which is the same
//! cache, the same five-second TTL and the same fleet-wide invalidation as
//! every other entry-path lookup. One staleness story, not two.
//!
//! # What it must never do
//!
//! **Never `Access-Control-Allow-Origin: *`.** This surface reads a business's
//! diary and will take deposits. A wildcard would let any page on the internet
//! read one tenant's data with a visitor's browser.
//!
//! **Never a suffix match.** `https://salon.com` must not admit
//! `https://salon.com.attacker.example`, which is what `ends_with` does and is
//! the most common way this check is written wrong. The comparison is whole
//! strings, in the control plane, and `a_lookalike_origin_is_refused` is the
//! test that keeps it that way.
//!
//! **Never credentials.** Nothing here sets
//! `Access-Control-Allow-Credentials`, so a browser will not attach cookies to
//! these requests and a stolen session cannot be replayed from a tenant's own
//! site. The public surface has no session by design, and this is that decision
//! enforced one layer down.
//!
//! # What an unknown origin gets
//!
//! The request proceeds **without** the header. That is deliberate: refusing
//! with a 403 would tell a page which origins a tenant has configured, and the
//! browser blocks the response either way. A caller that is not a browser —
//! curl, a server, this crate's own tests — is unaffected, which is correct,
//! because CORS was never an authentication mechanism.

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// The headers a booking site legitimately sends.
///
/// Listed rather than reflected: echoing `Access-Control-Request-Headers` back
/// makes the allowlist whatever the caller asked for, which is not an
/// allowlist. `Authorization` is **not** here — this surface has no session.
const ALLOWED_HEADERS: &str = "content-type, accept-language, idempotency-key";

/// How long a browser may skip the preflight. Ten minutes: long enough that a
/// site is not preflighting every click, short enough that revoking an origin
/// is not remembered all afternoon.
const MAX_AGE: &str = "600";

/// Answers preflights and stamps allowed responses.
///
/// A request with no `Origin` is not a browser doing cross-origin work, so it
/// passes through untouched — which is every request this system served before
/// Phase 17.
pub async fn layer(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return next.run(request).await;
    };

    let allowed = allows(&state, request.headers(), &origin).await;

    // A preflight is answered here and never reaches a handler: it carries no
    // credential and asks only whether the real request would be permitted.
    if request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key("access-control-request-method")
    {
        return preflight(allowed.as_deref());
    }

    let mut response = next.run(request).await;
    if let Some(origin) = allowed {
        stamp(response.headers_mut(), &origin);
    } else {
        // **`Vary: Origin` even when refusing.** Without it a shared cache can
        // serve one origin's stamped response to another, which turns a correct
        // check into an incorrect one somewhere downstream.
        vary(response.headers_mut());
    }
    response
}

/// The origin, if this tenant answers it.
async fn allows(state: &AppState, headers: &axum::http::HeaderMap, origin: &str) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let slug = crate::extract::tenant_label(host, &state.domain)?;

    let tenant = state.control.tenant_by_slug(&slug).await.ok()??;
    state
        .control
        .allows_origin(tenant.id, origin)
        .await
        .ok()?
        .then(|| origin.to_owned())
}

fn preflight(origin: Option<&str>) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    vary(headers);

    // A refused preflight is still a 204 with no allow header. The browser
    // draws the conclusion; saying more would describe the allowlist to a page
    // that is not on it.
    let Some(origin) = origin else {
        return response;
    };

    stamp(headers, origin);
    insert(headers, header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST");
    insert(
        headers,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        ALLOWED_HEADERS,
    );
    insert(headers, header::ACCESS_CONTROL_MAX_AGE, MAX_AGE);
    response
}

fn stamp(headers: &mut axum::http::HeaderMap, origin: &str) {
    vary(headers);
    insert(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
}

fn vary(headers: &mut axum::http::HeaderMap) {
    insert(headers, header::VARY, "Origin");
}

/// Silently drops a value that cannot be a header, which is unreachable: every
/// caller passes either a literal or an origin the control plane's `CHECK`
/// constraint has already restricted to ASCII.
fn insert(headers: &mut axum::http::HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}
