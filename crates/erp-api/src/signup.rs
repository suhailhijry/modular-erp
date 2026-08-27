//! Self-service signup: from a form to a working system.
//!
//! The requirement: "anyone registering online can run their own system without
//! contacting us directly". This handler is thin on purpose — it validates the
//! request, names the modules, and hands the whole thing to
//! [`ControlPlane::sign_up`](erp_control::ControlPlane::sign_up), which does it
//! in one operation and compensates if any part fails.
//!
//! Thin is also what makes it compile. See `erp-control/src/provision.rs` on why
//! a chain of `async fn`s here cannot be proven `Send`.

use axum::extract::State;
use axum::http::StatusCode;
use erp_control::ModuleSetup;
use erp_i18n::Locale;
use erp_types::Timestamp;
use erp_web::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Language;
use erp_web::Problem;

/// The shortest password we will store.
///
/// Length is the only rule. Composition rules ("one number, one symbol") push
/// people toward `Password1!`; NIST dropped them in 2017.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(sign_up))
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "slug": "acme",
    "company": "Acme Trading Co.",
    "email": "owner@acme.example",
    "password": "correct horse battery staple",
    "modules": ["ledger", "sales"]
}))]
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

#[derive(Debug, Serialize, ToSchema)]
struct SignedUp {
    #[schema(value_type = uuid::Uuid)]
    tenant: erp_types::TenantId,
    slug: String,
    /// Ready to use — signing up logs you in.
    token: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
    modules: Vec<String>,
}

/// Register a new tenant.
///
/// Creates the company, its database, its first owner, and a session — in one
/// operation that compensates if any part of it fails. The response is a
/// working bearer token: signing up logs you in.
#[utoipa::path(
    post,
    path = "/v1/signups",
    tag = "signup",
    // Unauthenticated by definition: the point is to arrive without an account.
    security(),
    request_body = Signup,
    responses(
        (status = CREATED, body = SignedUp),
        (status = BAD_REQUEST, description = "A password under 12 characters, or a module that does not exist", body = Problem),
        (status = UNAUTHORIZED, description = "The address already has an account and the password did not match it", body = Problem),
        (status = CONFLICT, description = "The slug is taken", body = Problem),
    ),
)]
async fn sign_up(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Signup>,
) -> Result<(StatusCode, Json<SignedUp>), Problem> {
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::BadRequest(
            erp_i18n::Message::new(erp_web::messages::PASSWORD_TOO_SHORT).with(
                "n",
                erp_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
            ),
        )
        .into_problem(locale, &crate::CATALOG));
    }

    let modules = parse_modules(&body.modules, locale)?;
    let names: Vec<String> = modules.iter().map(|m| m.module.to_string()).collect();

    let done = state
        .control
        .sign_up(body.email, body.password, body.slug, body.company, modules)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;

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
        crate::modules::check_offered(setup, locale)?;
        crate::modules::check_requirements(setup, requested, locale)?;
    }

    Ok(setups)
}
