//! Turning what went wrong into a status code.
//!
//! One enum, one `From` per source error, one `status()`. Deliberately not a
//! trait: mapping a domain failure onto HTTP is a decision about *this* API, and
//! scattering it across the crates that produce the errors is how two of them
//! end up disagreeing about whether a conflict is a 409 or a 422.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use spa_control::{AccessError, AuthError};
use spa_i18n::{Locale, Localize, Message};

use crate::catalog::CATALOG;
use crate::problem::Problem;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Enqueue(#[from] spa_eventlog::EnqueueError),
    #[error(transparent)]
    Append(#[from] spa_eventlog::AppendError),
    /// A request that parsed but asked for something impossible.
    #[error("bad request: {}", .0.code)]
    BadRequest(Message),
    /// A route or a record that is not there for this caller.
    #[error("not found: {}", .0.code)]
    NotFound(Message),
}

impl ApiError {
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            // No credential, a dead one, or an identity that can no longer sign
            // in. 401 for all of them: the client's move is the same, which is
            // to log in again.
            Self::Auth(AuthError::InvalidCredentials | AuthError::NoSession)
            | Self::Access(AccessError::NoSuchIdentity | AccessError::IdentitySuspended) => {
                StatusCode::UNAUTHORIZED
            }

            // 404, not 403 — and the same 404 a genuinely missing tenant gets.
            // Distinguishing "exists but you may not" from "does not exist"
            // hands out a tenant-enumeration oracle for free.
            Self::Access(AccessError::NoSuchTenant | AccessError::NotAMember) => {
                StatusCode::NOT_FOUND
            }

            // The tenant is real and the caller is entitled; it is simply not
            // serving right now. Retryable, so 503 rather than 403.
            Self::Access(AccessError::TenantNotActive { .. } | AccessError::Pool(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }

            // Two different conflicts, one status: a name someone else took, and
            // a record someone else changed first. Both mean "look at what is
            // there now and decide again".
            Self::Access(AccessError::SlugTaken(_))
            | Self::Append(spa_eventlog::AppendError::Conflict { .. }) => StatusCode::CONFLICT,

            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,

            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> Message {
        match self {
            Self::Auth(e) => e.message(),
            Self::Access(e) => e.message(),
            Self::Enqueue(e) => e.message(),
            Self::Append(e) => e.message(),
            Self::BadRequest(message) | Self::NotFound(message) => message.clone(),
        }
    }

    /// Renders as problem+json in the caller's language.
    #[must_use]
    pub fn into_problem(self, locale: Locale) -> Problem {
        let status = self.status();
        if status.is_server_error() {
            // The only place the internal `Display` text is recorded. It never
            // reaches the response — a 500 tells a user nothing but "ours".
            tracing::error!(error = %self, "request failed");
        }
        Problem::new(status, &self.message(), locale, &CATALOG)
    }
}

impl IntoResponse for ApiError {
    /// Falls back to the default locale.
    ///
    /// Handlers that know the caller's language call
    /// [`into_problem`](ApiError::into_problem) instead; this exists so `?` in a
    /// handler is never a compile error, not so it is the normal path.
    fn into_response(self) -> Response {
        self.into_problem(Locale::DEFAULT).into_response()
    }
}
