//! Turning what went wrong into a status code.
//!
//! One enum, one `From` per source error, one `status()`. Deliberately not a
//! trait: mapping a domain failure onto HTTP is a decision about *this* API, and
//! scattering it across the crates that produce the errors is how two of them
//! end up disagreeing about whether a conflict is a 409 or a 422.

use axum::http::StatusCode;
use spa_control::{AccessError, AuthError};
use spa_i18n::{Catalog, Locale, Localize, Message};

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
            Self::Access(
                AccessError::SlugTaken(_) | AccessError::Auth(AuthError::HandleTaken(_)),
            )
            | Self::Append(spa_eventlog::AppendError::Conflict { .. }) => StatusCode::CONFLICT,

            // Signing up with an address that already has an account, without
            // that account's password. A credential failure, so 401 — the same
            // answer a login gives, because it is the same question.
            Self::Access(AccessError::Auth(_)) => StatusCode::UNAUTHORIZED,

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
    ///
    /// # Why the catalog is an argument
    ///
    /// Because [`Self::BadRequest`] and [`Self::NotFound`] carry a message the
    /// *caller* chose, and only the caller knows which catalog can render it.
    /// This used to render through a fixed one, which was fine while every
    /// caller lived in the same crate — and the moment modules started shipping
    /// their own routes, `ledger.does_not_balance` came back to a client as the
    /// bare code with no sentence in it.
    ///
    /// So the caller passes the catalog it renders everything else through, and
    /// a message it can name is a message it can render. There is deliberately
    /// no `IntoResponse for ApiError`: it could not name a catalog, so `?` on
    /// one in a handler would have silently taken this same wrong turn.
    #[must_use]
    pub fn into_problem(self, locale: Locale, catalog: &dyn Catalog) -> Problem {
        let status = self.status();
        if status.is_server_error() {
            // The only place the internal `Display` text is recorded. It never
            // reaches the response — a 500 tells a user nothing but "ours".
            tracing::error!(error = %self, "request failed");
        }
        Problem::new(status, &self.message(), locale, catalog)
    }
}
