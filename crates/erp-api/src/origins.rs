//! Which websites may call this tenant's public API, and proving they are the
//! tenant's to name.
//!
//! # Why a tenant cannot simply list an origin
//!
//! Because `https://salon.com` is not theirs to claim by typing it. The public
//! surface reads a business's diary and will take deposits; an allowlist a
//! tenant can write freely is an allowlist where one tenant names another
//! company's site — or their own competitor's — and receives requests a browser
//! believed were safe to send.
//!
//! So there are two steps and they are separate acts. A **domain** is claimed
//! and then proved, and only a proved domain licenses **origins**. Adding
//! `https://www.salon.com` after `https://salon.com` is then one row and not a
//! second proof, because the thing that was proved is the domain.
//!
//! # What proves it, and why that is not here
//!
//! Publishing a token where only the domain's owner could put it — a DNS `TXT`
//! record, or a file under `/.well-known/`. Both are **outbound network calls**,
//! and an outbound network call is an effect: a value in the outbox, delivered
//! by a handler (D9). This module records the claim and mints the token; a
//! handler checks the world and calls `verify_domain` when it comes back yes.
//!
//! Until that handler exists, `POST .../verification` is the operator's way to
//! say the check was made by hand — which is honest about where the work is,
//! and is why it is `ManageTenant` and audited rather than open.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::Actor;
use erp_i18n::Locale;
use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Json;
use erp_web::Problem;
use erp_web::{Allowed, Language, ManageTenant, Read};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_origins, allow_origin))
        .routes(routes!(revoke_origin))
        .routes(routes!(claim_domain))
        .routes(routes!(verify_domain))
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "domain": "salon.example" }))]
struct NewDomain {
    /// The registrable domain, with no scheme and no port.
    domain: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct DomainClaimed {
    domain: String,
    /// Publish this where only the domain's owner could — a `TXT` record on
    /// `_erp-verification.<domain>`.
    ///
    /// **The same token every time you ask.** A tenant who has already published
    /// one must not be told to publish a different one.
    verification_token: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "domain": "salon.example", "origin": "https://salon.example" }))]
struct NewOrigin {
    /// Which claimed domain licenses it.
    domain: String,
    /// Exactly what a browser sends in `Origin`: scheme, host, and a port only
    /// when it is not the scheme's default. It is echoed back verbatim, so a
    /// trailing slash or a path makes it a different string and it will not
    /// match.
    origin: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Origins {
    /// Every origin currently answered. An origin under a domain that is
    /// claimed but not yet proved is **not** in this list, because it is not
    /// answered.
    items: Vec<String>,
}

/// Which websites this business's public API answers.
#[utoipa::path(
    get,
    path = "/v1/origins",
    tag = "tenant",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    responses(
        (status = OK, body = Origins),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn list_origins(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Origins>, Problem> {
    let items = state
        .control
        .origins(tenant.db.tenant())
        .await
        .map_err(|e| access(e, locale))?;
    Ok(Json(Origins { items }))
}

/// Claim a domain, and get the token that proves it.
///
/// Claiming licenses nothing on its own. Asking twice returns the same token.
#[utoipa::path(
    post,
    path = "/v1/domains",
    tag = "tenant",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = NewDomain,
    responses(
        (status = CREATED, body = DomainClaimed),
        (status = BAD_REQUEST, description = "Not a domain, or one another tenant has already claimed", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn claim_domain(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewDomain>,
) -> Result<(StatusCode, Json<DomainClaimed>), Problem> {
    let domain = body.domain.trim().to_lowercase();

    // **The token is minted inside the control plane**, not here. A caller who
    // could choose it could choose a predictable one, and an attacker who can
    // predict the token a victim will be issued can publish it first.
    let token = state
        .control
        .claim_domain(
            tenant.db.tenant(),
            &domain,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| access(e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(DomainClaimed {
            domain,
            verification_token: token,
        }),
    ))
}

/// Record that a domain's ownership has been proved.
///
/// **This does not check anything**, and the doc comment on this module says
/// why: checking is an outbound call, and an outbound call is an outbox effect
/// with a handler that does not exist yet. Until it does, this is an operator
/// saying the check was made — audited as `tenant.domain_verified`.
#[utoipa::path(
    post,
    path = "/v1/domains/{domain}/verification",
    tag = "tenant",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("domain" = String, Path, description = "The claimed domain."),
    ),
    responses(
        (status = NO_CONTENT, description = "Proved. Already-proved is the same answer."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "This tenant has not claimed that domain", body = Problem),
    ),
)]
async fn verify_domain(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(domain): Path<String>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .verify_domain(
            tenant.db.tenant(),
            &domain,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| access(e, locale))?;

    // Already-verified answers the same as newly-verified: the caller wanted it
    // proved and it is. Reporting a conflict would make a retried request an
    // error for having succeeded the first time.
    Ok(StatusCode::NO_CONTENT)
}

/// License one origin under a domain this tenant has claimed.
#[utoipa::path(
    post,
    path = "/v1/origins",
    tag = "tenant",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = NewOrigin,
    responses(
        (status = NO_CONTENT, description = "Licensed. It answers once the domain is proved."),
        (status = BAD_REQUEST, description = "Not an origin, or a domain this tenant has not claimed", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn allow_origin(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewOrigin>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .allow_origin(
            tenant.db.tenant(),
            &body.domain,
            &body.origin,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| access(e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Stop answering one origin.
///
/// Takes effect at once on this node and within the entry cache's TTL across
/// the fleet — the same bound every other revocation here carries.
#[utoipa::path(
    delete,
    path = "/v1/origins/{origin}",
    tag = "tenant",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("origin" = String, Path, description = "The origin, URL-encoded."),
    ),
    responses(
        (status = NO_CONTENT, description = "Gone. One that was never there is the same answer."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn revoke_origin(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(origin): Path<String>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .revoke_origin(
            tenant.db.tenant(),
            &origin,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| access(e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

fn access(error: erp_control::AccessError, locale: Locale) -> Problem {
    ApiError::Access(error).into_problem(locale, &crate::CATALOG)
}
