//! Issuing, listing, rotating and revoking API keys.
//!
//! Control-plane routes, like members and modules: a key is a way into a
//! tenant, and what a tenant *is* lives in the control plane.
//!
//! # The secret is in exactly one response
//!
//! Issuing and rotating return it; nothing else ever does, and it is not stored
//! in a form anything can read back. A key that can be re-read is a key with as
//! many copies as there are people who can read it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::{Actor, ApiKey, KeyScope, ROTATION_OVERLAP, Role};
use erp_i18n::{Locale, Localize};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Json, Language, ManageTenant, bad_request};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_keys, issue_key))
        .routes(routes!(revoke_key))
        .routes(routes!(rotate_key))
}

#[derive(Debug, Serialize, ToSchema)]
struct KeyRecord {
    id: String,
    /// **Safe anywhere.** It identifies an integration and proves nothing.
    public_key: String,
    name: String,
    /// `module:capability`, or `*:capability`.
    scopes: Vec<String>,
    role: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    created_at: Timestamp,
    /// Best effort, so it can be a little behind. Enough to answer "is anything
    /// still using this" before somebody revokes it.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    last_used_at: Option<Timestamp>,
    /// Set by a rotation on the key being replaced. This is the overlap window.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    expires_at: Option<Timestamp>,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    revoked_at: Option<Timestamp>,
    revoked_why: Option<String>,
    /// The key this one replaced, when it came from a rotation.
    rotated_from: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct KeyIssued {
    #[serde(flatten)]
    key: KeyRecord,
    /// **Shown once.** There is no way to read it again, by design.
    secret: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "Booking widget",
    "scopes": ["booking:read", "crm:read"],
    "role": "viewer"
}))]
struct NewKey {
    /// What a person calls it, so revoking the right one does not need a guess.
    name: String,
    /// What it may do. `booking:read`, `*:read`, `ledger:post_entries`.
    scopes: Vec<String>,
    /// The role its machine identity holds — `owner`, `accountant`, `clerk` or
    /// `viewer`.
    ///
    /// **The scopes can only narrow this.** A key given a viewer's role and a
    /// scope that permits posting still cannot post, because the role is
    /// checked as well.
    role: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Revocation {
    /// Why. Kept on the record, because "which of these did we revoke after the
    /// incident, and why" is the question afterwards.
    why: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Rotation {
    /// How long the old key keeps working. Omitted is seven days.
    ///
    /// **Zero is legitimate** and means "stop now" — for a key that is known to
    /// have leaked, where the overlap is the thing you do not want.
    #[serde(default)]
    overlap_seconds: Option<u64>,
}

/// Every key this tenant has, newest first.
///
/// Revoked ones included: "what did we revoke and when" is the question after an
/// incident, and a list that hides them cannot answer it.
#[utoipa::path(
    get,
    path = "/v1/keys",
    tag = "keys",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = Vec<KeyRecord>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_keys(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Vec<KeyRecord>>, Problem> {
    let keys = state
        .control
        .keys(tenant.db.tenant())
        .await
        .map_err(|e| refused(&e, locale))?;

    Ok(Json(keys.into_iter().map(record).collect()))
}

/// Issue one. **The secret is in this response and nowhere else.**
#[utoipa::path(
    post,
    path = "/v1/keys",
    tag = "keys",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    request_body = NewKey,
    responses(
        (status = CREATED, description = "The key, with its secret. Store it now; it cannot be read again.", body = KeyIssued),
        (status = BAD_REQUEST, description = "Not a scope, not a role, or no scopes at all", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn issue_key(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewKey>,
) -> Result<(StatusCode, Json<KeyIssued>), Problem> {
    let (scopes, role) = wanted(&body, locale)?;

    let (key, secret) = state
        .control
        .issue_key(
            tenant.db.tenant(),
            &body.name,
            &scopes,
            role,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| refused(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(KeyIssued {
            key: record(key),
            secret: secret.expose().to_owned(),
        }),
    ))
}

/// Stop one working, now.
#[utoipa::path(
    delete,
    path = "/v1/keys/{key}",
    tag = "keys",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("key" = String, Path, description = "The key's id — not its public key."),
    ),
    request_body = Revocation,
    responses(
        (status = NO_CONTENT, description = "Revoked, or was already."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No key by that id here", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn revoke_key(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Revocation>,
) -> Result<StatusCode, Problem> {
    let id = key_id(&id, locale)?;
    let revoked = state
        .control
        .revoke_key(
            tenant.db.tenant(),
            id,
            &body.why,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| refused(&e, locale))?;

    // **Already revoked is `204`, not `404`.** The caller wanted it off and it
    // is off, and a retry of a revocation must never read as "no such key".
    if !revoked && !exists(&state, &tenant, id).await? {
        return Err(no_such_key(&id.to_string(), locale));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Replace one, with an overlap.
///
/// **The overlap is the point.** The old key keeps working while the
/// integration holding it is redeployed on its own schedule — because a key
/// that cannot be rotated without downtime is a key nobody rotates.
///
/// The replacement carries the same scopes and role. Changing what a key may do
/// is a different act; folding it into a rotation is how an integration comes
/// back with permissions nobody chose.
#[utoipa::path(
    post,
    path = "/v1/keys/{key}/rotation",
    tag = "keys",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("key" = String, Path, description = "The key being replaced."),
    ),
    request_body = Rotation,
    responses(
        (status = CREATED, description = "The replacement, with its secret. The old key stops at `expires_at`.", body = KeyIssued),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No key by that id here, or it is revoked", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn rotate_key(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Rotation>,
) -> Result<(StatusCode, Json<KeyIssued>), Problem> {
    let id = key_id(&id, locale)?;
    let overlap = body
        .overlap_seconds
        .map_or(ROTATION_OVERLAP, std::time::Duration::from_secs);

    let issued = state
        .control
        .rotate_key(
            tenant.db.tenant(),
            id,
            overlap,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| refused(&e, locale))?;

    let Some((key, secret)) = issued else {
        return Err(no_such_key(&id.to_string(), locale));
    };

    Ok((
        StatusCode::CREATED,
        Json(KeyIssued {
            key: record(key),
            secret: secret.expose().to_owned(),
        }),
    ))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn record(key: ApiKey) -> KeyRecord {
    KeyRecord {
        id: key.id.to_string(),
        public_key: key.public_key,
        name: key.name,
        scopes: key.scopes.iter().map(KeyScope::as_string).collect(),
        role: key.role.as_str().to_owned(),
        created_at: key.created_at,
        last_used_at: key.last_used_at,
        expires_at: key.expires_at,
        revoked_at: key.revoked_at,
        revoked_why: key.revoked_why,
        rotated_from: key.rotated_from.map(|id| id.to_string()),
    }
}

fn wanted(body: &NewKey, locale: Locale) -> Result<(Vec<KeyScope>, Role), Problem> {
    if body.scopes.is_empty() {
        return Err(bad_request(
            erp_control::messages::NOT_A_SCOPE,
            "scope",
            "",
            locale,
        ));
    }
    let scopes = body
        .scopes
        .iter()
        .map(|raw| KeyScope::parse(raw))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &e.message(),
                locale,
                &crate::CATALOG,
            )
        })?;

    let role: Role = body
        .role
        .parse()
        .map_err(|_| bad_request(erp_web::messages::UNKNOWN_ROLE, "role", &body.role, locale))?;

    Ok((scopes, role))
}

fn key_id(raw: &str, locale: Locale) -> Result<uuid::Uuid, Problem> {
    raw.parse().map_err(|_| no_such_key(raw, locale))
}

async fn exists(
    state: &AppState,
    tenant: &Allowed<ManageTenant>,
    id: uuid::Uuid,
) -> Result<bool, Problem> {
    Ok(state
        .control
        .keys(tenant.db.tenant())
        .await
        .map_err(|e| refused(&e, Locale::DEFAULT))?
        .iter()
        .any(|key| key.id == id))
}

fn no_such_key(id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(erp_control::messages::NO_SUCH_KEY)
            .with("id", erp_i18n::MessageArg::text(id)),
        locale,
        &crate::CATALOG,
    )
}

fn refused(error: &erp_control::AccessError, locale: Locale) -> Problem {
    erp_web::ApiError::Access(error_of(error)).into_problem(locale, &crate::CATALOG)
}

/// `AccessError` is not `Clone`, and the helper above takes a reference so the
/// call sites read like every other one. This is the one place that pays for it.
fn error_of(error: &erp_control::AccessError) -> erp_control::AccessError {
    erp_control::AccessError::Corrupt(error.to_string())
}
