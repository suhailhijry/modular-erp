//! The routes that exist today, and the document that describes them.
//!
//! # Why the router and the document are the same object
//!
//! [`OpenApiRouter`] registers an axum route *from* its handler's
//! `#[utoipa::path]` attribute, so the path and the method a client reads are
//! the path and the method the server answers on — the same string, not two
//! that agree today. A handler with no attribute does not compile inside
//! `routes!`, and one with an attribute that is never registered is dead code.
//! Neither half can grow a route the other does not have.
//!
//! Schemas come from the wire types by derive, so renaming a field renames it
//! in the document. What is left hand-written is the *response* declarations —
//! which status a handler answers with, and what it carries — and those are
//! checked against real responses by `tests/contract.rs`.

use std::sync::LazyLock;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use erp_web::Json;
use serde::{Deserialize, Serialize};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Anonymous, Authenticated, Language, Read};

/// Everything the router serves, as a description.
///
/// The paths are not listed here: they arrive from [`api_router`], which is the
/// same value the server runs. Listing them would be a second place to forget.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ERP",
        description = "\
A multi-tenant ERP. Every tenant has its own database; every write is an event; \
every read model is derived and can be rebuilt.

## Authentication

`POST /v1/sessions` returns a bearer token. Send it as `Authorization: Bearer <token>`. \
Operations documented without the `session` requirement are deliberately open — \
the module catalogue and the chart-of-accounts catalogue are product information \
a signup form needs before anyone has an account, and an invitation's token *is* \
its credential.

## Errors

Every failure is `application/problem+json` (RFC 9457) carrying a stable `code`. \
**Branch on the code, never on `detail`**, which is prose in whichever language \
the request asked for. `args` carries the values the message names, typed, so a \
client can render its own sentence. `docs/ERRORS.md` lists every code.

Two exceptions, and they are the honest kind: **413** (a body over 1 MB) and \
**504** (a request still running after 30 seconds) are refused at the edge, \
before anything in this document runs, and carry no body at all. Every operation \
below can answer either.

## Language

`Accept-Language` is honoured on every response, including failures. Arabic and \
English are first-class; anything else falls back to English rather than failing.

## Reading your own write

Read models are driven by a worker, so a read taken immediately after a write can \
legitimately not see it. Every write returns the log `position` it landed at; pass \
it back as `?consistent_after=<position>` and the read waits for the projection to \
reach it. Without it, reads are served as they are.

## Money

Minor units and an explicit currency, never a decimal string and never a float. \
`{ \"minor\": 1050, \"currency\": \"SAR\" }` is 10.50 SAR.",
        version = "0.1.0",
        license(name = "AGPL-3.0-or-later"),
    ),
    // The default, so a route that says nothing is documented as needing a
    // session. Forgetting must never be the permissive option — the same rule
    // the authorization extractors follow.
    security(("session" = [])),
    tags(
        (name = "signup", description = "Getting a system of your own."),
        (name = "sessions", description = "Logging in and out."),
        (name = "tenants", description = "The company you are working in."),
        (name = "members", description = "Who else has access, and as what."),
        (name = "invitations", description = "Inviting a colleague, and taking up an invitation."),
        (name = "audit", description = "What was done, by whom, and when: a tenant's trail for its owner, and a person's own."),
        (name = "modules", description = "Which parts of the system this tenant has turned on."),
        (name = "ledger", description = "Accounts, journal entries, and the trial balance."),
        (name = "sales", description = "Invoices, payments, credit notes, and the VAT return."),
        (name = "purchases", description = "Supplier bills, what is owed, and the tax paid on them."),
        (name = "tax_sa", description = "Saudi Arabia: the VAT return, what has been filed, and ZATCA clearance and reporting."),
        (name = "platform", description = "Running the platform rather than a company: platform staff, and what each of them may do. Every route needs a platform role that permits it and a second factor."),
        (name = "service", description = "Liveness and this document."),
    ),
)]
struct ApiDoc;

