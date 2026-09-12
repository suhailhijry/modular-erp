//! The platform's own surface: routes about no tenant, for platform staff.
//!
//! Every route here takes `Staff<P>`, never `Allowed<C>` — a tenant role means
//! nothing on this surface, and `every_platform_role_against_every_platform_
//! endpoint` is the test that holds each route to the power its table names.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::{Actor, PlatformRole, StaffError};
use erp_i18n::{Locale, Localize};
use erp_types::{IdentityId, TenantId, Timestamp};
use erp_web::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{
    After, ApiError, AppState, HandleDeadLetters, Language, ManageStaff, Paged, Power, Problem,
    Query, ReadAuditTrail, Staff, SuspendTenants,
};

use crate::audit::AuditView;
use crate::effects::{DeadLetterView, dead_letter_id, no_such_dead_letter};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_staff, grant_staff))
        .routes(routes!(change_staff_role, revoke_staff))
        .routes(routes!(suspend_tenant))
        .routes(routes!(reinstate_tenant))
        .routes(routes!(list_control_dead_letters))
        .routes(routes!(requeue_control_dead_letter))
        .routes(routes!(dismiss_control_dead_letter))
        .routes(routes!(platform_audit_trail))
        .routes(routes!(reset_any_second_factor))
}

#[derive(Debug, Serialize, ToSchema)]
struct StaffView {
    #[schema(value_type = uuid::Uuid)]
    identity: IdentityId,
    handle: Option<String>,
    platform_role: &'static str,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    since: Timestamp,
    suspended: bool,
    /// **False is somebody every platform route refuses** until they enrol an
    /// authenticator app.
    second_factor: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "email": "noura@erp.example", "platform_role": "support" }))]
struct NewStaff {
    /// The login of an account that already exists, and has a second factor.
    email: String,
    platform_role: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct StaffAdded {
    #[schema(value_type = uuid::Uuid)]
    identity: IdentityId,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "platform_role": "billing" }))]
struct StaffRoleChange {
    platform_role: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "reason": "The August invoice is unpaid after three reminders." }))]
struct Suspension {
    /// Why, in 1 to 500 characters. **Written for the tenant's owner**, not
    /// as an internal note: they read it in their tenant's audit trail.
    reason: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "reason": "Ticket 4471: lost phone and recovery sheet, identity checked by video call." }))]
struct ResetReason {
    /// Why, in 1 to 500 characters. **The only record there will be** of why
    /// somebody's two-step sign-in was taken away: it goes into the platform
    /// audit trail under your name, and the person themselves is told nothing
    /// but that it happened.
    reason: String,
}

/// What to narrow the trail to. Paging is `After`, read beside it.
#[derive(Debug, Deserialize)]
struct AuditFilter {
    #[serde(default)]
    tenant: Option<TenantId>,
    #[serde(default)]
    identity: Option<IdentityId>,
}

/// Everybody on platform staff.
#[utoipa::path(
    get,
    path = "/v1/platform/staff",
    tag = "platform",
    responses(
        (status = OK, body = Vec<StaffView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not a superadmin — `access.not_permitted` naming `manage_staff` — or one without a second factor, `access.staff_second_factor_required`", body = Problem),
    ),
)]
async fn list_staff(
    _staff: Staff<ManageStaff>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Vec<StaffView>>, Problem> {
    let staff = state
        .control
        .staff()
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(
        staff
            .into_iter()
            .map(|s| StaffView {
                identity: s.identity,
                handle: s.handle,
                platform_role: s.role.as_str(),
                since: s.since,
                suspended: s.suspended,
                second_factor: s.second_factor,
            })
            .collect(),
    ))
}

/// Make an existing account platform staff.
///
/// The account must already have a second factor: a staff account with only a
/// password is one somebody holding that password could enrol their own on.
#[utoipa::path(
    post,
    path = "/v1/platform/staff",
    tag = "platform",
    request_body = NewStaff,
    responses(
        (status = CREATED, body = StaffAdded),
        (status = BAD_REQUEST, description = "No such platform role", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = CONFLICT, description = "Already staff — change their role instead", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No account signs in with that address, or it has no second factor", body = Problem),
    ),
)]
async fn grant_staff(
    staff: Staff<ManageStaff>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewStaff>,
) -> Result<(StatusCode, Json<StaffAdded>), Problem> {
    let role = parse_role(&body.platform_role, locale)?;
    let identity = state
        .control
        .grant_staff(&body.email, role, actor(&staff))
        .await
        .map_err(|e| staff_problem(&e, locale))?;
    Ok((StatusCode::CREATED, Json(StaffAdded { identity })))
}

