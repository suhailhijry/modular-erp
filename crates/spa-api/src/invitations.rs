//! Inviting a colleague, and taking up an invitation.
//!
//! Two audiences, which is why the routes are split across two prefixes. The
//! `/v1/tenants/{slug}/invitations` ones need `ManageTenant`; the
//! `/v1/invitations/{token}` ones are **unauthenticated**, because the person
//! accepting has no account yet — the token is the credential.

use crate::wire::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use spa_control::{Actor, InvitationError, Role};
use spa_i18n::{Locale, Localize};
use spa_types::Timestamp;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiError;
use crate::extract::{Allowed, Language, ManageTenant};
use crate::problem::Problem;
use crate::state::AppState;
use crate::wire::bad_request;

/// Matches signup. A password chosen through an invitation is the same kind of
/// password as one chosen at signup, so the rule is the same one.
const MIN_PASSWORD: usize = 12;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_invitations, invite))
        .routes(routes!(revoke_invitation))
        // No authentication: whoever is accepting does not have an account yet,
        // and the token in the path is what proves they were invited.
        .routes(routes!(show_invitation))
        .routes(routes!(accept_invitation))
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "handle": "khalid@acme.example", "role": "accountant" }))]
struct NewInvitation {
    /// The address to invite. Also the login handle they will end up with.
    handle: String,
    /// `owner`, `manager`, `accountant` or `viewer`.
    role: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct InvitationCreated {
    id: String,
    handle: String,
    role: &'static str,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
    /// **Shown once.** Pass it on however you already talk to this person.
    /// Nothing stores it, so a lost link is re-issued rather than recovered.
    token: String,
    /// Where the token goes, so a client does not have to build the URL itself
    /// and get it subtly wrong.
    accept_path: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct InvitationView {
    id: String,
    handle: String,
    role: &'static str,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    created_at: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct PendingView {
    /// What you are being invited to, and as what. A link that does not say is
    /// a link people accept without reading.
    company: String,
    slug: String,
    handle: String,
    role: &'static str,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
    /// `true` when that address already has an account here — so the form asks
    /// for the existing password rather than offering to choose a new one.
    has_account: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "password": "correct horse battery staple" }))]
struct Acceptance {
    /// The existing account's password, or the one being chosen for a new
    /// account. Which it is depends on `has_account`.
    password: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct AcceptedView {
    #[schema(value_type = uuid::Uuid)]
    tenant: spa_types::TenantId,
    /// Ready to use — accepting signs you in.
    token: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    expires_at: Timestamp,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Invite somebody, leaving the password to them.
///
/// The `token` comes back **once** and is stored nowhere — pass it on however
/// you already talk to this person. A lost link is re-issued, not recovered.
#[utoipa::path(
    post,
    path = "/v1/tenants/{slug}/invitations",
    tag = "invitations",
    params(("slug" = String, Path, description = "The tenant's name in URLs.")),
    request_body = NewInvitation,
    responses(
        (status = CREATED, body = InvitationCreated),
        (status = BAD_REQUEST, description = "No such role", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = CONFLICT, description = "Already a member here", body = Problem),
    ),
)]
async fn invite(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewInvitation>,
) -> Result<(StatusCode, Json<InvitationCreated>), Problem> {
    let role: Role = body
        .role
        .parse()
        .map_err(|_| bad_request(crate::messages::UNKNOWN_ROLE, "role", &body.role, locale))?;

    let (invitation, token) = state
        .control
        .invite(
            tenant.db.tenant(),
            body.handle,
            role,
            tenant.session.identity,
        )
        .await
        .map_err(|e| invitation_problem(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(InvitationCreated {
            id: invitation.id.to_string(),
            handle: invitation.handle,
            role: invitation.role.as_str(),
            expires_at: invitation.expires_at,
            accept_path: format!("/v1/invitations/{}/acceptance", token.expose()),
            token: token.expose().to_owned(),
        }),
    ))
}

