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

/// Turns requested module names into their setup descriptions.
///
/// The list and the dependency rule both live in [`crate::modules`], so signing
/// up for a module and enabling it later cannot disagree about either.
fn parse_modules(requested: &[String], locale: Locale) -> Result<Vec<ModuleSetup>, Problem> {
    let setups: Vec<ModuleSetup> = requested
        .iter()
        .map(|name| crate::modules::find(name, locale))
        .collect::<Result<_, _>>()?;

    // Checked against what was *asked for*, not against what exists: signing up
    // for sales without the ledger is refused at the door rather than producing
    // a system that fails on its first invoice.
    for setup in &setups {
        crate::modules::check_requirements(setup, requested, locale)?;
    }

    Ok(setups)
}
