//! Getting back in, and changing your mind.
//!
//! See `erp_control::passwords` for why a reset demands the second factor and
//! issues no session. This layer adds the two things only it knows: where the
//! link points, and what a stranger is allowed to ask for per minute.

use axum::extract::State;
use axum::http::StatusCode;
use erp_i18n::Locale;
use erp_web::{Anonymous, Authenticated, Json, Language, Problem};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;

/// The floor a new password has to clear, the same as a signup's.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(request_password_reset))
        .routes(routes!(redeem_password_reset))
        .routes(routes!(change_password))
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "email": "sara@bassat.test" }))]
struct ResetRequest {
    /// The address to send a link to.
    email: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "token": "…", "password": "hunter2hunter2" }))]
struct Redemption {
    /// The token from the link, **sent in the body rather than the path**: a
    /// path lands in access logs, browser history and the `Referer` of every
    /// asset the page loads, and this one is worth what a password is worth.
    token: String,
    /// The new password. Twelve characters, the same floor as a signup's.
    password: String,
    /// A code from the authenticator app, or one of the recovery codes.
    /// Required when the account has a second factor — ask for it after a
    /// `401` says so.
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "current": "hunter2hunter2", "new": "correcthorsebattery" }))]
struct Change {
    /// The password being replaced. Asked for because a session left open on a
    /// shared machine is not proof of anything.
    current: String,
    /// The new one.
    new: String,
}

/// **Send a link to somebody who has forgotten their password.**
///
/// The answer is `202` whether or not the address has an account, and an
/// address with no account is sent nothing. Telling a stranger which addresses
/// are registered is one thing; mailing every address they type is a worse one,
/// because the cost lands on the sending domain that carries every tenant's
/// signup and invitation mail.
#[utoipa::path(
    post,
    path = "/v1/password-resets",
    tag = "sessions",
    // Unauthenticated by definition: not being able to log in is the reason.
    security(),
    request_body = ResetRequest,
    responses(
        (status = ACCEPTED, description = "If that address has an account, a link is on its way. Nothing has changed."),
        (status = TOO_MANY_REQUESTS, description = "A link went to this address moments ago, or too many attempts came from here. Retryable, and the message says when.", body = Problem),
    ),
)]
async fn request_password_reset(
    anonymous: Anonymous,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<ResetRequest>,
) -> Result<StatusCode, Problem> {
    // The same per-account bound as a login, because this is a route about one
    // account that anybody may call.
    anonymous.charge_for_handle(&state, &body.email).await?;

    // **Where the link points is decided here**, because only this layer knows
    // the deployment's public domain. The token is appended by
    // `request_password_reset`, which is the only place it exists in the clear.
    let link_base = format!("https://{}/reset/", state.domain);

    match state
        .control
        .request_password_reset(&body.email, locale, &link_base)
        .await
    {
        // **Identical on purpose, and the identity is the point.** A cooldown
        // only ever fires for an address that has an account, so answering it
        // differently would say which addresses do — and the per-handle
        // limiter above has already charged this caller either way.
        Ok(_) | Err(erp_control::PasswordError::TooSoon { .. }) => Ok(StatusCode::ACCEPTED),
        Err(e) => Err(password_problem(&e, locale)),
    }
}

/// **Choose the new password.**
///
/// Issues no session: log in afterwards. That is not an oversight — turning a
/// reset into a session would let somebody holding the link disable the
/// account's second factor, which needs nothing but a session.
#[utoipa::path(
    post,
    path = "/v1/password-resets/redemption",
    tag = "sessions",
    security(),
    request_body = Redemption,
    responses(
        (status = NO_CONTENT, description = "Changed. Every session ended; log in with the new password."),
        (status = BAD_REQUEST, description = "A password under 12 characters", body = Problem),
        (status = UNAUTHORIZED, description = "The account has a second factor and no code came, or the code was wrong. `auth.second_factor_required` means ask for one and retry.", body = Problem),
        (status = NOT_FOUND, description = "That link is wrong, expired, already used, or has had too many wrong codes", body = Problem),
        (status = TOO_MANY_REQUESTS, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key, so a second factor cannot be checked", body = Problem),
    ),
)]
async fn redeem_password_reset(
    anonymous: Anonymous,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Redemption>,
) -> Result<StatusCode, Problem> {
    // The per-caller bound is charged by `Anonymous` itself, on the way in.
    let _ = &anonymous;
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(short_password(locale));
    }

    state
        .control
        .reset_password(
            &body.token,
            &body.password,
            body.code.as_deref(),
            erp_types::Timestamp::from(chrono::Utc::now()),
            crate::routes::sealing_key(&state, locale)?,
        )
        .await
        .map_err(|e| password_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// **Change the password you are signed in with.**
///
/// Ends every session, this one included. A password is changed because it may
/// be known, and a session minted under it is that password still working.
#[utoipa::path(
    post,
    path = "/v1/sessions/current/password",
    tag = "sessions",
    request_body = Change,
    responses(
        (status = NO_CONTENT, description = "Changed. Every session ended, including this one; log in again."),
        (status = BAD_REQUEST, description = "A password under 12 characters", body = Problem),
        (status = UNAUTHORIZED, description = "The current password did not match, or this account has no password to change", body = Problem),
    ),
)]
async fn change_password(
    auth: Authenticated,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Change>,
) -> Result<StatusCode, Problem> {
    if body.new.chars().count() < MIN_PASSWORD {
        return Err(short_password(locale));
    }

    state
        .control
        .change_password(auth.session.identity, &body.current, &body.new)
        .await
        .map_err(|e| password_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// The same shape `signup_problem` uses, and mostly the same answers.
fn password_problem(error: &erp_control::PasswordError, locale: Locale) -> Problem {
    use erp_control::PasswordError;
    let status = match error {
        // 404, not 400: a wrong link and a spent one are the same answer, and
        // that answer is "there is nothing here".
        PasswordError::NotValid => StatusCode::NOT_FOUND,
        PasswordError::TooSoon { .. } => StatusCode::TOO_MANY_REQUESTS,
        // **The one a client acts on.** It means "ask for six digits and send
        // the same link back"; the link is still unspent.
        PasswordError::Auth(
            erp_control::AuthError::SecondFactorRequired
            | erp_control::AuthError::InvalidCredentials,
        ) => StatusCode::UNAUTHORIZED,
        other => {
            tracing::error!(error = %other, "password change failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };

    let message = if status.is_server_error() {
        erp_i18n::Message::new(erp_control::messages::INTERNAL)
    } else {
        erp_i18n::Localize::message(error)
    };
    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}

fn short_password(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::BAD_REQUEST,
        &erp_i18n::Message::new(erp_web::messages::PASSWORD_TOO_SHORT).with(
            "least",
            erp_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
        ),
        locale,
        &crate::catalog::CATALOG,
    )
}