/// Change what a staff member may do.
#[utoipa::path(
    patch,
    path = "/v1/platform/staff/{identity}",
    tag = "platform",
    params(("identity" = uuid::Uuid, Path, description = "From `GET /v1/platform/staff`.")),
    request_body = StaffRoleChange,
    responses(
        (status = NO_CONTENT, description = "Changed. Takes effect on their next request."),
        (status = BAD_REQUEST, description = "No such platform role", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not platform staff", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The last superadmin cannot be demoted", body = Problem),
    ),
)]
async fn change_staff_role(
    staff: Staff<ManageStaff>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
    Json(body): Json<StaffRoleChange>,
) -> Result<StatusCode, Problem> {
    let role = parse_role(&body.platform_role, locale)?;
    state
        .control
        .change_staff_role(identity, role, actor(&staff))
        .await
        .map_err(|e| staff_problem(&e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Take somebody off platform staff. Their account stays.
///
/// The last superadmin cannot be removed here; `operator revoke-staff` is the
/// break-glass path for when that account is the problem.
#[utoipa::path(
    delete,
    path = "/v1/platform/staff/{identity}",
    tag = "platform",
    params(("identity" = uuid::Uuid, Path, description = "From `GET /v1/platform/staff`.")),
    responses(
        (status = NO_CONTENT, description = "Removed. Takes effect on their next request."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not platform staff", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The last superadmin cannot be removed", body = Problem),
    ),
)]
async fn revoke_staff(
    staff: Staff<ManageStaff>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .revoke_staff(identity, actor(&staff))
        .await
        .map_err(|e| staff_problem(&e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Suspend a tenant: nothing runs for it until it is reinstated.
///
/// From the next request its members, its API keys and its public pages are
/// all answered `503 access.tenant_unavailable` — the one message, whoever
/// asks — everywhere but its audit trail, `GET /v1/audit`, which the owner (or
/// a key scoped `*:manage_tenant`) can still read. Its background jobs stop, a visit already under way before its next
/// job. Nobody is signed out: members may work for other tenants too. Support
/// can still open it.
///
/// **Write the reason for the tenant's owner.** It is recorded, under your
/// name, in the audit trail about their tenant, and they read it there —
/// `GET /v1/audit` answers them while every other route is `503` — so it is no
/// place for internal notes.
#[utoipa::path(
    post,
    path = "/v1/platform/tenants/{id}/suspend",
    tag = "platform",
    params(("id" = uuid::Uuid, Path, description = "The tenant.")),
    request_body = Suspension,
    responses(
        (status = NO_CONTENT, description = "Suspended."),
        (status = BAD_REQUEST, description = "No reason, or one over 500 characters — `tenants.suspension_reason`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not billing or superadmin — `access.not_permitted` naming `suspend_tenants` — or without a second factor", body = Problem),
        (status = NOT_FOUND, description = "No such tenant", body = Problem),
        (status = CONFLICT, description = "Not active — already suspended, or still provisioning. `tenants.wrong_status` names what it is", body = Problem),
    ),
)]
async fn suspend_tenant(
    staff: Staff<SuspendTenants>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<TenantId>,
    Json(body): Json<Suspension>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .suspend_tenant(id, &body.reason, actor(&staff))
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lift a tenant's suspension. It is usable again from the next request, and
/// its background jobs catch up at their next visit.
#[utoipa::path(
    post,
    path = "/v1/platform/tenants/{id}/reinstate",
    tag = "platform",
    params(("id" = uuid::Uuid, Path, description = "The tenant.")),
    responses(
        (status = NO_CONTENT, description = "Reinstated."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not billing or superadmin — `access.not_permitted` naming `suspend_tenants` — or without a second factor", body = Problem),
        (status = NOT_FOUND, description = "No such tenant", body = Problem),
        (status = CONFLICT, description = "Not suspended. `tenants.wrong_status` names what it is", body = Problem),
    ),
)]
async fn reinstate_tenant(
    staff: Staff<SuspendTenants>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<TenantId>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .reinstate_tenant(id, actor(&staff))
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Everything the control plane gave up on, oldest first: signup, invitation
/// and reset emails, and sign-in texts.
///
/// The key says which — `signup:`, `invitation:`, `reset:` or `code:`, then
/// the id of the row it was about. The same shape as a tenant's
/// `GET /v1/effects/dead`.
#[utoipa::path(
    get,
    path = "/v1/platform/effects/dead",
    tag = "platform",
    responses(
        (status = OK, body = Vec<DeadLetterView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not support or superadmin — `access.not_permitted` naming `handle_dead_letters` — or without a second factor", body = Problem),
    ),
)]
async fn list_control_dead_letters(
    _staff: Staff<HandleDeadLetters>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Vec<DeadLetterView>>, Problem> {
    let dead = state
        .control
        .dead_letters(crate::effects::PAGE)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(dead.into_iter().map(DeadLetterView::from).collect()))
}

/// Put one back in the queue, due now, with its attempts reset. The worker
/// sends it on its next pass; it keeps its idempotency key.
///
/// **Dismiss a `code:` or `reset:` one instead.** A sign-in code expires in
/// five minutes and a reset link in an hour, and the retries take about four
/// hours to give up, so one that died of an outage had expired long before it
/// died. Requeued, it sends somebody a code that no longer works; they ask for
/// another. Recorded in the audit trail under your name.
#[utoipa::path(
    post,
    path = "/v1/platform/effects/dead/{id}/requeue",
    tag = "platform",
    params(("id" = i64, Path, description = "From `GET /v1/platform/effects/dead`.")),
    responses(
        (status = NO_CONTENT, description = "Back in the queue."),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not support or superadmin, or without a second factor", body = Problem),
        (status = NOT_FOUND, description = "No dead letter with that id — never dead, or already dealt with", body = Problem),
    ),
)]
async fn requeue_control_dead_letter(
    staff: Staff<HandleDeadLetters>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = dead_letter_id(&id, locale)?;
    let found = state
        .control
        .requeue_dead_letter(id, actor(&staff))
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    if !found {
        return Err(no_such_dead_letter(id, locale));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete one that should not be sent — a stale sign-in code or reset link,
/// or anything the person has since asked for again.
///
/// **Only a dead one.** A pending or delivered effect is a `404` and is left
/// alone. Recorded in the audit trail under your name, by kind and key.
#[utoipa::path(
    delete,
    path = "/v1/platform/effects/dead/{id}",
    tag = "platform",
    params(("id" = i64, Path, description = "From `GET /v1/platform/effects/dead`.")),
    responses(
        (status = NO_CONTENT, description = "Gone."),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not support or superadmin, or without a second factor", body = Problem),
        (status = NOT_FOUND, description = "No dead letter with that id — never dead, or already dealt with", body = Problem),
    ),
)]
async fn dismiss_control_dead_letter(
    staff: Staff<HandleDeadLetters>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = dead_letter_id(&id, locale)?;
    let found = state
        .control
        .dismiss_dead_letter(id, actor(&staff))
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    if !found {
        return Err(no_such_dead_letter(id, locale));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The audit trail, every tenant's and the platform's own, newest first.
///
/// `tenant` narrows it to one company's — what its owner reads at
/// `GET /v1/audit`. `identity` narrows it to entries about that person and
/// entries they made, platform staff included. Both is the entries in each.
/// Neither is everything, what concerns no tenant included: staff changes,
/// the control plane's dead letters, clusters, signups nobody confirmed.
/// Unlike a customer's view, every actor is named by login.
#[utoipa::path(
    get,
    path = "/v1/platform/audit",
    tag = "platform",
    params(
        ("tenant" = Option<uuid::Uuid>, Query, description = "Only what concerns this tenant."),
        ("identity" = Option<uuid::Uuid>, Query, description = "Only what concerns, or was done by, this person."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page: 50, at most 200. Clamped, never refused."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the trail ended.", body = Paged<AuditView>),
        (status = BAD_REQUEST, description = "An unreadable cursor, or a `tenant` or `identity` that is not a UUID", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not support or superadmin — `access.not_permitted` naming `read_audit_trail` — or without a second factor", body = Problem),
    ),
)]
async fn platform_audit_trail(
    _staff: Staff<ReadAuditTrail>,
    State(state): State<AppState>,
    Language(locale): Language,
    Query(page): Query<After>,
    Query(filter): Query<AuditFilter>,
) -> Result<Json<Paged<AuditView>>, Problem> {
    let before = crate::audit::resume(&page, locale)?;
    let (default, max) = crate::audit::PAGE;
    let entries = state
        .control
        .platform_audit(
            filter.tenant,
            filter.identity,
            page.limit(default, max),
            before,
        )
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(Paged::of(entries, AuditView::from)))
}

