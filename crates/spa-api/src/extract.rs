//! What a handler gets to assume, and who checked it.

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use spa_control::{Lane, Session, TenantDb};
use spa_i18n::Locale;

use crate::error::ApiError;
use crate::problem::Problem;
use crate::state::AppState;

/// The caller's language, from `Accept-Language`.
///
/// Infallible: an absent or unparseable header is English, not a 400. Extracted
/// on its own so an error response can be localized even when the *next*
/// extractor is what failed.
#[derive(Debug, Clone, Copy)]
pub struct Language(pub Locale);

impl<S: Send + Sync> FromRequestParts<S> for Language {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .map_or(Locale::DEFAULT, Locale::from_accept_language),
        ))
    }
}

/// Proof that a live session presented a valid token.
///
/// Not cached, unlike every other entry-path lookup: a stale membership for five
/// seconds is survivable, a stale *logout* is not.
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub session: Session,
    pub token: String,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));

        let token = bearer(parts).ok_or_else(|| {
            ApiError::Auth(spa_control::AuthError::NoSession).into_problem(locale)
        })?;

        let session = state
            .control
            .session(&token)
            .await
            .map_err(|e| ApiError::Auth(e).into_problem(locale))?;

        Ok(Self { session, token })
    }
}

/// A route into one tenant, with every access check already passed.
///
/// The extractor *is* the authorization: `ControlPlane::enter` refuses unless
/// the identity is active, the tenant is enterable, and a live membership joins
/// them. A handler taking this has been handed proof of all three, and cannot
/// obtain a `TenantDb` any other way.
#[derive(Debug)]
pub struct Tenant {
    pub db: TenantDb,
    pub session: Session,
}

impl FromRequestParts<AppState> for Tenant {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let Language(locale) = Language::from_request_parts(parts, state)
            .await
            .unwrap_or(Language(Locale::DEFAULT));
        let auth = Authenticated::from_request_parts(parts, state).await?;

        let Path(slug): Path<String> =
            Path::from_request_parts(parts, state).await.map_err(|_| {
                Problem::new(
                    StatusCode::NOT_FOUND,
                    &spa_i18n::Message::new(spa_control::messages::ACCESS_DENIED),
                    locale,
                    &crate::catalog::CATALOG,
                )
            })?;

        // Slug → id is one cached lookup, and the same 404 covers "no such
        // tenant" and "not yours".
        let tenant = state
            .control
            .tenant_by_slug(&slug)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale))?
            .ok_or_else(|| {
                ApiError::Access(spa_control::AccessError::NoSuchTenant).into_problem(locale)
            })?;

        let db = state
            .control
            .enter(auth.session.identity, tenant.id, Lane::Interactive)
            .await
            .map_err(|e| ApiError::Access(e).into_problem(locale))?;

        Ok(Self {
            db,
            session: auth.session,
        })
    }
}

/// The bearer token, if the header is well formed.
fn bearer(parts: &Parts) -> Option<String> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
        .filter(|t| !t.is_empty())
}