/// What every operation shares, applied once rather than repeated on each.
///
/// `Accept-Language`, the bearer scheme, and what each status means are
/// properties of *this API*, not of any one route. Declaring them per-handler
/// would mean thirty chances to leave one out, and the one left out would be
/// invisible.
///
/// Applied to the finished document rather than through utoipa's `modifiers`,
/// which run before any route is registered — there is nothing to walk yet at
/// that point, and a convention that silently reaches nothing is worse than one
/// that was never written.
struct Conventions;

/// What a status means here, for a response that did not say something more
/// specific. Every one of these is uniform across the API, which is why filling
/// them in one place is honest rather than a shortcut.
fn canonical_meaning(status: &str) -> &'static str {
    match status {
        "200" => "Done.",
        "201" => "Created.",
        "204" => "Done. No body.",
        "400" => {
            "The request was understood and asks for something impossible. `code` says which part."
        }
        "401" => "No session, or one that has expired. Log in again.",
        "403" => "Your role here does not permit this.",
        "404" => {
            "No such tenant, or not one of yours — the same answer for both, so this API is not a tenant-enumeration oracle."
        }
        "409" => "Somebody changed this first. Read it again and decide.",
        "422" => "Well formed, and refused on the state of what it names.",
        "503" => "Not serving right now rather than broken. Retryable.",
        _ => "See `code`.",
    }
}

