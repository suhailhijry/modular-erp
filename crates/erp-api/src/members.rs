//! Who else has access to a tenant.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_control::{Actor, MemberError, Role};
use erp_i18n::{Locale, Localize};
use erp_types::{IdentityId, Timestamp};
use erp_web::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::ApiError;
use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Language, ManageTenant, Read};

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
        .routes(routes!(second_factor_policy, set_second_factor_policy))
        .routes(routes!(reset_member_second_factor))
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
    /// to the recipient — see `erp-control/src/members.rs`.
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;

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

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "required": true }))]
struct SecondFactorPolicy {
    /// **When true, a member with no authenticator app is refused at entry.**
    ///
    /// It refuses entry *to this tenant*: their session stays valid and their
    /// other organisations stay reachable. Somebody already signed in is not
    /// thrown out mid-action — they are stopped the next time they come
    /// through the door, and told to enrol. And while it holds, a member who
    /// has an authenticator app can replace it but not turn it off.
    required: bool,
}

/// Whether this organisation requires two-step sign-in.
#[utoipa::path(
    get,
    path = "/v1/members/second-factor-policy",
    tag = "members",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    responses(
        (status = OK, body = SecondFactorPolicy),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn second_factor_policy(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<SecondFactorPolicy>, Problem> {
    let found = state
        .control
        .tenant(tenant.db.tenant())
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(Json(SecondFactorPolicy {
        required: found.is_some_and(|t| t.requires_second_factor),
    }))
}

/// Require — or stop requiring — two-step sign-in here.
///
/// **Turning it on is refused unless you have enrolled one yourself.** One
/// rule, no special cases, and it guarantees at least one person can still get
/// in: an owner who could switch this on from an unprotected account would be
/// one click from locking the business out of its own books.
///
/// Turning it **off** carries no such condition — somebody has to be able to
/// undo this, and needing a second factor to remove the requirement is the trap
/// it exists to prevent.
#[utoipa::path(
    put,
    path = "/v1/members/second-factor-policy",
    tag = "members",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = SecondFactorPolicy,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not permitted here — or switching it on without a second factor of your own, which is `auth.tenant_requires_second_factor`", body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn set_second_factor_policy(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<SecondFactorPolicy>,
) -> Result<StatusCode, Problem> {
    state
        .control
        .set_second_factor_requirement(tenant.db.tenant(), tenant.session.identity, body.required)
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}

/// **Reset a colleague's two-step sign-in**, for somebody who has lost both
/// their authenticator app and their recovery codes.
///
/// Their authenticator app and every recovery code stop working, every session
/// they have anywhere ends, and they are emailed a one-time link. **Until they
/// open it, their password alone cannot set up a new app** — which holds after
/// the link expires, so running one out changes nothing. Send another by
/// calling this again; there is no second route, because a fresh link is the
/// same act.
///
/// **Who may.** This tenant's owner, always; or a member who holds
/// `hr:reset_second_factor` in the branch they are asking in, which needs an
/// employee record and travels up the org chart like every claim outside
/// `hr::SEGREGATED`. Nobody else, whatever else their role allows — and **no
/// API key**, whatever its scopes and whatever role it was issued
/// (`keys.not_a_person`): a machine has no employee record, so it can hold no
/// claim, and taking a colleague's sign-in away is a person's act.
///
/// **Who cannot be reset here**, each with its own code: yourself
/// (`second_factor.reset_yourself` — replace your app instead), this tenant's
/// owner (`second_factor.reset_the_owner`), platform staff
/// (`second_factor.reset_platform_staff`), and **anybody who also belongs to
/// another organisation** (`second_factor.reset_another_company`): two-step
/// sign-in is their account's everywhere, not this company's, so support resets
/// those.
///
/// Recorded in this tenant's audit trail under your name.
#[utoipa::path(
    post,
    path = "/v1/members/{identity}/second-factor-reset",
    tag = "members",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
        ("identity" = uuid::Uuid, Path, description = "From `GET /v1/members`."),
        ("X-Branch" = Option<String>, Header, description = "The branch the claim is judged in. The owner needs none."),
    ),
    responses(
        (status = NO_CONTENT, description = "Reset. A link is on its way to them; nothing here shows it."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "Not the owner and not holding `hr:reset_second_factor` — `access.not_permitted` naming `manage_tenant`; or an API key, which may never do this — `keys.not_a_person`", body = Problem),
        (status = NOT_FOUND, description = "Not a member here", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Refused on who they are: `second_factor.reset_yourself`, `second_factor.reset_the_owner`, `second_factor.reset_platform_staff`, `second_factor.reset_another_company` (contact support), or `second_factor.reset_no_login` when the account has no email login to send to", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "This person has been reset three times in the last hour, by anybody, here or by support — `request.too_many_requests`, and `args.seconds` says how long to wait", body = Problem),
    ),
)]
async fn reset_member_second_factor(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(identity): Path<IdentityId>,
) -> Result<StatusCode, Problem> {
    // **A key is not a person, and this is a person's act.** The scope gate is
    // the door's capability, and the door here is `Allowed<Read>` — so a
    // reporting credential issued `*:read` clears a gate that shuts on it at
    // every sibling member route, and the owner's role its machine identity
    // holds is all the check below would ask for. A wider scope is not the
    // answer: no key can hold the claim that is the other way in, because
    // `claimant` finds no employee record for a machine. Same refusal
    // `GET /v1/sessions/current/audit` gives one, for the same reason.
    if tenant.key.is_some() {
        return Err(erp_web::not_a_person(locale));
    }

    // **`Allowed<Read>` at the door, and the real check here**, because the
    // claim that lifts it lives in the tenant's own database and the extractor
    // is above it. `manage_tenant` is what the 403 names: it is what lets the
    // owner through without a claim, so it is what somebody refused should ask
    // for — or ask to be granted the claim.
    if !tenant.db.is_owner() && !holds_the_claim(&tenant, locale).await? {
        return Err(erp_web::not_permitted(
            erp_control::Capability::ManageTenant,
            locale,
        ));
    }

    // **After the caller has proved they may**, so a stranger's refusal costs
    // the target nothing; before the control plane, so a refused attempt ends
    // no session and sends no mail.
    erp_web::charge_for_a_reset(&state, identity, locale).await?;

    // Where the link points is decided here, because only this layer knows the
    // deployment's public domain — as `request_password_reset` does.
    let link_base = format!("https://{}/second-factor/", state.domain);
    state
        .control
        .reset_member_second_factor(
            tenant.db.tenant(),
            tenant.session.identity,
            identity,
            locale,
            &link_base,
        )
        .await
        .map_err(|e| reset_problem(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Whether the person behind this request holds `hr:reset_second_factor` here.
///
/// `hr::actor_holds` and not `hr::may`: "nobody has granted this claim in this
/// tenant" must read as *not held* rather than as a pass, or a tenant that uses
/// no claims would let every clerk reset every colleague. It asks the grants,
/// which live in the tenant's own migration chain, before it asks `hr`'s read
/// model, so a tenant that has granted nothing never looks for `proj_hr` at all
/// (§68). A tenant that *has* granted has had `hr` on — that is the only way a
/// grant is written — and switching a module off never drops its read models,
/// so the second query has a table to read either way.
async fn holds_the_claim(tenant: &Allowed<Read>, locale: Locale) -> Result<bool, Problem> {
    let mut conn = tenant.db.acquire().await.map_err(|e| {
        ApiError::Access(erp_control::AccessError::Pool(e)).into_problem(locale, &crate::CATALOG)
    })?;
    hr::actor_holds(
        &mut conn,
        hr::RESET_SECOND_FACTOR,
        &erp_web::metadata(tenant),
    )
    .await
    .map_err(|e| {
        ApiError::Access(erp_control::AccessError::Database(e))
            .into_problem(locale, &crate::CATALOG)
    })
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
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
        ("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),
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
            erp_i18n::Message::new(erp_web::messages::UNKNOWN_ROLE)
                .with("role", erp_i18n::MessageArg::text(raw.to_owned())),
        )
        .into_problem(locale, &crate::CATALOG)
    })
}

fn too_short(locale: Locale) -> Problem {
    ApiError::BadRequest(
        erp_i18n::Message::new(erp_web::messages::PASSWORD_TOO_SHORT).with(
            "n",
            erp_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
        ),
    )
    .into_problem(locale, &crate::CATALOG)
}

/// What each reset refusal answers. Shared with the platform route, which is
/// why it lives beside the error rather than inside either handler.
pub(crate) fn reset_problem(error: &erp_control::ResetError, locale: Locale) -> Problem {
    use erp_control::ResetError;
    let status = match error {
        // The same 404 `remove_member` gives somebody who is not here, so this
        // route is no oracle for identities the caller cannot already list.
        ResetError::NotAMember => StatusCode::NOT_FOUND,
        // Well formed, and refused on the state of whoever it names.
        ResetError::Yourself
        | ResetError::TheOwner
        | ResetError::PlatformStaff
        | ResetError::AnotherCompany
        | ResetError::NoLogin => StatusCode::UNPROCESSABLE_ENTITY,
        ResetError::Reason => StatusCode::BAD_REQUEST,
        // Support asking to reset a superadmin: 403 naming `manage_staff`, the
        // same answer every other platform door gives.
        ResetError::Access(
            erp_control::AccessError::StaffOnly(_)
            | erp_control::AccessError::StaffSecondFactorRequired,
        ) => StatusCode::FORBIDDEN,
        ResetError::Access(
            erp_control::AccessError::NoSuchIdentity | erp_control::AccessError::IdentitySuspended,
        ) => StatusCode::UNAUTHORIZED,
        ResetError::Access(_) | ResetError::Auth(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(error = %error, "a second-factor reset failed");
    }
    let message = if status.is_server_error() {
        erp_i18n::Message::new(erp_control::messages::INTERNAL)
    } else {
        error.message()
    };
    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
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
