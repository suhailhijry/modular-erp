use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{
        api_keys::*, audience::*, authenticator::*, authorization::*, crypto::*, identity::*,
        login_flow::*, otp_store::*,
    },
    event_sourcing::{EventStore, handle_command, load_aggregate},
};

/// Mount WITHOUT session_layer (these CREATE sessions). CORS for
/// browsers; optional_api_key_layer for app identification.
pub fn login_routes() -> Router<AuthHttpState> {
    Router::new()
        .route("/auth/otp/request", post(otp_request))
        .route("/auth/otp/verify", post(otp_verify))
        .route("/auth/password/login", post(password_login))
}

/// Mount UNDER session_layer.
pub fn session_routes() -> Router<AuthHttpState> {
    Router::new()
        .route("/auth/logout", post(logout))
        .route("/me/sessions", get(my_sessions))
        .route("/me/sessions/revoke-all", post(revoke_my_sessions))
        .route("/admin/identities", post(create_identity))
        .route(
            "/admin/identities/{identity_id}/suspend",
            post(suspend_identity),
        )
        .route(
            "/admin/identities/{identity_id}/reinstate",
            post(reinstate_identity),
        )
        .route(
            "/admin/identities/{identity_id}/sessions/revoke-all",
            post(revoke_identity_sessions),
        )
        .route(
            "/admin/identities/{identity_id}/authenticators",
            post(register_authenticator),
        )
        .route(
            "/admin/authenticators/{authenticator_id}/disable",
            post(disable_authenticator),
        )
        .route(
            "/admin/authenticators/{authenticator_id}/reenable",
            post(reenable_authenticator),
        )
        .route(
            "/admin/authenticators/{authenticator_id}/move",
            post(move_authenticator),
        )
        .route("/admin/api-keys", post(issue_api_key))
        .route("/admin/api-keys/{key_id}/revoke", post(revoke_api_key))
        .route(
            "/admin/subjects/{subject_id}/grants",
            post(grant_permission),
        )
        .route(
            "/admin/subjects/{subject_id}/grants/{permission}",
            delete(revoke_permission),
        )
        .route(
            "/admin/identities/{identity_id}/groups/{group_id}",
            post(join_group).delete(leave_group),
        )
        .route(
            "/admin/subjects/{subject_id}/effective-scale",
            post(effective_scale),
        )
        .route("/resources/{resource_type}/{resource_id}/acl", get(get_acl))
        .route(
            "/resources/{resource_type}/{resource_id}/acl/co-owners",
            post(add_co_owner),
        )
        .route(
            "/resources/{resource_type}/{resource_id}/acl/co-owners/{identity_id}",
            delete(remove_co_owner),
        )
        .route(
            "/resources/{resource_type}/{resource_id}/acl/identities/{identity_id}/scale",
            put(set_identity_scale),
        )
}

#[derive(Clone)]
pub struct AuthHttpState {
    pub services: AuthServices, // store + sessions (middleware also uses this)
    pub flow: Arc<LoginFlow>,
    pub authz: Arc<Authorizer>,
}

#[derive(Clone)]
pub struct AuthServices {
    pub store: Arc<dyn EventStore>,
    pub sessions: Arc<dyn super::session_store::SessionStore>,
}