/// **Reset anybody's two-step sign-in.**
///
/// The route for the person a tenant cannot reset itself: somebody who belongs
/// to more than one organisation, whose factor is their account's rather than
/// any one company's. `POST /v1/members/{identity}/second-factor-reset` is the
/// one a company uses for its own people.
///
/// Their authenticator app and every recovery code stop working, every session
/// they hold ends, and they are emailed a one-time link. **Until they open one,
/// their password alone cannot set up a new app**, and that holds after the link
/// expires — call this again to send another.
///
/// **Resetting platform staff needs `manage_staff`**, so support cannot reset a
/// superadmin's. Nobody resets their own here either: replace your app, or ask
/// another superadmin.
///
/// The reason is required and goes into the platform audit trail under your
/// name. It is the only record of why anybody's sign-in was weakened, so write
/// the ticket, not "user request".
#[utoipa::path(
    post,
    path = "/v1/platform/identities/{identity}/second-factor-reset",
    tag = "platform",
    params(("identity" = uuid::Uuid, Path, description = "The person, from `GET /v1/platform/audit` or a tenant's member list.")),
    request_body = ResetReason,
    responses(
        (status = NO_CONTENT, description = "Reset. A link is on its way to them."),
        (status = BAD_REQUEST, description = "No reason, or one over 500 characters — `second_factor.reset_reason`", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not support or superadmin — `access.not_permitted` naming `reset_second_factors` — or without a second factor. Also what support gets for a staff account: `access.not_permitted` naming `manage_staff`", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "`second_factor.reset_yourself`, or `second_factor.reset_no_login` when there is no account with an email login by that id", body = Problem),
    ),
)]
async fn reset_any_second_factor(
    staff: Staff<erp_web::ResetSecondFactors>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
    Json(body): Json<ResetReason>,
) -> Result<StatusCode, Problem> {
    let link_base = format!("https://{}/second-factor/", state.domain);
    state
        .control
        .reset_any_second_factor(
            staff.session.identity,
            identity,
            &body.reason,
            locale,
            &link_base,
        )
        .await
        .map_err(|e| crate::members::reset_problem(&e, locale))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Who did it. Every one of these lands in the audit trail under their name.
fn actor<P: Power>(staff: &Staff<P>) -> Actor {
    Actor::identity(staff.session.identity)
}

fn parse_role(raw: &str, locale: Locale) -> Result<PlatformRole, Problem> {
    raw.parse::<PlatformRole>().map_err(|_| {
        ApiError::BadRequest(
            erp_i18n::Message::new(erp_web::messages::UNKNOWN_STAFF_ROLE)
                .with("role", erp_i18n::MessageArg::text(raw.to_owned())),
        )
        .into_problem(locale, &crate::CATALOG)
    })
}

fn staff_problem(error: &StaffError, locale: Locale) -> Problem {
    let status = match error {
        StaffError::AlreadyStaff(_) => StatusCode::CONFLICT,
        StaffError::NotStaff => StatusCode::NOT_FOUND,
        // Well formed, and refused on the state of what it names.
        StaffError::NoSuchAccount(_)
        | StaffError::NoSecondFactor(_)
        | StaffError::LastSuperadmin => StatusCode::UNPROCESSABLE_ENTITY,
        StaffError::Access(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(error = %error, "staff management failed");
    }
    Problem::new(status, &error.message(), locale, &crate::CATALOG)
}
