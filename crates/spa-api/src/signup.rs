//! Self-service signup: from a form to a working system.
//!
//! The requirement: "anyone registering online can run their own system without
//! contacting us directly". This handler is thin on purpose — it validates the
//! request, names the modules, and hands the whole thing to
//! [`ControlPlane::sign_up`](spa_control::ControlPlane::sign_up), which does it
//! in one operation and compensates if any part fails.
//!
//! Thin is also what makes it compile. See `spa-control/src/provision.rs` on why
//! a chain of `async fn`s here cannot be proven `Send`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};
use spa_control::ModuleSetup;
use spa_i18n::Locale;
use spa_types::Timestamp;

use crate::error::ApiError;
use crate::extract::Language;
use crate::problem::Problem;
use crate::state::AppState;

/// The shortest password we will store.
///
/// Length is the only rule. Composition rules ("one number, one symbol") push
/// people toward `Password1!`; NIST dropped them in 2017.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/v1/signups", routing::post(sign_up))
}

#[derive(Debug, Deserialize)]
struct Signup {
    /// The tenant's name in URLs. 2–50 characters.
    slug: String,
    /// What the business is called.
    company: String,
    /// The first user's login.
    email: String,
    password: String,
    /// Modules to start with. Unknown names are refused rather than ignored, so
    /// a typo is not a silently missing feature discovered at month end.
    #[serde(default)]
    modules: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SignedUp {
    tenant: spa_types::TenantId,
    slug: String,
    /// Ready to use — signing up logs you in.
    token: String,
    expires_at: Timestamp,
    modules: Vec<String>,
}

async fn sign_up(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Signup>,
) -> Result<(StatusCode, Json<SignedUp>), Problem> {
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::BadRequest(
            spa_i18n::Message::new(crate::messages::PASSWORD_TOO_SHORT).with(
                "n",
                spa_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
            ),
        )
        .into_problem(locale));
    }

    let modules = parse_modules(&body.modules, locale)?;
    let names: Vec<String> = modules.iter().map(|m| m.module.to_string()).collect();

    let done = state
        .control
        .sign_up(body.email, body.password, body.slug, body.company, modules)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok((
        StatusCode::CREATED,
        Json(SignedUp {
            tenant: done.tenant.id,
            slug: done.tenant.slug,
            token: done.token.expose().to_owned(),
            expires_at: done.session.expires_at,
            modules: names,
        }),
    ))
}

/// Every module this build offers, as `(name, setup)`.
///
/// The list, in one place — because two things need it and they must not
/// disagree: this endpoint, which refuses anything not here, and the demo
/// tenant, which enables *all* of it. "The demo has every module enabled" is a
/// requirement nothing could check while the set was a `match` arm.
///
/// ponytail: still not a `Module` trait. A trait would also have to carry the
/// routes and the worker's jobs, and those cannot cross this boundary — a
/// module must not depend on `spa-api` or `spa-worker`. Each composition root
/// keeps listing what it composes; only the *set* is shared.
#[must_use]
pub fn modules() -> Vec<(&'static str, ModuleSetup)> {
    vec![("ledger", ledger::setup()), ("sales", sales::setup())]
}

/// Turns requested module names into their setup descriptions.
fn parse_modules(requested: &[String], locale: Locale) -> Result<Vec<ModuleSetup>, Problem> {
    let available = modules();
    let setups: Vec<ModuleSetup> = requested
        .iter()
        .map(|name| {
            available
                .iter()
                .find(|(known, _)| *known == name.as_str())
                // Cheap: `ModuleSetup` holds a name, a `&'static str` of SQL and
                // a slice of group names.
                .map(|(_, setup)| setup.clone())
                .ok_or_else(|| {
                    ApiError::BadRequest(
                        spa_i18n::Message::new(crate::messages::UNKNOWN_MODULE)
                            .with("module", spa_i18n::MessageArg::text(name.clone())),
                    )
                    .into_problem(locale)
                })
        })
        .collect::<Result<_, _>>()?;

    // Sales posts every invoice to the ledger, so a tenant that asked for one
    // without the other would get a system that refuses its first invoice.
    // Refused here rather than silently adding the ledger: what someone is
    // signing up for should be what they asked for.
    if requested.iter().any(|m| m == "sales") && !requested.iter().any(|m| m == "ledger") {
        return Err(ApiError::BadRequest(
            spa_i18n::Message::new(crate::messages::MODULE_REQUIRES)
                .with(
                    "module",
                    spa_i18n::MessageArg::text(sales::module_id().to_string()),
                )
                .with(
                    "required",
                    spa_i18n::MessageArg::text(sales::requires().to_string()),
                ),
        )
        .into_problem(locale));
    }

    Ok(setups)
}
