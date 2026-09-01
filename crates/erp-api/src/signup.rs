//! Self-service signup: from a form to a working system, by way of a mailbox.
//!
//! The requirement is unchanged: "anyone registering online can run their own
//! system without contacting us directly". What changed is that it takes two
//! calls with an email in between, because the one-call version built a
//! database for anybody who could make an HTTP request. See
//! `erp-control/src/signup.rs` for what that cost and why nothing is built
//! until the address answers.
//!
//! Both handlers are thin on purpose — they validate the request, name the
//! modules, and hand the whole thing to the control plane, which does it in one
//! operation and compensates if any part fails.
//!
//! Thin is also what makes them compile. See `erp-control/src/provision.rs` on
//! why a chain of `async fn`s here cannot be proven `Send`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::{ModuleSetup, SignupError};
use erp_i18n::{Locale, Localize};
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
    OpenApiRouter::new()
        .routes(routes!(sign_up))
        .routes(routes!(confirm_signup))
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
    /// The first user's login. A confirmation goes here, and nothing is created
    /// until it comes back.
    email: String,
    password: String,
    /// Modules to start with. Unknown names are refused rather than ignored, so
    /// a typo is not a silently missing feature discovered at month end.
    #[serde(default)]
    modules: Vec<String>,
}

/// What a request produced, which is deliberately not much.
///
/// No token, and no identifier for the pending request. Both would be a way to
/// confirm without the mailbox, which is the whole thing this endpoint exists
/// to require. What comes back is what a page needs to say "check your email"
/// and to know when the link stops working.
#[derive(Debug, Serialize, ToSchema)]
struct SignupRequested {
    /// Where the confirmation went. Echoed back lowercased and trimmed, which
    /// is the form it was stored in.
    email: String,
    slug: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct SignedUp {
    #[schema(value_type = uuid::Uuid)]
    tenant: erp_types::TenantId,
    slug: String,
    /// Ready to use — confirming logs you in.
    token: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
    modules: Vec<String>,
}

/// Register a new tenant.
///
/// Sends a confirmation to the address and **creates nothing**. No account, no
/// company, no database: those are what `POST /v1/signups/{token}` builds, and
/// this endpoint is unauthenticated, so anything it built would be built for
/// anybody who could reach it.
///
/// An address that already has an account has to give that account's password.
/// Without it, naming somebody else's address would be a way to post mail
/// through us.
#[utoipa::path(
    post,
    path = "/v1/signups",
    tag = "signup",
    // Unauthenticated by definition: the point is to arrive without an account.
    security(),
    request_body = Signup,
    responses(
        (status = ACCEPTED, description = "A confirmation is on its way. Nothing exists yet.", body = SignupRequested),
        (status = BAD_REQUEST, description = "A password under 12 characters, or a module that does not exist", body = Problem),
        (status = UNAUTHORIZED, description = "The address already has an account and the password did not match it", body = Problem),
        (status = CONFLICT, description = "The slug is taken", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "A confirmation went to this address moments ago. Retryable, and the message says when.", body = Problem),
    ),
)]
async fn sign_up(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<Signup>,
) -> Result<(StatusCode, Json<SignupRequested>), Problem> {
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(short_password(locale));
    }

    let modules = parse_modules(&body.modules, locale)?;

    // **Where the link points is decided here**, because only this layer knows
    // the deployment's public domain. There is no tenant subdomain to hang it
    // on — that is the point, the tenant does not exist — so it goes on the
    // apex. The token is appended by `request_signup`, which is the only place
    // it exists in the clear.
    //
    // ponytail: it points at the API path, which answers with JSON. Same as the
    // invitation link, and the same one line changes when a frontend exists.
    let confirm_base = format!("https://{}/v1/signups/", state.domain);

    let (pending, _token) = state
        .control
        .request_signup(
            erp_control::SignupRequest {
                email: body.email,
                password: body.password,
                slug: body.slug,
                company: body.company,
                modules,
            },
            &confirm_base,
            locale,
        )
        .await
        .map_err(|e| signup_problem(&e, locale))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SignupRequested {
            email: pending.handle,
            slug: pending.slug,
            expires_at: pending.expires_at,
        }),
    ))
}

/// Confirm an address, and get the system that was asked for.
///
/// Creates the company, its database, its first owner, and a session — in one
/// operation that compensates if any part of it fails. The response is a
/// working bearer token: confirming logs you in.
///
/// There is deliberately no `GET` beside this. `/v1/join/{token}` has one
/// because whoever opens an invitation did not write it and has to be told what
/// they are joining; whoever opens this one filled the form in themselves, and
/// confirming does the thing they asked for.
#[utoipa::path(
    post,
    path = "/v1/signups/{token}",
    tag = "signup",
    security(),
    params(("token" = String, Path, description = "From the confirmation link.")),
    responses(
        (status = CREATED, body = SignedUp),
        (status = NOT_FOUND, description = "No such token, or a spent or expired one — the same answer for all three", body = Problem),
        (status = CONFLICT, description = "The slug was taken while the link sat in a mailbox. The link still works; ask for another name.", body = Problem),
    ),
)]
async fn confirm_signup(
    State(state): State<AppState>,
    Language(locale): Language,
    Path(token): Path<String>,
) -> Result<(StatusCode, Json<SignedUp>), Problem> {
    // **The modules come from the build, not from the stored row.**
    //
    // The row holds the names that were asked for; this turns them back into
    // the setups that install them, through the same lookup signup used. A
    // module withdrawn between the request and the click is therefore refused
    // here rather than installed from a description nothing offers any more.
    let stored = state
        .control
        .pending_signup_modules(&token)
        .await
        .map_err(|e| signup_problem(&e, locale))?;
    let modules = parse_modules(&stored, locale)?;

    let done = state
        .control
        .confirm_signup(&token, modules)
        .await
        .map_err(|e| signup_problem(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(SignedUp {
            tenant: done.tenant.id,
            slug: done.tenant.slug,
            token: done.token.expose().to_owned(),
            expires_at: done.session.expires_at,
            modules: stored,
        }),
    ))
}

fn short_password(locale: Locale) -> Problem {
    ApiError::BadRequest(
        erp_i18n::Message::new(erp_web::messages::PASSWORD_TOO_SHORT).with(
            "n",
            erp_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
        ),
    )
    .into_problem(locale, &crate::CATALOG)
}

/// Which failure is which, over HTTP.
fn signup_problem(error: &SignupError, locale: Locale) -> Problem {
    let status = match error {
        // 404, not 400: a bad token and a spent one are the same answer, and
        // that answer is "there is nothing here".
        SignupError::NotValid => StatusCode::NOT_FOUND,
        // 429, and the message carries the seconds. The only place in this API
        // that answers it, and it is not a rate limit — it is one address's
        // mail, capped. See the control plane's module docs.
        SignupError::TooSoon { .. } => StatusCode::TOO_MANY_REQUESTS,
        SignupError::Access(erp_control::AccessError::SlugTaken(_)) => StatusCode::CONFLICT,
        // The address has an account and the password was wrong. An ordinary
        // failed login, answered the way one is.
        SignupError::Auth(erp_control::AuthError::InvalidCredentials) => StatusCode::UNAUTHORIZED,
        // Capacity is an operator's problem described to a stranger, so it is a
        // 503 and the message says nothing about clusters.
        SignupError::Access(erp_control::AccessError::NoCapacity { .. }) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        other => {
            tracing::error!(error = %other, "signup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };

    let message = if status.is_server_error() {
        erp_i18n::Message::new(erp_control::messages::INTERNAL)
    } else {
        error.message()
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
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
