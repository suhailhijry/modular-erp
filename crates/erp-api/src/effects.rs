//! What this system promised and gave up on.
//!
//! # Why a route
//!
//! An effect goes to the outbox in the same transaction as the events that
//! caused it (D9) and is retried on a schedule; after the last attempt it is
//! **dead**: still in the table, still carrying its idempotency key, and
//! delivered by nothing. The first version counted dead letters in the health
//! check and offered no way back but hand-written SQL — so a provider outage
//! longer than the schedule turned every email, text and payment callback
//! promised during it into a permanent loss that the health check could only
//! keep reporting.
//!
//! These two routes are the way back: the list somebody reads, and the requeue
//! that puts one effect back in the queue with its attempts reset. The key
//! travels with it, so a delivery that in fact went through before the effect
//! was given up on is still not performed twice by a handler that honours it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_i18n::Locale;
use erp_types::Timestamp;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{Allowed, AppState, Json, Language, ManageTenant, Problem, bad_request, nudge};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_dead_letters))
        .routes(routes!(requeue_dead_letter))
}

/// Oldest first, and this many. A tenant with more dead letters than this has
/// a provider down, not a paging problem.
const PAGE: i64 = 200;

#[derive(Debug, Serialize, ToSchema)]
struct DeadLetterView {
    pub id: i64,
    /// The routing key — `email.send`, `webhook.post`.
    pub kind: String,
    pub idempotency_key: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    pub enqueued_at: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    pub dead_at: Timestamp,
}

/// Every effect given up on, oldest first.
#[utoipa::path(
    get,
    path = "/v1/effects/dead",
    tag = "service",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = Vec<DeadLetterView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_dead_letters(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
) -> Result<Json<Vec<DeadLetterView>>, Problem> {
    let mut conn = tenant.db.read().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    let dead = erp_eventlog::dead_letters(&mut conn, PAGE)
        .await
        .map_err(|e| unwell(e, locale))?;
    Ok(Json(
        dead.into_iter()
            .map(|d| DeadLetterView {
                id: d.id,
                kind: d.kind.to_string(),
                idempotency_key: d.idempotency_key,
                attempts: d.attempts,
                last_error: d.last_error,
                enqueued_at: d.enqueued_at,
                dead_at: d.dead_at,
            })
            .collect(),
    ))
}

/// Put one back in the queue, due now.
///
/// **Only a dead one.** A pending effect is already in the queue and a
/// delivered one was kept; there is nothing to put back, and the answer is
/// `404` rather than a silent no-op so a second click on the same row says so.
#[utoipa::path(
    post,
    path = "/v1/effects/dead/{id}/requeue",
    tag = "service",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("id" = i64, Path, description = "From `GET /v1/effects/dead`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Back in the queue; the worker has been nudged."),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No dead letter with that id", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn requeue_dead_letter(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    // Parsed here rather than by `Path<i64>`, so a bad id is a problem+json
    // like every other refusal and not axum's plain-text rejection.
    let id: i64 = id
        .parse()
        .map_err(|_| bad_request(erp_web::messages::INVALID_ID, "id", &id, locale))?;
    let mut conn = tenant.db.acquire().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    let found = erp_eventlog::requeue(&mut conn, id)
        .await
        .map_err(|e| unwell(e, locale))?;
    drop(conn);
    if !found {
        return Err(Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(erp_web::messages::NO_SUCH_DEAD_LETTER)
                .with("id", erp_i18n::MessageArg::text(id.to_string())),
            locale,
            &crate::CATALOG,
        ));
    }
    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::NO_CONTENT)
}

fn unwell(error: sqlx::Error, locale: Locale) -> Problem {
    tracing::warn!(%error, "the outbox could not be read");
    erp_web::ApiError::Access(erp_control::AccessError::Database(error))
        .into_problem(locale, &crate::CATALOG)
}
