//! Reading the audit trail: a tenant's, for its owner, and a person's own.
//! Staff read all of it at `GET /v1/platform/audit`, in [`crate::platform`].
//!
//! The trail is control-plane data, so neither route opens a tenant's
//! database — and the owner's route does not go through `enter`, which is what
//! lets the owner of a suspended tenant read why it was suspended.

use axum::extract::State;
use erp_control::AuditEntry;
use erp_i18n::Locale;
use erp_types::{IdentityId, TenantId, Timestamp};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{
    After, ApiError, AppState, Authenticated, Json, Language, ManagesTenant, Paged, Problem, Query,
    bad_request,
};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(audit_trail))
        .routes(routes!(my_audit_trail))
}

/// Rows per page when the caller does not say, and the most it may ask for.
pub(crate) const PAGE: (i64, i64) = (50, 200);

/// One thing that was done, by whom, and when.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AuditView {
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    at: Timestamp,
    /// Who did it. Null for the system, and for somebody since erased.
    #[schema(value_type = Option<uuid::Uuid>)]
    actor: Option<IdentityId>,
    /// The actor's login, where you may see it: on a tenant's trail and your
    /// own, only when they are, or were, a member of the tenant the entry
    /// concerns — so staff who never were appear by id alone. Null for an
    /// actor with no password login, such as an API key.
    actor_handle: Option<String>,
    /// Whose behalf the actor acted on, when staff acted as somebody.
    #[schema(value_type = Option<uuid::Uuid>)]
    on_behalf_of: Option<IdentityId>,
    /// The company it concerns. Null for one about a person or the platform
    /// alone.
    #[schema(value_type = Option<uuid::Uuid>)]
    tenant: Option<TenantId>,
    /// What was done: `tenant.suspended`, `membership.role_changed`,
    /// `api_key.revoked`, …
    action: String,
    /// What it was done to — `tenant`, `identity`, `api_key`, `effect`, … —
    /// and which one.
    subject_type: String,
    subject_id: String,
    /// The particulars, shaped by `action`. A suspension's `reason` is here.
    #[schema(value_type = Object)]
    detail: serde_json::Value,
}

impl From<AuditEntry> for AuditView {
    fn from(entry: AuditEntry) -> Self {
        Self {
            at: entry.at,
            actor: entry.actor,
            actor_handle: entry.actor_handle,
            on_behalf_of: entry.on_behalf_of,
            tenant: entry.tenant,
            action: entry.action,
            subject_type: entry.subject_type,
            subject_id: entry.subject_id,
            detail: entry.detail,
        }
    }
}

/// This tenant's audit trail, newest first: who changed its members, keys,
/// domains, modules and invitations, when support opened it, and why it was
/// suspended.
///
/// **It answers while the tenant is suspended**, where every other route on
/// this host answers `503`: it is where the owner reads the reason. Owner
/// only. Entries about a person alone — their account being suspended, say —
/// are theirs to read at `GET /v1/sessions/current/audit`, not the tenant's.
#[utoipa::path(
    get,
    path = "/v1/audit",
    tag = "audit",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page: 50, at most 200. Clamped, never refused."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the trail ended.", body = Paged<AuditView>),
        (status = BAD_REQUEST, description = "An unreadable cursor — `request.invalid_cursor`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner — `access.not_permitted` naming `manage_tenant` — or a tenant that requires a second factor this account has not enrolled", body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn audit_trail(
    owner: ManagesTenant,
    State(state): State<AppState>,
    Language(locale): Language,
    Query(page): Query<After>,
) -> Result<Json<Paged<AuditView>>, Problem> {
    let before = resume(&page, locale)?;
    let entries = state
        .control
        .tenant_audit(owner.tenant, page.limit(PAGE.0, PAGE.1), before)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(Paged::of(entries, AuditView::from)))
}

/// What the platform recorded about you, newest first — what was done to your
/// account and your memberships, and what you did, in every company you work
/// for. Your right of access under the PDPL.
///
/// Somebody else in an entry is named by login only when they are, or were, a
/// member of the company it concerns; staff who never were appear by id.
///
/// **A person's, so an API key is refused**, whatever its scopes. A key's own
/// trail is its issuing, which names the owner who issued it — an address the
/// key's scopes deny it everywhere else.
#[utoipa::path(
    get,
    path = "/v1/sessions/current/audit",
    tag = "audit",
    params(
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page: 50, at most 200. Clamped, never refused."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the trail ended.", body = Paged<AuditView>),
        (status = BAD_REQUEST, description = "An unreadable cursor — `request.invalid_cursor`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "An API key — `keys.not_a_person`", body = Problem),
    ),
)]
async fn my_audit_trail(
    auth: Authenticated,
    State(state): State<AppState>,
    Language(locale): Language,
    Query(page): Query<After>,
) -> Result<Json<Paged<AuditView>>, Problem> {
    if auth.key.is_some() {
        return Err(erp_web::not_a_person(locale));
    }
    let before = resume(&page, locale)?;
    let entries = state
        .control
        .identity_audit(auth.session.identity, page.limit(PAGE.0, PAGE.1), before)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(Paged::of(entries, AuditView::from)))
}

/// Where a page of the trail resumes. A cursor that is not one the trail
/// handed out — another list's, or a hand-made one — is the same `400` as one
/// that is not a cursor at all, never the first page again.
pub(crate) fn resume(page: &After, locale: Locale) -> Result<Option<i64>, Problem> {
    page.cursor(locale)?
        .map(|cursor| {
            erp_control::audit_position(&cursor).map_err(|_| {
                bad_request(
                    erp_web::messages::INVALID_CURSOR,
                    "after",
                    page.after.as_deref().unwrap_or_default(),
                    locale,
                )
            })
        })
        .transpose()
}
