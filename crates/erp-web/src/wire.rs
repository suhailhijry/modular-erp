//! Shapes and helpers every module's routes need.
//!
//! Extracted when the second module arrived and wanted all four of them, and
//! moved down here when the routes moved *up* into the modules — a module that
//! cannot name `Json` or `require_module` cannot ship its own HTTP surface.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use erp_eventlog::Metadata;
use erp_i18n::{Locale, Message, MessageArg, MessageCode};
use erp_types::{AggregateId, CurrencyCode, ModuleId, Money};
use serde::Deserialize;
use serde::de::DeserializeOwned;

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
pub struct Json<T>(pub T);

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
                    &crate::CATALOG,
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
pub struct Query<T>(pub T);

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
                &crate::CATALOG,
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
pub struct Amount {
    /// The amount in the currency's smallest unit. 1050 is 10.50 SAR.
    pub minor: i64,
    /// ISO 4217, upper case.
    pub currency: String,
}

impl Amount {
    pub fn parse(&self, locale: Locale) -> Result<Money, Problem> {
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
pub fn metadata<C: Capability>(tenant: &Allowed<C>) -> Metadata {
    let metadata = Metadata {
        actor: Some(tenant.session.identity.to_string()),
        ..Metadata::default()
    };
    // **Every write carries its branch**, because it is folded in here rather
    // than at each handler. See `Allowed::branch`.
    tenant.branch.as_ref().map_or(metadata.clone(), |branch| {
        metadata.at_branch(branch.as_str())
    })
}

/// The metadata a **create** runs under: who did it, and which request it was.
///
/// The second half is what `erp_eventlog::try_create` reads to tell a retry from
/// a different request that reused an identifier. A create that used plain
/// [`metadata`] would treat every repeat as a retry, which is the silent
/// document loss this pair exists to prevent — so creates call this one.
pub fn creating<C: Capability>(tenant: &Allowed<C>, key: &crate::IdempotencyKey) -> Metadata {
    metadata(tenant).with_fingerprint(key.fingerprint())
}

pub fn parse_id(raw: &str, locale: Locale) -> Result<AggregateId, Problem> {
    AggregateId::new(raw).map_err(|_| bad_request(crate::messages::INVALID_ID, "id", raw, locale))
}

pub fn bad_request(code: MessageCode, arg: &str, value: &str, locale: Locale) -> Problem {
    ApiError::BadRequest(Message::new(code).with(arg, MessageArg::text(value.to_owned())))
        .into_problem(locale, &crate::CATALOG)
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
pub fn require_module(tenant: &Tenant, module: &ModuleId, locale: Locale) -> Result<(), Problem> {
    if tenant.db.has_module(module) {
        return Ok(());
    }
    Err(ApiError::NotFound(
        Message::new(crate::messages::MODULE_NOT_ENABLED)
            .with("module", MessageArg::text(module.as_str().to_owned())),
    )
    .into_problem(locale, &crate::CATALOG))
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

/// One page of a list, and where the next one starts.
///
/// # Why every list is shaped like this
///
/// Because the alternative was a bare array that stopped at 200 and said
/// nothing. A tenant with 201 invoices saw 200 and had no way to tell — the
/// response looked exactly like a complete one, which is the worst shape a bug
/// can have.
///
/// `next` absent means **the list ended**. Present means pass it back as
/// `?after=` to continue.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct Paged<T> {
    pub items: Vec<T>,
    /// Pass back as `?after=` for the next page. Absent when there are no more.
    ///
    /// Opaque: what is in it is the query's business, and a client that parsed
    /// one would break the day a list is ordered differently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl<T> Paged<T> {
    /// Renders a page, mapping each row to its wire shape.
    pub fn of<R>(page: erp_types::Page<R>, view: impl Fn(R) -> T) -> Self {
        Self {
            next: page.next.map(|cursor| cursor.to_string()),
            items: page.items.into_iter().map(view).collect(),
        }
    }
}

/// How much of a list to return, and where to resume it.
#[derive(Debug, serde::Deserialize)]
pub struct After {
    #[serde(default)]
    pub after: Option<String>,
    /// How many rows. Absent takes the default; anything above the maximum is
    /// **clamped rather than refused**, because a caller asking for more than
    /// the server will give wants as much as it will give.
    #[serde(default)]
    pub limit: Option<i64>,
}

impl After {
    /// The page size, within what this API will serve.
    ///
    /// A limit below one is a request for nothing, which is never what anybody
    /// means — it comes from an uninitialised variable — so it takes the
    /// default too.
    #[must_use]
    pub fn limit(&self, default: i64, max: i64) -> i64 {
        match self.limit {
            Some(asked) if asked >= 1 => asked.min(max),
            _ => default,
        }
    }

    /// The cursor, or a 400 naming the parameter.
    ///
    /// A cursor this build cannot read is **refused**, not ignored: silently
    /// starting from the top would hand a caller the first page again and look
    /// like the list restarting.
    pub fn cursor(&self, locale: erp_i18n::Locale) -> Result<Option<erp_types::Cursor>, Problem> {
        self.after
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(|text| {
                erp_types::Cursor::decode(text).map_err(|_| {
                    bad_request(crate::messages::INVALID_CURSOR, "after", text, locale)
                })
            })
            .transpose()
    }
}