/// L0: machine gate - applied to MACHINE ROUTE GROUPS only, not
/// globally. The caller-class split:
///
///   Browser SPA:   CORS allowlist + cookie session + CSRF. NO api key -
///                  a browser cannot keep a secret; any key shipped to
///                  it is public within seconds of someone opening
///                  DevTools, so requiring one is theater.
///   Native apps:   key identifies the APP (telemetry, version gating,
///                  coarse abuse control) - but note an embedded key is
///                  extractable from any binary, so the SESSION remains
///                  the security boundary for human actions.
///   Third parties: key is a REAL boundary (servers can keep secrets);
///                  webhook routes additionally verify the provider's
///                  HMAC signature.
///
/// Router composition:
///
///   let machine_api = Router::new()
///       .merge(webhook_routes())          // Moyasar/Tabby/Tamara callbacks
///       .merge(integration_routes())
///       .layer(middleware::from_fn(api_key_layer));   // key REQUIRED
///
///   let human_api = Router::new()
///       .merge(invoice_routes())...
///       .layer(middleware::from_fn(session_layer))    // session does security
///       .layer(cors_layer());                         // explicit origin allowlist
///
/// Native apps calling the human API MAY send X-Api-Key for
/// identification; `optional_api_key_layer` records it without
/// requiring it.
pub async fn api_key_layer(
    axum::extract::Extension(services): axum::extract::Extension<AuthServices>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(presented) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some((prefix, secret)) = presented.split_once('.') else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let key = load_aggregate::<ApiKey>(services.store.as_ref(), prefix)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !key.verify(secret) {
        // Unknown prefix and wrong secret are indistinguishable - no
        // prefix-probing oracle.
        return Err(StatusCode::UNAUTHORIZED);
    }
    let Some(principal) = key.principal().cloned() else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    request.extensions_mut().insert(MachineContext {
        api_key_id: prefix.to_string(),
        principal,
        scopes: key.scopes().to_vec(),
    });
    Ok(next.run(request).await)
}

/// Optional variant for HUMAN routes: a valid key attaches
/// MachineContext (app identification in request_meta/telemetry), an
/// absent one is fine, an INVALID one is still rejected - a client
/// presenting a wrong key is misconfigured or probing, and silence
/// would hide it.
pub async fn optional_api_key_layer(
    axum::extract::Extension(services): axum::extract::Extension<AuthServices>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(presented) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) else {
        return Ok(next.run(request).await); // no key = browser or bare client: fine here
    };
    let Some((prefix, secret)) = presented.split_once('.') else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let key = load_aggregate::<ApiKey>(services.store.as_ref(), prefix)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !key.verify(secret) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if let Some(principal) = key.principal().cloned() {
        request.extensions_mut().insert(MachineContext {
            api_key_id: prefix.to_string(),
            principal,
            scopes: key.scopes().to_vec(),
        });
    }
    Ok(next.run(request).await)
}