/// Invitations sent and not yet taken up.
#[utoipa::path(
    get,
    path = "/v1/tenants/{slug}/invitations",
    tag = "invitations",
    params(("slug" = String, Path, description = "The tenant's name in URLs.")),
    responses(
        (status = OK, description = "Pending invitations. The tokens are not here — they were shown once.", body = Vec<InvitationView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn list_invitations(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Vec<InvitationView>>, Problem> {
    let invitations = state
        .control
        .invitations(tenant.db.tenant())
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok(Json(
        invitations
            .into_iter()
            .map(|i| InvitationView {
                id: i.id.to_string(),
                handle: i.handle,
                role: i.role.as_str(),
                expires_at: i.expires_at,
                created_at: i.created_at,
            })
            .collect(),
    ))
}

/// Withdraw an invitation before it is taken up.
#[utoipa::path(
    delete,
    path = "/v1/tenants/{slug}/invitations/{invitation}",
    tag = "invitations",
    params(
        ("slug" = String, Path, description = "The tenant's name in URLs."),
        ("invitation" = uuid::Uuid, Path, description = "From `GET /v1/tenants/{slug}/invitations`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Withdrawn. The token no longer works."),
        (status = BAD_REQUEST, description = "Not a well-formed id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
    ),
)]
async fn revoke_invitation(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(params): Path<std::collections::HashMap<String, String>>,
) -> Result<StatusCode, Problem> {
    let raw = params.get("invitation").map_or("", String::as_str);
    let id: uuid::Uuid = raw
        .parse()
        .map_err(|_| bad_request(crate::messages::INVALID_ID, "id", raw, locale))?;

    state
        .control
        .revoke_invitation(
            tenant.db.tenant(),
            id,
            Actor::identity(tenant.session.identity),
        )
        .await
        .map_err(|e| ApiError::Access(e).into_problem(locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// What an invitation is for, before accepting it.
///
/// Unauthenticated: whoever is accepting has no account yet, and the token *is*
/// the credential. A link that does not say what it is for is a link people
/// accept without reading.
#[utoipa::path(
    get,
    path = "/v1/invitations/{token}",
    tag = "invitations",
    security(),
    params(("token" = String, Path, description = "From the invitation link.")),
    responses(
        (status = OK, body = PendingView),
        (status = NOT_FOUND, description = "No such token, or a spent or expired one — the same answer for all three", body = Problem),
    ),
)]
async fn show_invitation(
    State(state): State<AppState>,
    Language(locale): Language,
    Path(token): Path<String>,
) -> Result<Json<PendingView>, Problem> {
    let pending = state
        .control
        .pending_invitation(&token)
        .await
        .map_err(|e| invitation_problem(&e, locale))?;

    Ok(Json(PendingView {
        company: pending.company,
        slug: pending.slug,
        handle: pending.handle,
        role: pending.role.as_str(),
        expires_at: pending.expires_at,
        has_account: pending.has_account,
    }))
}

/// Take up an invitation.
///
/// `password` is the existing account's password when `has_account` was `true`,
/// and the one being chosen otherwise. Either way the response is a working
/// session: accepting signs you in.
#[utoipa::path(
    post,
    path = "/v1/invitations/{token}/acceptance",
    tag = "invitations",
    security(),
    params(("token" = String, Path, description = "From the invitation link.")),
    request_body = Acceptance,
    responses(
        (status = CREATED, body = AcceptedView),
        (status = BAD_REQUEST, description = "A password under 12 characters", body = Problem),
        (status = UNAUTHORIZED, description = "The invitation was real; the password for the existing account was not", body = Problem),
        (status = NOT_FOUND, description = "No such token, or a spent or expired one", body = Problem),
        (status = CONFLICT, description = "Already a member there", body = Problem),
    ),
)]
async fn accept_invitation(
    State(state): State<AppState>,
    Language(locale): Language,
    Path(token): Path<String>,
    Json(body): Json<Acceptance>,
) -> Result<(StatusCode, Json<AcceptedView>), Problem> {
    // Checked before the token is looked up, so a short password is the same
    // answer whether or not the invitation was real.
    if body.password.chars().count() < MIN_PASSWORD {
        return Err(ApiError::BadRequest(
            spa_i18n::Message::new(crate::messages::PASSWORD_TOO_SHORT).with(
                "n",
                spa_i18n::MessageArg::Count(i64::try_from(MIN_PASSWORD).unwrap_or(i64::MAX)),
            ),
        )
        .into_problem(locale));
    }

    let accepted = state
        .control
        .accept_invitation(&token, body.password)
        .await
        .map_err(|e| invitation_problem(&e, locale))?;

    Ok((
        StatusCode::CREATED,
        Json(AcceptedView {
            tenant: accepted.tenant,
            token: accepted.token.expose().to_owned(),
            expires_at: accepted.session.expires_at,
        }),
    ))
}

// ---------------------------------------------------------------------------

fn invitation_problem(error: &InvitationError, locale: Locale) -> Problem {
    let status = match error {
        // 404, not 400: a bad token and a spent one are the same answer, and
        // that answer is "there is nothing here".
        InvitationError::NotValid => StatusCode::NOT_FOUND,
        // The invitation was real; the credential was not.
        InvitationError::WrongPassword => StatusCode::UNAUTHORIZED,
        InvitationError::Member(spa_control::MemberError::AlreadyAMember(_)) => {
            StatusCode::CONFLICT
        }
        other => {
            tracing::error!(error = %other, "invitation failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };

    let message = if status.is_server_error() {
        spa_i18n::Message::new(spa_control::messages::INTERNAL)
    } else {
        error.message()
    };

    Problem::new(status, &message, locale, &crate::catalog::CATALOG)
}
