//! Who else has access to a tenant.

use crate::wire::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use spa_control::{Actor, MemberError, Role};
use spa_i18n::{Locale, Localize};
use spa_types::{IdentityId, Timestamp};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageTenant, Read};
use crate::problem::Problem;
use crate::state::AppState;

/// Shortest password an owner may set for a colleague.
///
/// The same floor as signup: a password someone picks for another person is not
/// more trustworthy than one they pick for themselves.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_members, add_member))
        .routes(routes!(change_role, remove_member))
        .routes(routes!(set_module_role, clear_module_role))
}

#[derive(Debug, Serialize, ToSchema)]
struct ModuleRoleView {
    module: String,
    role: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
struct MemberView {
    #[schema(value_type = uuid::Uuid)]
    identity: IdentityId,
    handle: Option<String>,
    /// What they are here, and in any module not listed below.
    role: &'static str,
    /// Where the tenant said something different. Usually empty.
    module_roles: Vec<ModuleRoleView>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    since: Timestamp,
    suspended: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "email": "sara@acme.example",
    "password": "correct horse battery staple",
    "role": "accountant"
}))]
struct NewMember {
    /// Their login. If it already belongs to someone, that account gains access
    /// rather than a second one being created for the same person.
    email: String,
    /// Chosen by the owner and handed over. An invitation flow would leave this
    /// to the recipient — see `spa-control/src/members.rs`.
    password: String,
    role: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct MemberAdded {
    #[schema(value_type = uuid::Uuid)]
    identity: IdentityId,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "role": "accountant" }))]
struct RoleChange {
    role: String,
}

/// Reading the list is `Read`, not `ManageTenant`.
///
/// Knowing who can see your books is not an administrative privilege; it is the
/// thing a viewer most needs to be able to check.
#[utoipa::path(
    get,
    path = "/v1/members",
    tag = "members",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, body = Vec<MemberView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn list_members(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Vec<MemberView>>, Problem> {
    let members = state
        .control
        .members(tenant.db.tenant())
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok(Json(
        members
            .into_iter()
            .map(|m| MemberView {
                identity: m.identity,
                handle: m.handle,
                role: m.role.as_str(),
                module_roles: m
                    .module_roles
                    .into_iter()
                    .map(|(module, role)| ModuleRoleView {
                        module: module.as_str().to_owned(),
                        role: role.as_str(),
                    })
                    .collect(),
                since: m.since,
                suspended: m.suspended,
            })
            .collect(),
    ))
}

/// Add somebody, choosing their password for them.
///
/// If the address already has an account here, that account gains access rather
/// than a second one being created for the same person. `POST
/// /v1/invitations` is the other way round: the recipient picks
/// their own password and nobody has to hand one over.
#[utoipa::path(
    post,
    path = "/v1/members",
    tag = "members",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),),
    request_body = NewMember,
    responses(
        (status = CREATED, body = MemberAdded),
        (status = BAD_REQUEST, description = "A password under 12 characters, or a role that does not exist", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Already a member here — change their role instead", body = Problem),
    ),
)]
async fn add_member(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewMember>,
) -> Result<(StatusCode, Json<MemberAdded>), Problem> {
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(too_short(locale));
    }
    let role = parse_role(&body.role, locale)?;

    let identity = state
        .control
        .add_member(
            tenant.db.tenant(),
            body.email,
            body.password,
            role,
            actor(&tenant),
        )
        .await
        .map_err(|e| member_problem(&e, locale))?;

    Ok((StatusCode::CREATED, Json(MemberAdded { identity })))
}

