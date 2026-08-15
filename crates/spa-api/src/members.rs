//! Who else has access to a tenant.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};
use spa_control::{Actor, MemberError, Role};
use spa_i18n::{Locale, Localize};
use spa_types::{IdentityId, Timestamp};

use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageTenant, Read};
use crate::problem::Problem;
use crate::state::AppState;

/// Shortest password an owner may set for a colleague.
///
/// The same floor as signup: a password someone picks for another person is not
/// more trustworthy than one they pick for themselves.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/tenants/{slug}/members", routing::get(list).post(add))
        .route(
            "/v1/tenants/{slug}/members/{identity}",
            routing::patch(change_role).delete(remove),
        )
        .route(
            "/v1/tenants/{slug}/members/{identity}/modules/{module}",
            routing::put(set_module_role).delete(clear_module_role),
        )
}

#[derive(Debug, Serialize)]
struct ModuleRoleView {
    module: String,
    role: &'static str,
}

#[derive(Debug, Serialize)]
struct MemberView {
    identity: IdentityId,
    handle: Option<String>,
    /// What they are here, and in any module not listed below.
    role: &'static str,
    /// Where the tenant said something different. Usually empty.
    module_roles: Vec<ModuleRoleView>,
    since: Timestamp,
    suspended: bool,
}

#[derive(Debug, Deserialize)]
struct NewMember {
    /// Their login. If it already belongs to someone, that account gains access
    /// rather than a second one being created for the same person.
    email: String,
    /// Chosen by the owner and handed over. An invitation flow would leave this
    /// to the recipient — see `spa-control/src/members.rs`.
    password: String,
    role: String,
}

#[derive(Debug, Serialize)]
struct MemberAdded {
    identity: IdentityId,
}

#[derive(Debug, Deserialize)]
struct RoleChange {
    role: String,
}

/// Reading the list is `Read`, not `ManageTenant`.
///
/// Knowing who can see your books is not an administrative privilege; it is the
/// thing a viewer most needs to be able to check.
async fn list(
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

async fn add(
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

async fn change_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((_slug, identity)): Path<(String, IdentityId)>,
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

async fn remove(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((_slug, identity)): Path<(String, IdentityId)>,
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
async fn set_module_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((_slug, identity, module)): Path<(String, IdentityId, String)>,
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
async fn clear_module_role(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((_slug, identity, module)): Path<(String, IdentityId, String)>,
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
