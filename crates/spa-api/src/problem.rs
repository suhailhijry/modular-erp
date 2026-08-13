//! Every failure, in one shape.
//!
//! RFC 9457 `application/problem+json`, plus two fields that make it usable by a
//! client that is not a person:
//!
//! - `code` — the stable [`MessageCode`]. **This is what a client branches on.**
//!   `detail` is prose in whatever language was asked for; branching on it
//!   breaks the first time a translator improves a sentence.
//! - `args` — the message's typed arguments, so a client can render its own
//!   sentence instead of ours.
//!
//! ```json
//! {
//!   "type":   "https://errors.example.com/auth.invalid_credentials",
//!   "title":  "Bad Request",
//!   "status": 400,
//!   "code":   "auth.invalid_credentials",
//!   "detail": "بيانات تسجيل الدخول غير صحيحة. يُرجى المحاولة مرة أخرى.",
//!   "args":   {}
//! }
//! ```

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use spa_i18n::{Catalog, Locale, Localize, Message};

/// Where a client can read about an error code.
const TYPE_PREFIX: &str = "https://errors.spa.example/";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Problem {
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: &'static str,
    pub status: u16,
    /// The stable identifier. Branch on this, never on `detail`.
    pub code: String,
    /// The message in the caller's language.
    pub detail: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub args: std::collections::BTreeMap<String, spa_i18n::MessageArg>,
}

impl Problem {
    /// Renders a localized message as a problem document.
    pub fn new(
        status: StatusCode,
        message: &Message,
        locale: Locale,
        catalog: &dyn Catalog,
    ) -> Self {
        Self {
            type_uri: format!("{TYPE_PREFIX}{}", message.code.as_str()),
            title: status.canonical_reason().unwrap_or("Error"),
            status: status.as_u16(),
            code: message.code.as_str().to_owned(),
            detail: catalog.render_or_code(locale, message),
            args: message.args.clone(),
        }
    }

    /// Renders any localizable error.
    pub fn from_error(
        status: StatusCode,
        error: &dyn Localize,
        locale: Locale,
        catalog: &dyn Catalog,
    ) -> Self {
        Self::new(status, &error.message(), locale, catalog)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            axum::Json(self),
        )
            .into_response()
    }
}
