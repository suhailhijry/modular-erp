//! The routes that exist today.
//!
//! Two of them, plus health. That is the point: they prove the whole path —
//! parse, authenticate, enter a tenant, answer in the caller's language — with
//! nothing domain-specific in it. Module routes mount alongside when modules
//! exist.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::extract::{Allowed, Authenticated, Language, Read};
use crate::problem::Problem;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", routing::get(health))
        .route("/v1/sessions", routing::post(log_in))
        .route("/v1/sessions/current", routing::delete(log_out))
        .route("/v1/tenants/{slug}", routing::get(tenant))
        // Module routes mount alongside. One module today; see `ledger_routes`.
        .merge(crate::signup::routes())
        .merge(crate::members::routes())
        .merge(crate::ledger_routes::routes())
        .merge(crate::sales_routes::routes())
        .with_state(state)
}

/// Liveness. Deliberately does not touch the database — a health check that
/// fails when the database is slow takes the fleet out during a slow query.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

#[derive(Debug, Deserialize)]
struct Credentials {
    handle: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct SessionCreated {
    token: String,
    expires_at: spa_types::Timestamp,
}

async fn log_in(
    State(state): State<AppState>,
    Language(locale): Language,
    Json(credentials): Json<Credentials>,
) -> Result<impl IntoResponse, Problem> {
    let (token, session) = state
        .control
        .log_in(&credentials.handle, &credentials.password)
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale))?;

    Ok((
        StatusCode::CREATED,
        Json(SessionCreated {
            token: token.expose().to_owned(),
            expires_at: session.expires_at,
        }),
    ))
}

async fn log_out(
    State(state): State<AppState>,
    Language(locale): Language,
    auth: Authenticated,
) -> Result<StatusCode, Problem> {
    state
        .control
        .log_out(&auth.token)
        .await
        .map_err(|e| ApiError::Auth(e).into_problem(locale))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
struct TenantView {
    id: spa_types::TenantId,
    /// What the caller may do here, so a client can hide what it must not
    /// offer. The server refuses regardless — this is for the buttons.
    role: Option<&'static str>,
    modules: Vec<String>,
}

/// Proves the whole path: `Allowed<Read>` in the signature means membership was
/// checked *and* the role permits it, and there is no way to obtain one that
/// skips either.
async fn tenant(tenant: Allowed<Read>) -> Json<TenantView> {
    Json(TenantView {
        id: tenant.db.tenant(),
        role: tenant.db.role().map(spa_control::Role::as_str),
        modules: tenant
            .db
            .modules()
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
    })
}