/// Names the roles on every `role` field, from the enum rather than from prose.
///
/// `role` is a `String` on the wire so an unknown one gets a localized
/// `request.unknown_role` rather than a serde rejection — which leaves the list
/// a client reads as a doc comment, in eight places. The first version of this
/// document offered `manager` in three of them, which has never been a role.
///
/// Generated here, so there is one list and it is the enum's. `platform_role`
/// is the staff vocabulary, named apart so neither list can be read as the
/// other.
fn name_the_roles(components: &mut utoipa::openapi::Components) {
    let listed = |roles: &[&str]| {
        roles
            .iter()
            .map(|role| format!("`{role}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let vocabularies = [
        (
            "role",
            listed(&erp_control::Role::ALL.map(erp_control::Role::as_str)),
        ),
        (
            "platform_role",
            listed(&erp_control::PlatformRole::ALL.map(erp_control::PlatformRole::as_str)),
        ),
    ];

    for schema in components.schemas.values_mut() {
        let utoipa::openapi::RefOr::T(utoipa::openapi::Schema::Object(object)) = schema else {
            continue;
        };
        for (field, listed) in &vocabularies {
            let Some(utoipa::openapi::RefOr::T(utoipa::openapi::Schema::Object(property))) =
                object.properties.get_mut(*field)
            else {
                continue;
            };

            let one_of = format!("One of {listed}.");
            property.description = Some(match property.description.take() {
                Some(existing) if !existing.is_empty() => format!("{existing}\n\n{one_of}"),
                _ => one_of,
            });
        }
    }
}

/// A failure, in the one shape every failure has.
fn problem_response(description: &str) -> utoipa::openapi::RefOr<utoipa::openapi::Response> {
    utoipa::openapi::RefOr::T(
        utoipa::openapi::ResponseBuilder::new()
            .description(description)
            .content(
                "application/json",
                utoipa::openapi::ContentBuilder::new()
                    .schema(Some(utoipa::openapi::Ref::from_schema_name("Problem")))
                    .build(),
            )
            .build(),
    )
}

impl Modify for Conventions {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_default()
            .add_security_scheme(
                "session",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some(
                            "A token from `POST /v1/sessions`. Sessions expire; \
                         a 401 with `auth.session_expired` means log in again.",
                        ))
                        .build(),
                ),
            );

        name_the_roles(openapi.components.get_or_insert_default());

        let language = utoipa::openapi::path::ParameterBuilder::new()
            .name("Accept-Language")
            .parameter_in(utoipa::openapi::path::ParameterIn::Header)
            .description(Some(
                "`ar` or `en`. Anything else falls back to English. Honoured on \
                 errors too.",
            ))
            .required(utoipa::openapi::Required::False)
            .schema(Some(
                utoipa::openapi::ObjectBuilder::new()
                    .schema_type(utoipa::openapi::schema::SchemaType::Type(
                        utoipa::openapi::Type::String,
                    ))
                    .examples(["ar"]),
            ))
            .build();

        let read_models = crate::modules::read_models();

        for (path, item) in &mut openapi.paths.paths {
            let rebuilt = served_from_read_models(path, &read_models);
            // Path-level, so it covers every method on the path and any added
            // later.
            item.parameters
                .get_or_insert_with(Vec::new)
                .push(language.clone());

            let operations = [
                item.get.as_mut(),
                item.put.as_mut(),
                item.post.as_mut(),
                item.delete.as_mut(),
                item.patch.as_mut(),
                item.head.as_mut(),
                item.options.as_mut(),
                item.trace.as_mut(),
            ];
            for operation in operations.into_iter().flatten() {
                // **One sentence about `Host`, everywhere.** Routes were written
                // when the tenant was always a subdomain; now a proved custom
                // domain reaches the tenant too, and the document should say so
                // once rather than in a hundred places.
                if let Some(parameters) = operation.parameters.as_mut() {
                    for parameter in parameters.iter_mut() {
                        if parameter.name.eq_ignore_ascii_case("host") {
                            parameter.description = Some(HOST_DOC.to_owned());
                        }
                    }
                }
                // What every operation can answer regardless of what it does.
                // Written here rather than on each handler because they are
                // uniform, and because the ones a handler is least likely to
                // remember are exactly these.
                let mut also = vec![
                    // Ours. A client's only move is to retry or report it.
                    (
                        "500",
                        "Something went wrong here. `code` is `system.internal_error`, and nothing more specific is safe to infer.",
                    ),
                    // **Every route, because the check is a layer.** A client
                    // declaring a version this build does not serve is refused
                    // before the handler is reached, so a 400 is reachable on a
                    // route that takes nothing at all.
                    (
                        "400",
                        "`x-api-version` names a version this API does not serve — `code` is `request.api_version_too_old` or `request.api_version_too_new`, and `args` carries the range. Where a route takes a body or a query string, a 400 may also mean that could not be read: see `code`.",
                    ),
                ];
                if operation.request_body.is_some() {
                    also.push((
                        "413",
                        "The body is larger than this route takes. Every route is capped; an upload is capped higher than the rest.",
                    ));
                    also.push((
                        "415",
                        "The body was sent without `Content-Type: application/json`.",
                    ));
                    also.push((
                        "422",
                        "The body is JSON and not the shape this route takes. `args.reason` names the field.",
                    ));
                }

                for (status, description) in also {
                    operation
                        .responses
                        .responses
                        .entry(status.to_owned())
                        .or_insert_with(|| problem_response(description));
                }

                if rebuilt {
                    may_be_rebuilding(&mut operation.responses);
                }

                for (status, response) in &mut operation.responses.responses {
                    if let utoipa::openapi::RefOr::T(response) = response
                        && response.description.is_empty()
                    {
                        canonical_meaning(status).clone_into(&mut response.description);
                    }
                }
            }
        }
    }
}