pub async fn session_layer(
    axum::extract::Extension(services): axum::extract::Extension<AuthServices>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let cookie_val = |name: &str| {
        headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split("; ")
                    .find_map(|kv| kv.strip_prefix(&format!("{name}=")[..]).map(str::to_owned))
            })
    };
    let session_cookie = cookie_val("__Host-session");

    let (session_id, via_cookie) = match (bearer, session_cookie.as_deref()) {
        (Some(b), _) => (b.to_owned(), false),
        (None, Some(cv)) => (cv.to_owned(), true),
        (None, None) => return Ok(next.run(request).await), // anonymous; route guards decide
    };

    // CSRF double-submit: mutating cookie-authenticated requests must
    // echo the csrf cookie's value in a header. Bearer flows are exempt
    // (no ambient credential = no CSRF).
    if via_cookie && !request.method().is_safe() {
        let header_token = headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
        let cookie_token = cookie_val("__Host-csrf");
        let ok = matches!(
            (header_token, cookie_token.as_deref()),
            (Some(h), Some(ck)) if super::crypto::constant_time_eq(h, ck)
        );
        if !ok {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let Some(record) = services
        .sessions
        .resolve(&session_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::UNAUTHORIZED); // presented-but-invalid fails loudly
    };

    let identity = load_aggregate::<Identity>(services.store.as_ref(), &record.identity_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !identity.is_active() {
        // Suspension takes effect at the NEXT request even with a live
        // session - the check belongs here, not only at login.
        return Err(StatusCode::UNAUTHORIZED);
    }

    request.extensions_mut().insert(AuthContext {
        identity_id: record.identity_id,
        audience: record.audience,
        is_system: identity.is_system(),
        session_id,
    });
    Ok(next.run(request).await)
}

pub struct Auth(pub AuthContext);

impl<S: Send + Sync> FromRequestParts<S> for Auth {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .map(Auth)
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

fn meta_json(auth: Option<&AuthContext>) -> serde_json::Value {
    match auth {
        Some(a) => {
            serde_json::json!({ "identity_id": a.identity_id, "session_audience": format!("{:?}", a.audience) })
        }
        None => serde_json::json!({}),
    }
}

fn internal(e: impl Into<anyhow::Error>) -> Response {
    tracing::error!(error = %e.into(), "auth endpoint internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn otp_err(e: OtpError) -> Response {
    let (status, msg) = match &e {
        OtpError::Unusable => (StatusCode::BAD_REQUEST, "login method not available"),
        OtpError::Invalid => (StatusCode::UNAUTHORIZED, "code invalid or expired"),
        OtpError::TooManyAttempts => (
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts - request a new code",
        ),
        OtpError::Other(_) => return internal(anyhow::anyhow!("{e}")),
    };
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodBody {
    Phone,
    Email,
    Username,
}

impl From<MethodBody> for AuthMethod {
    fn from(m: MethodBody) -> Self {
        match m {
            MethodBody::Phone => AuthMethod::Phone,
            MethodBody::Email => AuthMethod::Email,
            MethodBody::Username => AuthMethod::Username,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudienceBody {
    Client,
    Employee,
    Admin,
}

impl From<AudienceBody> for Audience {
    fn from(a: AudienceBody) -> Self {
        match a {
            AudienceBody::Client => Audience::Client,
            AudienceBody::Employee => Audience::Employee,
            AudienceBody::Admin => Audience::Admin,
        }
    }
}

/// How the client wants the session delivered. Browsers say Cookie
/// (Set-Cookie + CSRF cookie); apps say Token (session id in the body,
/// sent back as Bearer).
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Cookie,
    Token,
}

#[derive(Deserialize)]
pub struct OtpRequestBody {
    pub method: MethodBody,
    pub identifier: String,
    pub audience: AudienceBody,
}

async fn otp_request(
    State(state): State<AuthHttpState>,
    Json(body): Json<OtpRequestBody>,
) -> Response {
    match state
        .flow
        .otp_request(body.method.into(), &body.identifier, body.audience.into())
        .await
    {
        // Uniform response whether a code was sent, suppressed by the
        // resend window, or the identifier is unknown - no enumeration.
        // The Some(code) case is where the caller hands off to SMS/email
        // delivery; the code itself NEVER appears in the response.
        Ok(maybe_code) => {
            if let Some(_code) = maybe_code {
                // TODO deployment seam: enqueue delivery(_code) to the
                // SMS/email provider here.
            }
            Json(serde_json::json!({ "status": "if this identifier is registered, a code has been sent" })).into_response()
        }
        Err(OtpError::Unusable) => otp_err(OtpError::Unusable),
        Err(e) => otp_err(e),
    }
}

#[derive(Deserialize)]
pub struct OtpVerifyBody {
    pub method: MethodBody,
    pub identifier: String,
    pub code: String,
    pub audience: AudienceBody,
    pub transport: Transport,
}

#[derive(Serialize)]
pub struct TokenSessionResponse {
    pub session_token: String,
}

fn session_response(session_id: String, transport: Transport) -> Response {
    match transport {
        Transport::Token => Json(TokenSessionResponse {
            session_token: session_id,
        })
        .into_response(),
        Transport::Cookie => {
            // __Host- prefix: Secure, Path=/, no Domain - the strictest
            // cookie class. Session cookie is HttpOnly (JS never reads
            // it); CSRF cookie is NOT HttpOnly on purpose - the SPA must
            // read it to echo it in X-Csrf-Token (double submit).
            let csrf = super::crypto::generate_token();
            let mut response = Json(serde_json::json!({ "status": "ok" })).into_response();
            let headers = response.headers_mut();
            headers.append(
                header::SET_COOKIE,
                HeaderValue::from_str(&format!(
                    "__Host-session={session_id}; Path=/; Secure; HttpOnly; SameSite=Lax"
                ))
                .expect("valid cookie"),
            );
            headers.append(
                header::SET_COOKIE,
                HeaderValue::from_str(&format!("__Host-csrf={csrf}; Path=/; Secure; SameSite=Lax"))
                    .expect("valid cookie"),
            );
            response
        }
    }
}

async fn otp_verify(
    State(state): State<AuthHttpState>,
    Json(body): Json<OtpVerifyBody>,
) -> Response {
    match state
        .flow
        .otp_verify(
            body.method.into(),
            &body.identifier,
            &body.code,
            body.audience.into(),
        )
        .await
    {
        Ok(session_id) => session_response(session_id, body.transport),
        Err(e) => otp_err(e),
    }
}

#[derive(Deserialize)]
pub struct PasswordLoginBody {
    pub username: String,
    pub password: String,
    pub audience: AudienceBody,
    pub transport: Transport,
}

async fn password_login(
    State(state): State<AuthHttpState>,
    Json(body): Json<PasswordLoginBody>,
) -> Response {
    match state
        .flow
        .password_login(&body.username, &body.password, body.audience.into())
        .await
    {
        Ok(session_id) => session_response(session_id, body.transport),
        Err(e) => otp_err(e),
    }
}

/// Revokes the CURRENT session (from AuthContext) and clears cookies.
async fn logout(State(state): State<AuthHttpState>, Auth(auth): Auth) -> Response {
    if let Err(e) = state.services.sessions.revoke(&auth.session_id).await {
        return internal(e);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    // Expire both cookies; harmless for token clients.
    for cookie in [
        "__Host-session=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0",
        "__Host-csrf=; Path=/; Secure; SameSite=Lax; Max-Age=0",
    ] {
        response
            .headers_mut()
            .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
    }
    response
}

// =======================================================================
// Self-service sessions
// =======================================================================

#[derive(Serialize)]
pub struct SessionRow {
    pub audience: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

async fn my_sessions(State(state): State<AuthHttpState>, Auth(auth): Auth) -> Response {
    match state
        .services
        .sessions
        .sessions_for_identity(&auth.identity_id)
        .await
    {
        Ok(sessions) => Json(
            sessions
                .into_iter()
                .map(|s| SessionRow {
                    audience: format!("{:?}", s.audience),
                    created_at: s.created_at,
                    last_seen_at: s.last_seen_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

async fn revoke_my_sessions(State(state): State<AuthHttpState>, Auth(auth): Auth) -> Response {
    match state
        .services
        .sessions
        .revoke_all_for_identity(&auth.identity_id)
        .await
    {
        Ok(count) => Json(serde_json::json!({ "revoked": count })).into_response(),
        Err(e) => internal(e),
    }
}

// =======================================================================
// Identities (admin)
// =======================================================================

async fn require_admin(
    state: &AuthHttpState,
    auth: &AuthContext,
    permission: &str,
    scale: Scale,
) -> Result<(), Response> {
    state
        .authz
        .require(auth, permission, scale, &AuthzFacts::default())
        .await
        .map_err(|e| {
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        })
}

#[derive(Deserialize)]
pub struct CreateIdentityBody {
    #[serde(default)]
    pub is_system: bool,
}

#[derive(Serialize)]
pub struct CreatedIdentityResponse {
    pub identity_id: String,
}

async fn create_identity(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Json(body): Json<CreateIdentityBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.identities", Scale::Create).await {
        return r;
    }
    // Creating a SYSTEM identity is the most privileged act in the
    // system - only existing system identities may do it, regardless of
    // any granted permission.
    if body.is_system && !auth.is_system {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "only system identities may create system identities" }))).into_response();
    }
    let identity_id = uuid::Uuid::new_v4().to_string();
    match handle_command::<Identity>(
        state.services.store.as_ref(),
        None,
        &identity_id,
        IdentityCommand::Create {
            is_system: body.is_system,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(CreatedIdentityResponse { identity_id }),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct SuspendBody {
    pub reason: String,
}

async fn suspend_identity(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(identity_id): Path<String>,
    Json(body): Json<SuspendBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.identities", Scale::Edit).await {
        return r;
    }
    match handle_command::<Identity>(
        state.services.store.as_ref(),
        None,
        &identity_id,
        IdentityCommand::Suspend {
            reason: body.reason,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => {
            // Suspension should bite immediately, not at next-request-
            // per-session-expiry: kill the live sessions too.
            if let Err(e) = state
                .services
                .sessions
                .revoke_all_for_identity(&identity_id)
                .await
            {
                tracing::warn!(error = %e, identity_id, "suspended but session revocation failed - middleware still blocks on next request");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal(e),
    }
}

async fn reinstate_identity(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(identity_id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.identities", Scale::Edit).await {
        return r;
    }
    match handle_command::<Identity>(
        state.services.store.as_ref(),
        None,
        &identity_id,
        IdentityCommand::Reinstate,
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

async fn revoke_identity_sessions(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(identity_id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.identities", Scale::Edit).await {
        return r;
    }
    match state
        .services
        .sessions
        .revoke_all_for_identity(&identity_id)
        .await
    {
        Ok(count) => Json(serde_json::json!({ "revoked": count })).into_response(),
        Err(e) => internal(e),
    }
}

// =======================================================================
// Authenticators (admin)
// =======================================================================

#[derive(Deserialize)]
pub struct RegisterAuthenticatorBody {
    pub method: MethodBody,
    pub identifier: String,
    /// Username method only.
    pub secret: Option<String>,
}

async fn register_authenticator(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(identity_id): Path<String>,
    Json(body): Json<RegisterAuthenticatorBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.authenticators", Scale::Create).await {
        return r;
    }
    let method: AuthMethod = body.method.into();
    let aid = authenticator_id(method, &body.identifier);
    match handle_command::<Authenticator>(
        state.services.store.as_ref(),
        None,
        &aid,
        AuthenticatorCommand::Register {
            method,
            identity_id: identity_id.clone(),
            secret: body.secret,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "authenticator_id": aid })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct DisableBody {
    pub reason: String,
}

async fn disable_authenticator(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(authenticator_id): Path<String>,
    Json(body): Json<DisableBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.authenticators", Scale::Edit).await {
        return r;
    }
    match handle_command::<Authenticator>(
        state.services.store.as_ref(),
        None,
        &authenticator_id,
        AuthenticatorCommand::Disable {
            reason: body.reason,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

async fn reenable_authenticator(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(authenticator_id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.authenticators", Scale::Edit).await {
        return r;
    }
    match handle_command::<Authenticator>(
        state.services.store.as_ref(),
        None,
        &authenticator_id,
        AuthenticatorCommand::Reenable,
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MoveAuthenticatorBody {
    pub to_identity_id: String,
    pub reason: String,
}

async fn move_authenticator(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(authenticator_id): Path<String>,
    Json(body): Json<MoveAuthenticatorBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.authenticators", Scale::Edit).await {
        return r;
    }
    match handle_command::<Authenticator>(
        state.services.store.as_ref(),
        None,
        &authenticator_id,
        AuthenticatorCommand::MoveToIdentity {
            to_identity_id: body.to_identity_id,
            reason: body.reason,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

// =======================================================================
// API keys (admin)
// =======================================================================

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrincipalBody {
    FirstParty { app: String },
    ThirdParty { partner: String },
}

#[derive(Deserialize)]
pub struct IssueKeyBody {
    pub principal: PrincipalBody,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// "live" / "test" - lands in the key prefix.
    pub env_tag: String,
}

#[derive(Serialize)]
pub struct IssuedKeyResponse {
    pub key_id: String,
    /// Shown ONCE. Not retrievable afterward - only its hash is stored.
    pub api_key: String,
}

async fn issue_api_key(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Json(body): Json<IssueKeyBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.api_keys", Scale::Create).await {
        return r;
    }
    let generated = generate_api_key(&body.env_tag);
    let principal = match body.principal {
        PrincipalBody::FirstParty { app } => MachinePrincipal::FirstParty { app },
        PrincipalBody::ThirdParty { partner } => MachinePrincipal::ThirdParty { partner },
    };
    match handle_command::<ApiKey>(
        state.services.store.as_ref(),
        None,
        &generated.prefix,
        ApiKeyCommand::Issue {
            principal,
            secret_sha256: generated.secret_sha256.clone(),
            scopes: body.scopes,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(IssuedKeyResponse {
                key_id: generated.prefix,
                api_key: generated.presentable,
            }),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

async fn revoke_api_key(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(key_id): Path<String>,
    Json(body): Json<DisableBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.api_keys", Scale::Delete).await {
        return r;
    }
    match handle_command::<ApiKey>(
        state.services.store.as_ref(),
        None,
        &key_id,
        ApiKeyCommand::Revoke {
            reason: body.reason,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

// =======================================================================
// RBAC (admin). subject_id: "identity:{id}" or "group:{id}" - the
// grants registry aggregate id is "perms:{subject_id}".
// =======================================================================

#[derive(Deserialize)]
pub struct GrantBody {
    pub permission: String,
    pub scale: Scale,
    pub condition: Option<AuthzCondition>,
}

async fn grant_permission(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(subject_id): Path<String>,
    Json(body): Json<GrantBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.permissions", Scale::Create).await {
        return r;
    }
    match handle_command::<PermissionGrants>(
        state.services.store.as_ref(),
        None,
        &format!("perms:{subject_id}"),
        PermissionGrantsCommand::Grant {
            permission: body.permission,
            grant: ConditionalGrant {
                scale: body.scale,
                condition: body.condition,
            },
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn revoke_permission(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((subject_id, permission)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.permissions", Scale::Delete).await {
        return r;
    }
    match handle_command::<PermissionGrants>(
        state.services.store.as_ref(),
        None,
        &format!("perms:{subject_id}"),
        PermissionGrantsCommand::Revoke { permission },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

async fn join_group(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((identity_id, group_id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.permissions", Scale::Edit).await {
        return r;
    }
    match handle_command::<PermissionGrants>(
        state.services.store.as_ref(),
        None,
        &format!("perms:identity:{identity_id}"),
        PermissionGrantsCommand::JoinGroup { group_id },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

async fn leave_group(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((identity_id, group_id)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.permissions", Scale::Edit).await {
        return r;
    }
    match handle_command::<PermissionGrants>(
        state.services.store.as_ref(),
        None,
        &format!("perms:identity:{identity_id}"),
        PermissionGrantsCommand::LeaveGroup { group_id },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

/// Debug/audit: what scale would this subject get for a permission,
/// under supplied facts? Answers "why can/can't this person do X" in
/// one call instead of a support session.
#[derive(Deserialize)]
pub struct EffectiveScaleBody {
    pub permission: String,
    #[serde(default)]
    pub amount_minor: Option<i64>,
    #[serde(default)]
    pub branch_id: Option<String>,
}

async fn effective_scale(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path(subject_id): Path<String>,
    Json(body): Json<EffectiveScaleBody>,
) -> Response {
    if let Err(r) = require_admin(&state, &auth, "auth.permissions", Scale::Read).await {
        return r;
    }
    let facts = AuthzFacts {
        amount_minor: body.amount_minor,
        branch_id: body.branch_id,
        local_datetime: Some(chrono::Utc::now().naive_utc()),
        ..Default::default()
    };
    let grants = match load_aggregate::<PermissionGrants>(
        state.services.store.as_ref(),
        &format!("perms:{subject_id}"),
    )
    .await
    {
        Ok(g) => g,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "subject_id": subject_id,
        "permission": body.permission,
        "effective_scale": format!("{:?}", grants.scale_of(&body.permission, &facts)),
        "groups": grants.groups(),
    }))
    .into_response()
}

// =======================================================================
// Resource ACLs
// =======================================================================

#[derive(Serialize)]
pub struct AclResponse {
    pub owner_identity_id: String,
    pub co_owners: Vec<String>,
    pub overrides: std::collections::BTreeMap<String, String>,
}

async fn get_acl(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((resource_type, resource_id)): Path<(String, String)>,
) -> Response {
    // Unknown resource types 404 before touching anything - the
    // registry is the source of truth for what types exist.
    if state.authz.policies.get(&resource_type).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Reading an ACL requires a relation to the resource or admin read.
    let facts = AuthzFacts::default();
    if state
        .authz
        .require_on(&auth, &resource_type, &resource_id, Scale::Read, &facts)
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let acl = match load_aggregate::<ResourceAcl>(
        state.services.store.as_ref(),
        &format!("{resource_type}:{resource_id}"),
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    Json(AclResponse {
        owner_identity_id: acl.owner().to_string(),
        co_owners: acl.co_owners().iter().cloned().collect(),
        overrides: acl
            .overrides()
            .iter()
            .map(|(k, v)| (k.clone(), format!("{v:?}")))
            .collect(),
    })
    .into_response()
}

/// Only the OWNER or an admin may reshape an ACL - co-owners can use
/// the resource, not share it further.
async fn require_owner_or_admin(
    state: &AuthHttpState,
    auth: &AuthContext,
    resource_type: &str,
    resource_id: &str,
) -> Result<(), Response> {
    if state.authz.policies.get(resource_type).is_none() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    if auth.is_system {
        return Ok(());
    }
    let acl = load_aggregate::<ResourceAcl>(
        state.services.store.as_ref(),
        &format!("{resource_type}:{resource_id}"),
    )
    .await
    .map_err(|e| internal(e))?;
    if matches!(acl.relation(&auth.identity_id), Relation::Owner) {
        return Ok(());
    }
    require_admin(state, auth, "auth.acls", Scale::Edit).await
}

#[derive(Deserialize)]
pub struct CoOwnerBody {
    pub identity_id: String,
}

async fn add_co_owner(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((resource_type, resource_id)): Path<(String, String)>,
    Json(body): Json<CoOwnerBody>,
) -> Response {
    if let Err(r) = require_owner_or_admin(&state, &auth, &resource_type, &resource_id).await {
        return r;
    }
    match handle_command::<ResourceAcl>(
        state.services.store.as_ref(),
        None,
        &format!("{resource_type}:{resource_id}"),
        ResourceAclCommand::AddCoOwner {
            identity_id: body.identity_id,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

async fn remove_co_owner(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((resource_type, resource_id, identity_id)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = require_owner_or_admin(&state, &auth, &resource_type, &resource_id).await {
        return r;
    }
    match handle_command::<ResourceAcl>(
        state.services.store.as_ref(),
        None,
        &format!("{resource_type}:{resource_id}"),
        ResourceAclCommand::RemoveCoOwner { identity_id },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct SetScaleBody {
    pub scale: Scale,
}

async fn set_identity_scale(
    State(state): State<AuthHttpState>,
    Auth(auth): Auth,
    Path((resource_type, resource_id, identity_id)): Path<(String, String, String)>,
    Json(body): Json<SetScaleBody>,
) -> Response {
    if let Err(r) = require_owner_or_admin(&state, &auth, &resource_type, &resource_id).await {
        return r;
    }
    match handle_command::<ResourceAcl>(
        state.services.store.as_ref(),
        None,
        &format!("{resource_type}:{resource_id}"),
        ResourceAclCommand::SetIdentityScale {
            identity_id,
            scale: body.scale,
        },
        meta_json(Some(&auth)),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}
