//! The payroll module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Drafting is a `PUT` and approving is a `POST`
//!
//! Because drafting is idempotent on the run's id — send it again with a
//! different list and you get a different draft under the same run — and
//! approving is the act that happens once. The methods say which is which
//! without a reader having to know the module.

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
use erp_web::{After, Allowed, Language, ManageAccounts, Paged, PostEntries, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, bad_request, metadata, parse_id, require_module};

use crate::{PayrollError, Period};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_runs))
        .routes(routes!(get_run, draft_run))
        .routes(routes!(run_payslips))
        .routes(routes!(approve_run))
        .routes(routes!(payroll_accounts, set_payroll_accounts))
}

static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &ledger::CATALOG,
    &hr::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"period": "2026-05", "employees": ["EMP-0001", "EMP-0002"]}))]
struct NewDraft {
    /// `YYYY-MM`. The entry posts on the **last day of it**, not the day the
    /// run was approved: a February run approved in March belongs in February.
    period: String,
    /// Who is in the run.
    ///
    /// **The caller says, and that is deliberate.** Enumerating employees means
    /// reading `hr`'s read model, and a payroll run is money leaving the
    /// business — it must not be computed from a table that may be a second
    /// behind. List staff from `GET /v1/hr/employees` and send the ones to pay.
    employees: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PayrollCash {
    minor: i64,
    currency: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct RunRecord {
    id: String,
    period: String,
    gross: PayrollCash,
    deductions: PayrollCash,
    net: PayrollCash,
    people: i32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    drafted_at: Timestamp,
    /// Set once it has posted. **An approved run cannot be redrafted.**
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    approved_at: Option<Timestamp>,
    /// The journal entry it made. Read it at `GET /v1/ledger/entries/{entry}`.
    entry: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct PayslipRecord {
    employee: String,
    /// **As it was when the run was made.** A payslip says who it was for, and
    /// somebody who marries next month does not get a new copy of last month's.
    name: String,
    basic: PayrollCash,
    gross: PayrollCash,
    deductions: PayrollCash,
    net: PayrollCash,
}

#[derive(Debug, Serialize, ToSchema)]
struct RunAccepted {
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct Accounts {
    /// The cost. `5100` in every shipped chart.
    expense: String,
    /// What is owed to people until it is paid. `2200`.
    payable: String,
    /// What is held on their behalf. `2210`.
    withheld: String,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Payroll runs, newest period first.
#[utoipa::path(
    get,
    path = "/v1/payroll/runs",
    tag = "payroll",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<RunRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable payroll", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_runs(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(page): Query<After>,
) -> Result<Json<Paged<RunRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let runs = crate::runs(&mut conn, page.limit(50, 200), after.as_ref())
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(runs, record)))
}

/// One run.
#[utoipa::path(
    get,
    path = "/v1/payroll/runs/{run}",
    tag = "payroll",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("run" = String, Path, description = "Its id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = RunRecord),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_run(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<RunRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::run(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|r| Json(record(r)))
        .ok_or_else(|| not_found(&id, locale))
}

/// Draft a run: compute what everybody would be paid, and **post nothing**.
///
/// Drafting again replaces the previous draft, so a business can fix two
/// payslips and run it over. An approved run refuses — the entry is in the books
/// and the payslips are what people were told.
#[utoipa::path(
    put,
    path = "/v1/payroll/runs/{run}",
    tag = "payroll",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("run" = String, Path, description = "Your key for this run. `2026-05` reads well."),
    ),
    request_body = NewDraft,
    responses(
        (status = OK, description = "Drafted. Nothing has posted.", body = RunAccepted),
        (status = BAD_REQUEST, description = "Not a month, or nobody to pay", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable payroll", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Somebody is not on the books or has no salary, or the run is already approved", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn draft_run(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewDraft>,
) -> Result<Json<RunAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let run = parse_id(&id, locale)?;
    let period = Period::parse(&body.period)
        .map_err(|e| bad_request(crate::messages::NOT_A_PERIOD, "period", &e.0, locale))?;
    let employees = body
        .employees
        .iter()
        .map(|e| parse_id(e, locale))
        .collect::<Result<Vec<_>, _>>()?;

    let committed = crate::draft_run(
        &tenant.db,
        &run,
        period,
        &employees,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(RunAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// The payslips in a run.
#[utoipa::path(
    get,
    path = "/v1/payroll/runs/{run}/payslips",
    tag = "payroll",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("run" = String, Path, description = "Its id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<PayslipRecord>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn run_payslips(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<Vec<PayslipRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let slips = crate::payslips(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(
        slips
            .into_iter()
            .map(|s| PayslipRecord {
                employee: s.employee,
                name: s.name,
                basic: money(s.basic),
                gross: money(s.gross),
                deductions: money(s.deductions),
                net: money(s.net),
            })
            .collect(),
    ))
}

/// Approve a run, which **posts it**.
///
/// The entry is dated to the last day of the period. Approving an approved run
/// reports the entry it already made rather than posting a second one.
#[utoipa::path(
    post,
    path = "/v1/payroll/runs/{run}/approval",
    tag = "payroll",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("run" = String, Path, description = "Its id."),
    ),
    responses(
        (status = OK, description = "Posted. Already-approved is the same answer.", body = RunAccepted),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such run", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "The period is closed, or the ledger refused it", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn approve_run(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<RunAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let run = parse_id(&id, locale)?;

    let committed = crate::approve_run(&tenant.db, &run, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(RunAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Where payroll posts.
#[utoipa::path(
    get,
    path = "/v1/payroll/posting-accounts",
    tag = "payroll",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    responses(
        (status = OK, body = Accounts),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn payroll_accounts(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<Accounts>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let accounts = crate::PostingAccounts::resolve(&mut conn)
        .await
        .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;

    Ok(Json(Accounts {
        expense: accounts.expense.to_string(),
        payable: accounts.payable.to_string(),
        withheld: accounts.withheld.to_string(),
    }))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/payroll/posting-accounts",
    tag = "payroll",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = Accounts,
    responses(
        (status = NO_CONTENT, description = "Stored."),
        (status = BAD_REQUEST, description = "An unusable account code", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_payroll_accounts(
    tenant: Allowed<ManageAccounts>,
    Language(locale): Language,
    Json(body): Json<Accounts>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let accounts = crate::PostingAccounts {
        expense: parse_id(&body.expense, locale)?,
        payable: parse_id(&body.payable, locale)?,
        withheld: parse_id(&body.withheld, locale)?,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        crate::PostingAccounts::KEY,
        &accounts,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn money(amount: erp_types::Money) -> PayrollCash {
    PayrollCash {
        minor: amount.minor(),
        currency: amount.currency().to_string(),
    }
}

fn record(r: crate::RunSummary) -> RunRecord {
    RunRecord {
        id: r.id,
        period: r.period,
        gross: money(r.gross),
        deductions: money(r.deductions),
        net: money(r.net),
        people: r.people,
        drafted_at: r.drafted_at,
        approved_at: r.approved_at,
        entry: r.entry,
    }
}

fn not_found(id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(crate::messages::NO_SUCH_RUN)
            .with("id", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "payroll read failed");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<PayrollError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                PayrollError::NoSuchRun(_) => StatusCode::NOT_FOUND,
                PayrollError::NobodyToPay | PayrollError::Period(_) => StatusCode::BAD_REQUEST,
                // Well-formed, refused on the state of the world.
                _ => StatusCode::UNPROCESSABLE_ENTITY,
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
            tracing::error!(error = %other, "payroll command failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                erp_i18n::Message::new(crate::messages::DATABASE),
            )
        }
    };
    Problem::new(status, &message, locale, &CATALOG)
}