/// Whether `path` is `/v1/{module}` or under it, for a module with read
/// models — the module `erp_web`'s `module_of` would find, and so the routes
/// its `read_models_current` may refuse.
fn served_from_read_models(
    path: &str,
    read_models: &std::collections::HashMap<erp_types::ModuleId, Vec<(&'static str, i16)>>,
) -> bool {
    path.split('/')
        .nth(2)
        .and_then(|segment| erp_types::ModuleId::new(segment).ok())
        .is_some_and(|module| read_models.contains_key(&module))
}

/// **Every route of a module with read models may answer 503 while one is
/// rebuilt**, because the refusal is in the extractor, before the handler. Said
/// once here, beside whatever else that route's 503 already means.
fn may_be_rebuilding(responses: &mut utoipa::openapi::Responses) {
    match responses.responses.get_mut("503") {
        Some(utoipa::openapi::RefOr::T(response)) => {
            let said = if response.description.is_empty() {
                canonical_meaning("503")
            } else {
                &response.description
            };
            response.description = format!("{said} Or {READ_MODEL_REBUILDING}");
        }
        Some(utoipa::openapi::RefOr::Ref(_)) => {}
        None => {
            responses.responses.insert(
                "503".to_owned(),
                problem_response(&format!(
                    "Not serving right now: `code` is {READ_MODEL_REBUILDING}"
                )),
            );
        }
    }
}

/// What a module route's 503 adds, on every one of them.
const READ_MODEL_REBUILDING: &str = "`request.read_model_rebuilding` while a read model this \
module is served from was built by an older release and has not been rebuilt yet — nothing is \
shown from tables worked out by rules this release has replaced. Retryable.";

/// What every `Host` header parameter says.
const HOST_DOC: &str = "The tenant's host: its subdomain of the platform domain — `bassat.erp.com` — \
or any host under a domain it has proved (`POST /v1/domains`, then the DNS record, then \
`POST /v1/domains/{domain}/verification`), such as `api.bassat.sa`. Every path is about that tenant.";

/// Every route, with the document that describes it.
fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .routes(routes!(openapi_json))
        .routes(routes!(log_in))
        .routes(routes!(log_out))
        .routes(routes!(
            second_factor,
            begin_second_factor,
            disable_second_factor
        ))
        .routes(routes!(confirm_second_factor))
        .routes(routes!(tenant))
        .merge(crate::passwords::routes())
        .merge(crate::audit::routes())
        .merge(crate::platform::routes())
        .merge(crate::signup::routes())
        .merge(crate::members::routes())
        .merge(crate::invitations::routes())
        .merge(crate::modules::routes())
        .merge(crate::origins::routes())
        .merge(crate::links::routes())
        .merge(crate::keys::routes())
        .merge(crate::hooks::routes())
        .merge(crate::codes::routes())
        .merge(crate::deposits::routes())
        .merge(crate::billing::routes())
        .merge(crate::effects::routes())
        .merge(crate::calendar::routes())
        .merge(crate::permission_limits::routes())
        .merge(crate::realtime::routes())
        // Every module's own routes, from the one list that also says what to
        // install. See `crate::modules::REGISTERED`.
        .merge(crate::modules::mounted())
}

/// The router and the document, from the one description of both.
fn parts() -> (Router<AppState>, utoipa::openapi::OpenApi) {
    let (router, mut document) = api_router().split_for_parts();
    Conventions.modify(&mut document);
    (router, document)
}

/// What every route that is not a file upload may take.
///
/// **Part of the API, not of a deployment.** It was in `bin/api.rs`, which
/// meant a second binary — the test harness, for one — silently had axum's own
/// default instead. `modules/files` raises it for its two upload routes and
/// nothing else does; axum's `DefaultBodyLimit` is innermost-wins, so a module's
/// own layer beats this one exactly where it is applied.
const MAX_JSON_BODY: usize = 1 << 20;

pub fn router(mut state: AppState) -> Router {
    // **What this build projects, from the list that mounts the routes**, so
    // no server built from here can refuse a stale read model by one list and
    // serve modules from another. See `erp_web::AppState::read_models`.
    state.read_models = std::sync::Arc::new(crate::modules::read_models());
    parts()
        .0
        // **Every answer is problem+json, including "there is no such
        // route".** axum's defaults are a bare 404 and 405 with no body; a
        // client that reads `code` from every refusal would read nothing from
        // these two, and the contract test could not see them because they are
        // on no route.
        .fallback(no_such_route)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_JSON_BODY))
        // **Every list, including the ones that do not exist yet.** An export
        // is the same query with a different encoder, so it is a layer rather
        // than something each handler has to remember — see `erp_web::csv`.
        .layer(axum::middleware::from_fn(erp_web::csv::layer))
        // **Outermost of the three**, so a client outside the range is refused
        // before anything reads its body — and so every response, refusals
        // included, carries what this build serves.
        .layer(axum::middleware::from_fn(erp_web::version::layer))
        // **Outermost, so a preflight never reaches a handler and a refusal
        // never reaches one either.** Per tenant and asynchronous, which is why
        // it is written here rather than configured from `tower-http` — see
        // `erp_web::cors`.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            erp_web::cors::layer,
        ))
        .with_state(state)
}

