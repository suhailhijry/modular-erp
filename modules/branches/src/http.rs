//! The branches module's HTTP surface.
//!
//! Translation only, like every module's.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{After, Allowed, IdempotencyKey, Language, ManageTenant, Paged, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, creating, metadata, parse_id, require_module};

use crate::{Address, BranchError, Details};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_branches, open_branch))
        .routes(routes!(get_branch, amend_branch))
        .routes(routes!(close_branch, reopen_branch))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct BranchAddress {
    street: String,
    #[serde(default)]
    building: Option<String>,
    #[serde(default)]
    district: Option<String>,
    city: String,
    #[serde(default)]
    postal_code: Option<String>,
    /// ISO 3166-1 alpha-2, two letters. Printed on every document this branch
    /// issues, which is why it is checked rather than stored as typed.
    country: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "فرع العليا",
    "name_latin": "Olaya",
    "address": { "street": "طريق الملك فهد", "city": "الرياض", "country": "SA" }
}))]
struct NewBranch {
    name: String,
    #[serde(default)]
    name_latin: Option<String>,
    address: BranchAddress,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct ClosingIt {
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BranchRecord {
    id: String,
    name: String,
    name_latin: Option<String>,
    address: BranchAddress,
    /// Set when it stopped trading. **It keeps everything it traded**, so a
    /// report over last year still names it.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    closed_at: Option<Timestamp>,
    closed_why: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    opened_on: Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct BranchAccepted {
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BranchQuery {
    #[serde(flatten)]
    page: After,
    /// Include the ones that have stopped trading.
    #[serde(default)]
    closed: bool,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Places, by name.
#[utoipa::path(
    get,
    path = "/v1/branches",
    tag = "branches",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("closed" = Option<bool>, Query, description = "Include the ones that have stopped trading."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<BranchRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable branches", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn list_branches(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<BranchQuery>,
) -> Result<Json<Paged<BranchRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::branches(
        &mut conn,
        query.closed,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, record)))
}

/// Open a place to trade from.
#[utoipa::path(
    post,
    path = "/v1/branches",
    tag = "branches",
    request_body = NewBranch,
    responses(
        (status = CREATED, body = BranchAccepted),
        (status = BAD_REQUEST, description = "No name, no address, or a country that is not two letters", body = Problem),
        (status = CONFLICT, description = "That key already opened a different branch", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable branches", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn open_branch(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewBranch>,
) -> Result<(StatusCode, Json<BranchAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();

    let committed = crate::open_branch(
        &tenant.db,
        &id,
        &details(body.name, body.name_latin, body.address),
        body.at.unwrap_or_else(chrono::Utc::now),
        &creating(&tenant, &key),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(BranchAccepted {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/branches/{branch}",
    tag = "branches",
    params(("branch" = String, Path, description = "The key it was opened under.")),
    responses(
        (status = OK, body = BranchRecord),
        (status = NOT_FOUND, description = "No such branch, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_branch(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<BranchRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::branch(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|found| Json(record(found)))
        .ok_or_else(|| missing(&id, locale))
}

/// Change what is known about it.
#[utoipa::path(
    put,
    path = "/v1/branches/{branch}",
    tag = "branches",
    params(("branch" = String, Path, description = "The key it was opened under.")),
    request_body = NewBranch,
    responses(
        (status = OK, body = BranchAccepted),
        (status = BAD_REQUEST, description = "No name, no address, or a country that is not two letters", body = Problem),
        (status = NOT_FOUND, description = "No such branch", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn amend_branch(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewBranch>,
) -> Result<Json<BranchAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let branch = parse_id(&id, locale)?;

    let committed = crate::amend_branch(
        &tenant.db,
        &branch,
        &details(body.name, body.name_latin, body.address),
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BranchAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Stop it trading. It keeps every document it issued.
#[utoipa::path(
    post,
    path = "/v1/branches/{branch}/closure",
    tag = "branches",
    params(("branch" = String, Path, description = "The key it was opened under.")),
    request_body = ClosingIt,
    responses(
        (status = OK, body = BranchAccepted),
        (status = NOT_FOUND, description = "No such branch", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn close_branch(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<ClosingIt>,
) -> Result<Json<BranchAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let branch = parse_id(&id, locale)?;

    let committed = crate::close_branch(
        &tenant.db,
        &branch,
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BranchAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Put it back into service. Removing the closure *is* the operation, which is
/// the shape `booking` and `prepaid` already use.
#[utoipa::path(
    delete,
    path = "/v1/branches/{branch}/closure",
    tag = "branches",
    params(("branch" = String, Path, description = "The key it was opened under.")),
    responses(
        (status = OK, body = BranchAccepted),
        (status = NOT_FOUND, description = "No such branch", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn reopen_branch(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<BranchAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let branch = parse_id(&id, locale)?;

    let committed =
        crate::reopen_branch(&tenant.db, &branch, chrono::Utc::now(), &metadata(&tenant))
            .await
            .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BranchAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn details(name: String, name_latin: Option<String>, address: BranchAddress) -> Details {
    Details {
        name,
        name_latin,
        address: Address {
            street: address.street,
            building: address.building,
            district: address.district,
            city: address.city,
            postal_code: address.postal_code,
            country: address.country,
        },
    }
}

fn record(b: crate::BranchSummary) -> BranchRecord {
    BranchRecord {
        id: b.id,
        name: b.name,
        name_latin: b.name_latin,
        address: BranchAddress {
            street: b.address.street,
            building: b.address.building,
            district: b.address.district,
            city: b.address.city,
            postal_code: b.address.postal_code,
            country: b.address.country,
        },
        closed_at: b.closed_at,
        closed_why: b.closed_why,
        opened_on: b.opened_on,
    }
}

fn missing(id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(crate::messages::NO_SUCH_BRANCH)
            .with("id", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<BranchError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                BranchError::NoSuchBranch(_) => StatusCode::NOT_FOUND,
                BranchError::Closed(_) => StatusCode::UNPROCESSABLE_ENTITY,
                BranchError::Details(_) => StatusCode::BAD_REQUEST,
            },
            rejection.message(),
        ),

        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }

        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),

        CommandError::Execute(ExecuteError::AlreadyExists { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::ALREADY_EXISTS),
        ),

        other => {
            tracing::error!(error = %other, "branches command failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
            )
        }
    };

    Problem::new(status, &message, locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    let status = match error {
        erp_tenant::PoolError::Overloaded { .. } => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(error = %error, "branches read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