/// Change what somebody may do across the whole tenant.
#[utoipa::path(
    patch,
    path = "/v1/members/{identity}",
    tag = "members",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("identity" = uuid::Uuid, Path, description = "From `GET /v1/members`."),
    ),
    request_body = RoleChange,
    responses(
        (status = NO_CONTENT, description = "Changed."),
        (status = BAD_REQUEST, description = "No such role", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not a member here", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The last owner cannot be demoted", body = Problem),
    ),
)]
async fn change_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
    Json(body): Json<RoleChange>,
) -> Result<StatusCode, Problem> {
    let role = parse_role(&body.role, locale)?;

    state
        .control
        .change_role(tenant.db.tenant(), identity, role, actor(&tenant))
        .await
        .map_err(|e| member_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Take somebody's access away.
///
/// Their profile stays. Adding them back later restores it rather than starting
/// a second one.
#[utoipa::path(
    delete,
    path = "/v1/members/{identity}",
    tag = "members",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("identity" = uuid::Uuid, Path, description = "From `GET /v1/members`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Removed."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not a member here", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The last owner cannot be removed", body = Problem),
    ),
)]
async fn remove_member(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .remove_member(tenant.db.tenant(), identity, actor(&tenant))
        .await
        .map_err(|e| member_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Gives somebody a different role in one module.
///
/// # Why this is a separate route rather than a field on the member
///
/// Because it is the exception. "Sara does the invoicing, Khalid does the
/// books" is a real arrangement and this is how a tenant says so — but most
/// people have one job, and a members form with a row per module per person
/// would put that in front of everybody who does not need it.
#[utoipa::path(
    put,
    path = "/v1/members/{identity}/modules/{module}",
    tag = "members",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("identity" = uuid::Uuid, Path, description = "From `GET /v1/members`."),
        ("module" = String, Path, description = "A name from `GET /v1/modules`."),
    ),
    request_body = RoleChange,
    responses(
        (status = NO_CONTENT, description = "Set. Applies in this module only."),
        (status = BAD_REQUEST, description = "No such role, or no such module", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not a member here", body = Problem),
    ),
)]
async fn set_module_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((identity, module)): Path<(IdentityId, String)>,
    Json(body): Json<RoleChange>,
) -> Result<StatusCode, Problem> {
    let role = parse_role(&body.role, locale)?;
    let module = crate::modules::find(&module, locale)?.module;

    state
        .control
        .set_module_role(
            tenant.db.tenant(),
            identity,
            &module,
            Some(role),
            actor(&tenant),
        )
        .await
        .map_err(|e| member_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Puts somebody back on their tenant-wide role in a module.
///
/// Different from setting them to `viewer` there: the exception is gone, so a
/// later change to their tenant-wide role reaches this module too.
#[utoipa::path(
    delete,
    path = "/v1/members/{identity}/modules/{module}",
    tag = "members",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.spa.com`. Every path below is about that tenant."),
        ("identity" = uuid::Uuid, Path, description = "From `GET /v1/members`."),
        ("module" = String, Path, description = "A name from `GET /v1/modules`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Cleared. They are back on their tenant-wide role here."),
        (status = BAD_REQUEST, description = "No such module", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "Not a member here", body = Problem),
    ),
)]
async fn clear_module_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((identity, module)): Path<(IdentityId, String)>,
) -> Result<StatusCode, Problem> {
    let module = crate::modules::find(&module, locale)?.module;

    state
        .control
        .set_module_role(tenant.db.tenant(), identity, &module, None, actor(&tenant))
        .await
        .map_err(|e| member_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------

/// Who did it. Every one of these lands in the audit trail.
fn actor(tenant: &Allowed<ManageTenant>) -> Actor {
    Actor::identity(tenant.session.identity)
}

fn parse_role(raw: &str, locale: Locale) -> Result<Role, Problem> {
    raw.parse::<Role>().map_err(|_| {
        ApiError::BadRequest(
            spa_i18n::Message::new(crate::messages::UNKNOWN_ROLE)
                .with("role", spa_i18n::MessageArg::text(raw.to_owned())),
        )
        .into_problem(locale)
    })
}

fn too_short(locale: Locale) -> Problem {
    ApiError::BadRequest(
        spa_i18n::Message::new(crate::messages::PASSWORD_TOO_SHORT).with(
            "n",
            spa_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
        ),
    )
    .into_problem(locale)
}

fn member_problem(error: &MemberError, locale: Locale) -> Problem {
    let status = match error {
        // The name is taken *for this tenant*. Changing their role is the move.
        MemberError::AlreadyAMember(_) => StatusCode::CONFLICT,
        // Well-formed and refused on the state of the tenant, not the request.
        MemberError::LastOwner => StatusCode::UNPROCESSABLE_ENTITY,
        // Names somebody who is not here. Not an oracle: the caller can already
        // list this tenant's members.
        MemberError::NotAMember => StatusCode::NOT_FOUND,
        MemberError::Access(_) | MemberError::Auth(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(error = %error, "member management failed");
    }
    Problem::new(status, &error.message(), locale, &crate::catalog::CATALOG)
}