async fn no_such_route(Language(locale): Language) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(erp_web::messages::NO_SUCH_ROUTE),
        locale,
        &crate::CATALOG,
    )
}

async fn method_not_allowed(Language(locale): Language) -> Problem {
    Problem::new(
        StatusCode::METHOD_NOT_ALLOWED,
        &erp_i18n::Message::new(erp_web::messages::METHOD_NOT_ALLOWED),
        locale,
        &crate::CATALOG,
    )
}

/// The document, for anything that wants it without running a server.
///
/// `tests/openapi.rs` writes it to `docs/openapi.json` and fails when the two
/// disagree.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    parts().1
}

/// Built once. Forced by the handler rather than by [`api_router`], so building
/// a router does not pay for serializing a document nobody asked for.
static DOCUMENT: LazyLock<serde_json::Value> =
    LazyLock::new(|| serde_json::to_value(openapi()).unwrap_or(serde_json::Value::Null));

/// This document.
///
/// Point any `OpenAPI` viewer at it. Deliberately not a bundled UI: the bundles
/// are megabytes of vendored assets fetched at build time, which is a network
/// dependency in a build that otherwise has none.
#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    tag = "service",
    security(),
    responses((status = OK, description = "This document, as OpenAPI 3.1", content_type = "application/json")),
)]
async fn openapi_json() -> Json<&'static serde_json::Value> {
    Json(&DOCUMENT)
}

/// Liveness.
///
/// Deliberately does not touch the database — a health check that fails when
/// the database is slow takes the fleet out during a slow query.
#[utoipa::path(
    get,
    path = "/v1/health",
    tag = "service",
    security(),
    responses((status = OK, body = Health)),
)]
async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(Health { status: "ok" }))
}

#[derive(Debug, Serialize, ToSchema)]
struct Health {
    status: &'static str,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "handle": "owner@acme.example", "password": "correct horse battery staple" }))]
struct Credentials {
    /// The email address the account was registered with.
    handle: String,
    password: String,
    /// **The six digits from an authenticator app, or a recovery code.**
    ///
    /// Only needed by an account that has enrolled a second factor. Sending one
    /// that has not is harmless and is ignored, so a client may always ask for
    /// it rather than first discovering which accounts are enrolled — which
    /// would be an enumeration oracle.
    ///
    /// Omitting it for an account that needs one answers `401` with code
    /// `auth.second_factor_required`, which is the signal to ask and retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct SessionCreated {
    /// Send as `Authorization: Bearer <token>`.
    ///
    /// **Also set as an `HttpOnly` cookie**, and they are the same session. A
    /// browser can ignore this field.
    token: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: erp_types::Timestamp,
}

/// Log in.
#[utoipa::path(
    post,
    path = "/v1/sessions",
    tag = "sessions",
    security(),
    request_body = Credentials,
    responses(
        (status = CREATED, body = SessionCreated),
        (status = UNAUTHORIZED, description = "Wrong handle or password — the same answer for both, deliberately. Also the answer when the password was right and this account needs its second factor: `code` is then `auth.second_factor_required`.", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "Too many attempts from this address, or against this account. `args.seconds` says how long to wait.", body = Problem),
    ),
)]
async fn log_in(
    anonymous: Anonymous,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(credentials): Json<Credentials>,
) -> Result<impl IntoResponse, Problem> {
    // **Before the hash.** The per-address budget was charged by the extractor;
    // this is the per-account one, so a guess spread over many addresses still
    // runs out. Refusing here costs the limiter a lookup and Argon2 nothing.
    anonymous
        .charge_for_handle(&state, &credentials.handle)
        .await?;
    // **Both factors, or neither.** `log_in` refuses an enrolled account
    // outright, so a client that never sends a code cannot get past one; a
    // client that always sends one works either way.
    let signed_in = match credentials.code.as_deref() {
        Some(code) => {
            let sealing = sealing_key(&state, locale)?;
            state
                .control
                .log_in_with_second_factor(
                    &credentials.handle,
                    &credentials.password,
                    code,
                    chrono::Utc::now(),
                    sealing,
                )
                .await
        }
        None => {
            state
                .control
                .log_in(&credentials.handle, &credentials.password)
                .await
        }
    };
    let (token, session) =
        signed_in.map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

    // **The cookie as well as the body**, and they are the same session — see
    // `crate::codes::session_cookie`. A browser can ignore the token entirely.
    Ok((
        StatusCode::CREATED,
        [(
            axum::http::header::SET_COOKIE,
            crate::codes::session_cookie(&token),
        )],
        Json(SessionCreated {
            token: token.expose().to_owned(),
            expires_at: session.expires_at,
        }),
    ))
}

