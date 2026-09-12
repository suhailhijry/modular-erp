//! Turning what went wrong into a status code.
//!
//! One enum, one `From` per source error, one `status()`. Deliberately not a
//! trait: mapping a domain failure onto HTTP is a decision about *this* API, and
//! scattering it across the crates that produce the errors is how two of them
//! end up disagreeing about whether a conflict is a 409 or a 422.

use axum::http::StatusCode;
use erp_control::{AccessError, AuthError};
use erp_i18n::{Catalog, Locale, Localize, Message};

use crate::problem::Problem;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Enqueue(#[from] erp_eventlog::EnqueueError),
    #[error(transparent)]
    Append(#[from] erp_eventlog::AppendError),
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
            //
            // **`SecondFactorRequired` is here too, and the `code` is what a
            // client branches on.** The password was right and no session was
            // created, which is the same HTTP answer as a wrong password — but
            // a client reading `auth.second_factor_required` knows to ask for
            // six digits rather than to say the password was wrong.
            Self::Auth(
                AuthError::InvalidCredentials
                | AuthError::NoSession
                | AuthError::SecondFactorRequired,
            )
            | Self::Access(AccessError::NoSuchIdentity | AccessError::IdentitySuspended) => {
                StatusCode::UNAUTHORIZED
            }

            // **403, not 404.** The enumeration argument below does not apply:
            // reaching this means already holding a session *and* a live
            // membership in this tenant, so there is nothing left to discover.
            // A 404 would tell a member to give up when the one thing they can
            // do is enrol.
            //
            // **403 on the platform surface, for anyone.** There is no tenant
            // here to keep from being enumerated, and the routes are in the
            // public document — hiding them behind a 404 would hide nothing.
            // Turning off a factor that staff or a tenant requires is here too:
            // logging in again would not change the answer, so not a 401.
            //
            // **And enrolling on an account somebody else's reset left
            // link-only.** A 401 would say "your credentials were wrong" to
            // somebody whose password is right; the missing thing is the link,
            // and no amount of logging in produces one.
            Self::Access(
                AccessError::SecondFactorRequired
                | AccessError::StaffOnly(_)
                | AccessError::StaffSecondFactorRequired,
            )
            | Self::Auth(AuthError::SecondFactorKept(_) | AuthError::EnrolmentLinkRequired) => {
                StatusCode::FORBIDDEN
            }

            // 404, not 403 — and the same 404 a genuinely missing tenant gets.
            // Distinguishing "exists but you may not" from "does not exist"
            // hands out a tenant-enumeration oracle for free.
            Self::Access(
                AccessError::NoSuchTenant
                | AccessError::NotAMember
                | AccessError::DomainNotClaimed(_),
            )
            | Self::NotFound(_) => StatusCode::NOT_FOUND,

            // The tenant is real and the caller is entitled; it is simply not
            // serving right now. Retryable, so 503 rather than 403.
            Self::Access(AccessError::TenantNotActive { .. } | AccessError::Pool(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }

            // Different conflicts, one status: a name someone else took, a
            // record someone else changed first, a tenant not in the status a
            // move starts from. All mean "look at what is there now and decide
            // again".
            Self::Access(
                AccessError::SlugTaken(_)
                | AccessError::Auth(AuthError::HandleTaken(_))
                | AccessError::DomainNotProved { .. }
                | AccessError::WrongTenantStatus { .. },
            )
            | Self::Append(erp_eventlog::AppendError::Conflict { .. }) => StatusCode::CONFLICT,

            // Signing up with an address that already has an account, without
            // that account's password. A credential failure, so 401 — the same
            // answer a login gives, because it is the same question.
            Self::Access(AccessError::Auth(_)) => StatusCode::UNAUTHORIZED,

            Self::Access(AccessError::DomainProofUnavailable(_)) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Access(
                AccessError::NotAnOrigin(_)
                | AccessError::OriginOutsideDomain { .. }
                | AccessError::SuspensionReason,
            )
            | Self::BadRequest(_) => StatusCode::BAD_REQUEST,

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
