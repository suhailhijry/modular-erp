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
use erp_web::{Allowed, Authenticated, Language, Read};

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
        (name = "modules", description = "Which parts of the system this tenant has turned on."),
        (name = "ledger", description = "Accounts, journal entries, and the trial balance."),
        (name = "sales", description = "Invoices, payments, credit notes, and the VAT return."),
        (name = "purchases", description = "Supplier bills, what is owed, and the tax paid on them."),
        (name = "tax_sa", description = "Saudi Arabia: the VAT return, what has been filed, and ZATCA clearance and reporting."),
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
/// Generated here, so there is one list and it is the enum's.
fn name_the_roles(components: &mut utoipa::openapi::Components) {
    let listed = erp_control::Role::ALL
        .iter()
        .map(|role| format!("`{}`", role.as_str()))
        .collect::<Vec<_>>()
        .join(", ");

    for schema in components.schemas.values_mut() {
        let utoipa::openapi::RefOr::T(utoipa::openapi::Schema::Object(object)) = schema else {
            continue;
        };
        let Some(utoipa::openapi::RefOr::T(utoipa::openapi::Schema::Object(property))) =
            object.properties.get_mut("role")
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

fn takes_a_query(operation: &utoipa::openapi::path::Operation) -> bool {
    operation.parameters.as_ref().is_some_and(|params| {
        params
            .iter()
            .any(|p| p.parameter_in == utoipa::openapi::path::ParameterIn::Query)
    })
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

        for item in openapi.paths.paths.values_mut() {
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
                ];
                if operation.request_body.is_some() {
                    also.push((
                        "400",
                        "The body is not JSON. `code` is `request.malformed_body`, and `args.reason` says what the parser found.",
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
                if takes_a_query(operation) {
                    also.push((
                        "400",
                        "The query string is missing something or carries something unreadable. `code` is `request.invalid_query`.",
                    ));
                }
                for (status, description) in also {
                    operation
                        .responses
                        .responses
                        .entry(status.to_owned())
                        .or_insert_with(|| problem_response(description));
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

/// Every route, with the document that describes it.
fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(health))
        .routes(routes!(openapi_json))
        .routes(routes!(log_in))
        .routes(routes!(log_out))
        .routes(routes!(tenant))
        .merge(crate::signup::routes())
        .merge(crate::members::routes())
        .merge(crate::invitations::routes())
        .merge(crate::modules::routes())
        .merge(crate::origins::routes())
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

pub fn router(state: AppState) -> Router {
    parts()
        .0
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
}

#[derive(Debug, Serialize, ToSchema)]
struct SessionCreated {
    /// Send as `Authorization: Bearer <token>`.
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
        (status = UNAUTHORIZED, description = "Wrong handle or password — the same answer for both, deliberately", body = Problem),
    ),
)]
async fn log_in(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(credentials): Json<Credentials>,
) -> Result<impl IntoResponse, Problem> {
    let (token, session) = state
        .control
        .log_in(&credentials.handle, &credentials.password)
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;

    Ok((
        StatusCode::CREATED,
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
) -> Result<StatusCode, Problem> {
    state
        .control
        .log_out(&auth.token)
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
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