/// Log out, ending the session this request authenticated with.
#[utoipa::path(
    delete,
    path = "/v1/sessions/current",
    tag = "sessions",
    responses(
        (status = NO_CONTENT, description = "Ended. The token no longer works."),
        (status = UNAUTHORIZED, body = Problem),
    ),
)]
async fn log_out(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
) -> Result<impl IntoResponse, Problem> {
    state
        .control
        .log_out(&auth.token)
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

    // **The cookie goes too.** The session row is gone either way, so leaving
    // the cookie behind costs nothing but a confusing round trip — a browser
    // that still holds one gets a 401 on its next request and no explanation.
    Ok((
        StatusCode::NO_CONTENT,
        [(
            axum::http::header::SET_COOKIE,
            crate::codes::cleared_cookie(),
        )],
    ))
}

// ---------------------------------------------------------------------------
// The second factor
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct SecondFactorView {
    /// Whether this account needs a code to sign in.
    enrolled: bool,
    /// Recovery codes not yet spent. **Zero with `enrolled` true is a person
    /// one lost phone away from locked out**, which is worth a warning on a
    /// screen: enrolling again issues a fresh set.
    recovery_codes_left: i64,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "uri": "otpauth://totp/Acme:sara%40acme.test?secret=JBSWY3DPEHPK3PXP&issuer=Acme&algorithm=SHA1&digits=6&period=30",
    "secret": "JBSWY3DPEHPK3PXP"
}))]
struct EnrolmentStarted {
    /// Render as a QR code. Every authenticator app reads this.
    uri: String,
    /// The same secret, for typing in when a camera will not co-operate.
    secret: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "code": "123456" }))]
struct SecondFactorCode {
    /// Six digits from the app.
    code: String,
    /// **Only when replacing a factor you already have**: a code from the *old*
    /// one, or one of its recovery codes. Replacing an enrolment destroys it
    /// and all ten recovery codes, so it asks for proof of the thing it is
    /// about to destroy. A `401 auth.second_factor_required` means send this
    /// too.
    ///
    /// Not needed for a first enrolment, which removes nothing.
    #[serde(default)]
    previous: Option<String>,
    /// **Only after somebody else reset your two-step sign-in**: the token from
    /// the email you were sent. A `403 auth.enrolment_link_required` means send
    /// it. Confirming spends it.
    #[serde(default)]
    link: Option<String>,
}

/// What starting an enrolment costs an account whose factor somebody reset.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[schema(example = json!({ "link": "…" }))]
struct BeginSecondFactor {
    /// The token from the enrolment email, **sent in the body rather than the
    /// path** for the reason a reset token is: a path lands in access logs,
    /// browser history and the `Referer` of every asset the page loads.
    ///
    /// Needed only by an account somebody else's reset left link-only. Send
    /// `{}` otherwise.
    #[serde(default)]
    link: Option<String>,
}

