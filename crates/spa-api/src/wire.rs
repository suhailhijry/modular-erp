//! Shapes and helpers every module's routes need.
//!
//! Extracted when the second module arrived and wanted all four of them. With
//! one module they lived in `ledger_routes`, which was the right place for them
//! then.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use spa_eventlog::Metadata;
use spa_i18n::{Locale, Message, MessageArg, MessageCode};
use spa_types::{AggregateId, CurrencyCode, ModuleId, Money};

use crate::error::ApiError;
use crate::extract::{Allowed, Capability, Tenant};
use crate::problem::Problem;

/// `axum::Json`, refusing in this API's shape.
///
/// # Why not `axum::Json`
///
/// Its rejection is `text/plain` with no `code` — so the one thing every route
/// promises ("every failure is `application/problem+json` with a stable code")
/// was untrue for the most common client mistake there is, sending the wrong
/// body. A client that always parses `problem+json` got a surprise on exactly
/// the request where it most needed the message.
///
/// Used as both extractor and response type, so a handler swaps the import and
/// changes nothing else. The *status* is still axum's: 400 for JSON that does
/// not parse, 422 for JSON that parses into the wrong shape, 415 for a body with
/// no `Content-Type` — all correct, and only the body was wrong.
#[derive(Debug)]
pub(crate) struct Json<T>(pub(crate) T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Problem> {
        let locale = spoken_in(request.headers());
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                let code = match rejection {
                    JsonRejection::MissingJsonContentType(_) => {
                        crate::messages::UNSUPPORTED_MEDIA_TYPE
                    }
                    _ => crate::messages::MALFORMED_BODY,
                };
                Err(Problem::new(
                    rejection.status(),
                    &Message::new(code).with("reason", MessageArg::text(rejection.body_text())),
                    locale,
                    &crate::catalog::CATALOG,
                ))
            }
        }
    }
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Query`, refusing in this API's shape. Same argument as
/// [`Json`].
#[derive(Debug)]
pub(crate) struct Query<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Problem> {
        let locale = spoken_in(&parts.headers);
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(Problem::new(
                rejection.status(),
                &Message::new(crate::messages::INVALID_QUERY)
                    .with("reason", MessageArg::text(rejection.body_text())),
                locale,
                &crate::catalog::CATALOG,
            )),
        }
    }
}

/// The caller's language, when the rejection happened before
/// [`Language`](crate::extract::Language) ran.
fn spoken_in(headers: &axum::http::HeaderMap) -> Locale {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Locale::DEFAULT, Locale::from_accept_language)
}

/// An amount, as a client sends it.
///
/// Minor units and an explicit currency — never a decimal string, and never a
/// float. A client that sends `10.50` has already lost the argument about how
/// many decimal places the currency has.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[schema(example = json!({ "minor": 1050, "currency": "SAR" }))]
pub(crate) struct Amount {
    /// The amount in the currency's smallest unit. 1050 is 10.50 SAR.
    pub(crate) minor: i64,
    /// ISO 4217, upper case.
    pub(crate) currency: String,
}

impl Amount {
    pub(crate) fn parse(&self, locale: Locale) -> Result<Money, Problem> {
        let currency = CurrencyCode::new(&self.currency).map_err(|_| {
            bad_request(
                crate::messages::UNKNOWN_CURRENCY,
                "currency",
                &self.currency,
                locale,
            )
        })?;
        Ok(Money::from_minor(self.minor, currency))
    }
}

/// Records who did it. Every event carries this (architecture L5).
///
/// Generic over the capability, because every write is behind a different one
/// and they all deref to the same `Tenant`.
pub(crate) fn metadata<C: Capability>(tenant: &Allowed<C>) -> Metadata {
    Metadata {
        actor: Some(tenant.session.identity.to_string()),
        ..Metadata::default()
    }
}

pub(crate) fn parse_id(raw: &str, locale: Locale) -> Result<AggregateId, Problem> {
    AggregateId::new(raw).map_err(|_| bad_request(crate::messages::INVALID_ID, "id", raw, locale))
}

pub(crate) fn bad_request(code: MessageCode, arg: &str, value: &str, locale: Locale) -> Problem {
    ApiError::BadRequest(Message::new(code).with(arg, MessageArg::text(value.to_owned())))
        .into_problem(locale)
}

/// Refuses a route belonging to a module the tenant did not enable.
///
/// A 404, not a 403: the route does not exist for this tenant, and saying
/// "forbidden" would tell them what they are not paying for in a way a 404 does
/// not.
///
/// ponytail: a runtime check, called at the top of each module's handlers. The
/// architecture's `ModuleEnabled<M>` token would make a disabled module's
/// handler unconstructable instead — worth building when a module has enough
/// routes that remembering the call becomes the weak link.
pub(crate) fn require_module(
    tenant: &Tenant,
    module: &ModuleId,
    locale: Locale,
) -> Result<(), Problem> {
    if tenant.db.has_module(module) {
        return Ok(());
    }
    Err(ApiError::NotFound(
        Message::new(crate::messages::MODULE_NOT_ENABLED)
            .with("module", MessageArg::text(module.as_str().to_owned())),
    )
    .into_problem(locale))
}
