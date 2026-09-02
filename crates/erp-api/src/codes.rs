//! Signing in with a phone number.
//!
//! # Why this market needs it
//!
//! A phone number is the identity here and an email address often is not. A
//! login that insists on one excludes people who have a phone, a bank account
//! and a business, and no inbox they read.
//!
//! # Two limiters, because they fail differently
//!
//! **Requesting** a code is bounded by a cooldown per number: the failure is
//! somebody using this system to send texts, which costs money and annoys
//! whoever owns the number. **Verifying** one is bounded by attempts on the code
//! itself: the failure is guessing, and a million guesses against six digits is
//! minutes.
//!
//! One limiter would have to be the stricter of the two everywhere, which makes
//! the ordinary case worse in order to defend against the rarer one. Both are in
//! `erp_control::otp`, in the database, so they hold across pods.
//!
//! # Two surfaces, one session
//!
//! A browser gets a cookie and everything else gets a bearer token, and they are
//! the **same session row** — see [`crate::codes::session_cookie`]. Two
//! authentication surfaces with one authorization answer, rather than two
//! parallel notions of who somebody is.

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use erp_control::{OtpError, SessionToken};
use erp_i18n::{Locale, Localize};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::{Json, Language, Problem};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(request_code))
        .routes(routes!(sign_in_with_code))
}

/// The cookie a browser session lives in.
pub(crate) const SESSION_COOKIE: &str = "erp_session";

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "phone": "+966500000000" }))]
struct CodeRequest {
    /// With the country code. `+966500000000`, or `00966500000000`.
    phone: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct CodeSent {
    /// The number as it was understood, so a caller can show it back.
    phone: String,
    /// When the code stops working.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "phone": "+966500000000", "code": "123456" }))]
struct CodeVerification {
    phone: String,
    /// The six digits from the text.
    code: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct SignedIn {
    /// Send as `Authorization: Bearer <token>`.
    ///
    /// **Also set as an `HttpOnly` cookie**, and they are the same session. A
    /// browser can ignore this field entirely.
    token: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
}

/// **Send a one-time code to a phone number.**
///
/// Answers the same way whether or not the number is known: `202` and an expiry.
/// Saying "no account" would make this a way to ask whether somebody has one.
///
/// Nothing is created. An identity is made when a code is *verified*, because
/// creating one on request would let anybody fill the table by typing numbers.
#[utoipa::path(
    post,
    path = "/v1/codes",
    tag = "sessions",
    security(),
    request_body = CodeRequest,
    responses(
        (status = ACCEPTED, description = "A code is on its way, if that number can receive one.", body = CodeSent),
        (status = BAD_REQUEST, description = "Not a phone number", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "One went moments ago. `args.seconds` says how long to wait.", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn request_code(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<CodeRequest>,
) -> Result<(StatusCode, Json<CodeSent>), Problem> {
    let requested = state
        .control
        .request_code(&body.phone, locale)
        .await
        .map_err(|e| refused(&e, locale))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(CodeSent {
            phone: requested.handle.clone(),
            expires_at: requested.expires_at,
        }),
    ))
}

/// **Sign in with the code.**
///
/// One answer for every way this fails — wrong, expired, spent, out of attempts,
/// never issued. Distinguishing them would say whether the number is known and
/// whether a code is outstanding, which are two things a caller should already
/// know.
#[utoipa::path(
    post,
    path = "/v1/sessions/code",
    tag = "sessions",
    security(),
    request_body = CodeVerification,
    responses(
        (status = CREATED, description = "Signed in. The token is in the body and in an `HttpOnly` cookie; they are the same session.", body = SignedIn),
        (status = BAD_REQUEST, description = "Not a phone number", body = Problem),
        (status = UNAUTHORIZED, description = "That code is not valid — one answer for every reason", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn sign_in_with_code(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<CodeVerification>,
) -> Result<impl IntoResponse, Problem> {
    let (token, session) = state
        .control
        .verify_code(&body.phone, &body.code)
        .await
        .map_err(|e| refused(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(SignedIn {
            token: token.expose().to_owned(),
            expires_at: session.expires_at,
        }),
    ))
}

/// The cookie a browser holds a session in.
///
/// # Why these attributes and not others
///
/// **`HttpOnly`**, so a script cannot read it — which is the whole point of
/// using a cookie rather than `localStorage`.
///
/// **`SameSite=Strict`**, which is this system's CSRF defence. A cross-site
/// request does not carry the cookie at all, so there is nothing for a forged
/// form to ride on and no token to check on every write. It costs one thing: a
/// link from an email into the app arrives signed out, and the fix for that is
/// `Lax` on a day somebody minds.
///
/// **`Secure`**, so it never travels in the clear — except on `localhost`,
/// where there is no TLS and a developer would otherwise be unable to sign in.
/// The exception is on the *attribute*, not on the check: a deployment serving
/// a real domain over HTTP has other problems.
///
/// **`Path=/`**, because the whole API is behind it.
#[must_use]
pub(crate) fn session_cookie(token: &SessionToken) -> HeaderValue {
    let cookie = format!(
        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Secure; Path=/",
        token.expose()
    );
    HeaderValue::from_str(&cookie)
        .unwrap_or_else(|_| unreachable!("a session token is hex and the rest is a literal"))
}

/// The cookie that ends one.
#[must_use]
pub(crate) fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_static("erp_session=; HttpOnly; SameSite=Strict; Secure; Path=/; Max-Age=0")
}

fn refused(error: &OtpError, locale: Locale) -> Problem {
    let status = match error {
        OtpError::NotANumber(_) => StatusCode::BAD_REQUEST,
        OtpError::TooSoon { .. } => StatusCode::TOO_MANY_REQUESTS,
        OtpError::NotValid => StatusCode::UNAUTHORIZED,
        OtpError::Access(_) | OtpError::Auth(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    Problem::new(status, &error.message(), locale, &crate::CATALOG)
}