/// What turning a second factor off costs: proof that you hold it.
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "code": "123456" }))]
struct DisableSecondFactor {
    /// A code from the app, or one of the recovery codes. Absent only when
    /// there is nothing enrolled to prove — abandoning an enrolment that was
    /// started and never confirmed.
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct RecoveryCodes {
    /// **Shown once and never again.** The server keeps only their digests, so
    /// this list cannot be produced a second time — enrolling again is the only
    /// way to get a new one, and it invalidates these.
    recovery_codes: Vec<String>,
}

/// Whether this account has a second factor.
#[utoipa::path(
    get,
    path = "/v1/sessions/second-factor",
    tag = "sessions",
    responses(
        (status = OK, body = SecondFactorView),
        (status = UNAUTHORIZED, body = Problem),
    ),
)]
async fn second_factor(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
) -> Result<Json<SecondFactorView>, Problem> {
    let identity = auth.session.identity;
    let problem =
        |e: erp_control::AuthError| ApiError::Auth(e).into_problem(locale, &crate::CATALOG);
    Ok(Json(SecondFactorView {
        enrolled: state
            .control
            .has_second_factor(identity)
            .await
            .map_err(problem)?,
        recovery_codes_left: state
            .control
            .recovery_codes_left(identity)
            .await
            .map_err(problem)?,
    }))
}

/// Start enrolling an authenticator app.
///
/// **Nothing changes about signing in until it is confirmed.** Somebody who
/// scans the QR and walks away is not locked out.
///
/// **Send `link` if somebody else reset your two-step sign-in.** From then on a
/// password alone cannot set up a new app — and that does not lapse when the
/// link does, so an expired one means asking for a fresh link rather than
/// waiting. Starting an enrolment does not spend it; confirming does.
#[utoipa::path(
    post,
    path = "/v1/sessions/second-factor",
    tag = "sessions",
    request_body = BeginSecondFactor,
    responses(
        (status = CREATED, body = EnrolmentStarted),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Somebody else reset this account's second factor, so only the link it was emailed may enrol the next one — `auth.enrolment_link_required`. The same answer for a missing, wrong, spent and expired link.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key, so there is nowhere safe to keep the secret", body = Problem),
    ),
)]
async fn begin_second_factor(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
    // **Optional, because this route had no body before the link existed.**
    // A client that sends none is an ordinary first enrolment or replacement.
    body: Option<Json<BeginSecondFactor>>,
) -> Result<impl IntoResponse, Problem> {
    let link = body.and_then(|Json(body)| body.link);
    let sealing = sealing_key(&state, locale)?;
    let enrolment = state
        .control
        .begin_second_factor(
            auth.session.identity,
            "ERP",
            &auth.session.identity.to_string(),
            sealing,
            link.as_deref(),
        )
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;
    Ok((
        StatusCode::CREATED,
        Json(EnrolmentStarted {
            uri: enrolment.uri,
            secret: enrolment.secret,
        }),
    ))
}

/// Confirm an enrolment with the first code the app shows.
///
/// Returns the recovery codes, **once**. **Every other session of this identity
/// ends** — the one confirming stays — because a session from before the
/// factor existed never went through it.
///
/// **Send `link` if somebody else reset your two-step sign-in.** Confirming
/// spends it, and every other link the account was sent, and puts the account
/// back to ordinary rules.
#[utoipa::path(
    post,
    path = "/v1/sessions/second-factor/confirmation",
    tag = "sessions",
    request_body = SecondFactorCode,
    responses(
        (status = CREATED, description = "Enrolled. Keep the recovery codes — they are not shown again.", body = RecoveryCodes),
        (status = UNAUTHORIZED, description = "The code is wrong, nothing is waiting to be confirmed, or a factor is already enrolled and `previous` did not prove it. `auth.second_factor_required` means send `previous`.", body = Problem),
        (status = FORBIDDEN, description = "Somebody else reset this account's second factor and `link` was missing, wrong, spent or expired — `auth.enrolment_link_required`", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn confirm_second_factor(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
    Json(body): Json<SecondFactorCode>,
) -> Result<impl IntoResponse, Problem> {
    let sealing = sealing_key(&state, locale)?;
    let confirmed = state
        .control
        .confirm_second_factor(
            auth.session.identity,
            &body.code,
            body.previous.as_deref(),
            chrono::Utc::now(),
            sealing,
            Some(&auth.token),
            body.link.as_deref(),
        )
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;
    Ok((
        StatusCode::CREATED,
        Json(RecoveryCodes {
            recovery_codes: confirmed.recovery_codes,
        }),
    ))
}

/// Turn the second factor off, taking the recovery codes with it.
///
/// **Send a code from the app, or one of the recovery codes.** A session used
/// to be enough, which made a stolen one all it took to strip the control the
/// theft was supposed to run into — and the factor, unlike the password, is
/// the one thing somebody who worked their way to a session does not have.
///
/// Absence fails closed: no code with a factor enrolled is
/// `401 auth.second_factor_required`, never a removal. Send `{}` when there is
/// nothing enrolled — an enrolment started and never confirmed.
///
/// **Refused outright when a second factor is required of the account** —
/// platform staff, or a live member of an organisation that requires one.
/// Replacing it is still open.
#[utoipa::path(
    delete,
    path = "/v1/sessions/second-factor",
    tag = "sessions",
    request_body = DisableSecondFactor,
    responses(
        (status = NO_CONTENT, description = "Off. The password is the whole login again."),
        (status = UNAUTHORIZED, description = "No code came, or it was wrong. `auth.second_factor_required` means send one.", body = Problem),
        (status = FORBIDDEN, description = "A second factor is required of this account, so it cannot be turned off, only replaced. `auth.staff_keeps_second_factor`: platform staff — dropping it means coming off the staff first. `auth.tenant_keeps_second_factor`: a live member of an organisation that requires one — dropping it takes its owner removing them (`DELETE /v1/members/{identity}`) or no longer requiring it; a member cannot leave on their own. Refused before the code is checked, so a recovery code sent here is not spent.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key, so a code cannot be checked", body = Problem),
    ),
)]
async fn disable_second_factor(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
    Json(body): Json<DisableSecondFactor>,
) -> Result<StatusCode, Problem> {
    let sealing = sealing_key(&state, locale)?;
    // **No code is not a shortcut**: an identity with a factor still has to
    // prove it, and one with only an unconfirmed enrolment has nothing to
    // prove.
    let code = body.code;
    state
        .control
        .disable_second_factor(
            auth.session.identity,
            code.as_deref(),
            chrono::Utc::now(),
            sealing,
        )
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The deployment's sealing key, or a refusal. **Not a degraded mode**: without
/// it there is nowhere safe to keep a shared secret, and keeping one in the
/// clear because an environment variable is missing is the "log a warning and
/// continue" this system does not do (L6).
pub(crate) fn sealing_key(
    state: &AppState,
    locale: erp_i18n::Locale,
) -> Result<&erp_eventlog::SealingKey, Problem> {
    state.sealing.as_ref().ok_or_else(|| {
        erp_web::Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(erp_web::messages::NO_SEALING_KEY),
            locale,
            &crate::CATALOG,
        )
    })
}

#[derive(Debug, Serialize, ToSchema)]
struct TenantView {
    #[schema(value_type = uuid::Uuid)]
    id: erp_types::TenantId,
    /// What the caller may do here, so a client can hide what it must not
    /// offer. The server refuses regardless — this is for the buttons.
    role: Option<&'static str>,
    /// The modules this tenant has turned on. A route belonging to any other
    /// module answers 404 here.
    modules: Vec<String>,
}

/// What this tenant is, and what you may do in it.
///
/// Proves the whole path: `Allowed<Read>` in the signature means membership was
/// checked *and* the role permits it, and there is no way to obtain one that
/// skips either.
#[utoipa::path(
    get,
    path = "/v1/tenant",
    tag = "tenants",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = TenantView),
        (status = UNAUTHORIZED, body = Problem),
        (status = NOT_FOUND, description = "No such tenant, or not one of yours — the same answer for both, so the API is not a tenant-enumeration oracle", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "The tenant is real and entitled, and not serving right now. Retryable.", body = Problem),
    ),
)]
async fn tenant(tenant: Allowed<Read>) -> Json<TenantView> {
    Json(TenantView {
        id: tenant.db.tenant(),
        role: tenant.db.role().map(erp_control::Role::as_str),
        modules: tenant
            .db
            .modules()
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
    })
}
