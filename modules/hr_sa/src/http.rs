//! The `hr_sa` module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Both calculations answer, and neither records
//!
//! `GET`, not `POST`: asking what somebody's end-of-service comes to is a
//! question, and the answer changes as their salary and service do. Recording
//! that it was *paid* is a payroll line, and belongs where money moves.
//!
//! The answers carry the inputs back — the base the contribution was computed
//! on, the days served, the entitlement before any reduction. A figure somebody
//! is going to be asked to justify has to say where it came from.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_i18n::Locale;
use erp_types::Money;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Consistency, Language, ManageAccounts, Read};
use erp_web::{Json, Query, bad_request, parse_id, require_module};

use crate::gosi::{Footing, Schedule};
use crate::gratuity::Leaving;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(gosi_schedule, set_gosi_schedule))
        .routes(routes!(gosi_for))
        .routes(routes!(end_of_service_for))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &hr::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct SaCash {
    minor: i64,
    currency: String,
}

/// What to set the schedule to.
///
/// **Its own type, without `configured`.** The answer says whether anybody has
/// confirmed the numbers; the request cannot, because sending it *is* the
/// confirmation. One type for both would accept a field it ignored, which is a
/// caller believing they said something.
#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "saudi_employee_bp": 975,
    "saudi_employer_bp": 1175,
    "non_saudi_employee_bp": 0,
    "non_saudi_employer_bp": 200,
    "ceiling_minor": 4_500_000
}))]
struct NewSchedule {
    saudi_employee_bp: u32,
    saudi_employer_bp: u32,
    non_saudi_employee_bp: u32,
    non_saudi_employer_bp: u32,
    /// Absent means no ceiling.
    ceiling_minor: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(example = json!({
    "saudi_employee_bp": 975,
    "saudi_employer_bp": 1175,
    "non_saudi_employee_bp": 0,
    "non_saudi_employer_bp": 200,
    "ceiling_minor": 4_500_000,
    "configured": false
}))]
struct ScheduleView {
    /// Withheld from a Saudi employee's pay, in basis points.
    saudi_employee_bp: u32,
    /// Paid by the employer for a Saudi employee, in basis points.
    saudi_employer_bp: u32,
    /// Withheld from a non-Saudi employee. Zero: hazards cover is the
    /// employer's alone.
    non_saudi_employee_bp: u32,
    non_saudi_employer_bp: u32,
    /// The most of one month's pay contributions are computed on, in minor
    /// units. Absent means no ceiling.
    ceiling_minor: Option<i64>,
    /// **Whether these are the shipped defaults or something this tenant
    /// chose.**
    ///
    /// The shipped ones are a starting point, not an authority: GOSI's schedule
    /// is set by the authority and has changed. This field is what tells
    /// somebody they are still on numbers nobody has confirmed.
    configured: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct ContributionView {
    /// What the contribution was computed on, **after the ceiling**.
    base: SaCash,
    /// Withheld from their pay.
    employee: SaCash,
    /// Paid by the business on top of it.
    employer: SaCash,
    /// Both together, which is what reaches the authority.
    total: SaCash,
}

#[derive(Debug, Serialize, ToSchema)]
struct AwardView {
    /// Before any reduction for resigning.
    entitlement: SaCash,
    /// What is actually owed.
    payable: SaCash,
    /// How long they served.
    days: i64,
    /// The wage the award was computed on, so the figure can be checked.
    wage: SaCash,
}

#[derive(Debug, Deserialize)]
struct GosiQuery {
    /// The base: basic plus housing, in minor units. **Not the whole salary.**
    ///
    /// Which allowances count is a question about the contract and the
    /// authority's definition, so it is sent rather than guessed from a salary
    /// record.
    base: i64,
    currency: String,
    /// `saudi` or `non_saudi`.
    footing: String,
}

#[derive(Debug, Deserialize)]
struct GratuityQuery {
    /// `dismissed`, `resigned`, `in_full` or `for_cause`.
    reason: String,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// This tenant's GOSI schedule.
///
/// **`configured` says whether anybody has confirmed it.** The shipped defaults
/// are a starting point: the authority sets the schedule and has changed it.
#[utoipa::path(
    get,
    path = "/v1/hr_sa/gosi/schedule",
    tag = "hr_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    responses(
        (status = OK, body = ScheduleView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn gosi_schedule(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<ScheduleView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;

    // Read once, and report **whether it was stored** rather than only what it
    // came to: a tenant on the defaults needs to be told so.
    let stored = erp_eventlog::configuration::get::<Schedule>(&mut conn, Schedule::KEY)
        .await
        .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;
    let configured = stored.is_some();
    let schedule = stored.map_or_else(Schedule::default, |c| c.value);

    Ok(Json(view(schedule, configured)))
}

/// Set it.
#[utoipa::path(
    put,
    path = "/v1/hr_sa/gosi/schedule",
    tag = "hr_sa",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = NewSchedule,
    responses(
        (status = NO_CONTENT, description = "Stored."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_gosi_schedule(
    tenant: Allowed<ManageAccounts>,
    State(_state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewSchedule>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let schedule = Schedule {
        saudi_employee_bp: body.saudi_employee_bp,
        saudi_employer_bp: body.saudi_employer_bp,
        non_saudi_employee_bp: body.non_saudi_employee_bp,
        non_saudi_employer_bp: body.non_saudi_employer_bp,
        ceiling_minor: body.ceiling_minor,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    erp_eventlog::configuration::set(
        &mut conn,
        Schedule::KEY,
        &schedule,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;

    Ok(StatusCode::NO_CONTENT)
}

/// What contributions come to on a base.
///
/// A calculation and not a record: it answers, and stores nothing.
#[utoipa::path(
    get,
    path = "/v1/hr_sa/gosi/contribution",
    tag = "hr_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("base" = i64, Query, description = "Basic plus housing, in minor units. Not the whole salary."),
        ("currency" = String, Query, description = "ISO 4217."),
        ("footing" = String, Query, description = "`saudi` or `non_saudi`."),
    ),
    responses(
        (status = OK, body = ContributionView),
        (status = BAD_REQUEST, description = "Not a footing, or a currency that did not parse", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn gosi_for(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Query(query): Query<GosiQuery>,
) -> Result<Json<ContributionView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let currency = erp_types::CurrencyCode::new(&query.currency).map_err(|_| {
        bad_request(
            erp_web::messages::UNKNOWN_CURRENCY,
            "currency",
            &query.currency,
            locale,
        )
    })?;
    let footing = footing(&query.footing, locale)?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let schedule = Schedule::resolve(&mut conn)
        .await
        .map_err(|e| Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &CATALOG))?;
    drop(conn);

    let base = Money::from_minor(query.base, currency);
    let computed = crate::contribution(base, footing, schedule).map_err(|_| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::AMOUNT_OUT_OF_RANGE),
            locale,
            &CATALOG,
        )
    })?;
    let total = computed.total().map_err(|_| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::AMOUNT_OUT_OF_RANGE),
            locale,
            &CATALOG,
        )
    })?;

    Ok(Json(ContributionView {
        base: cash(computed.base),
        employee: cash(computed.employee),
        employer: cash(computed.employer),
        total: cash(total),
    }))
}

/// What somebody is owed at the end of their service.
///
/// Reads their salary and their dates from `hr`, so the figure and the record
/// cannot disagree — and answers with the inputs, because this is a number
/// somebody will be asked to justify.
#[utoipa::path(
    get,
    path = "/v1/hr_sa/employees/{employee}/end-of-service",
    tag = "hr_sa",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("reason" = String, Query, description = "`dismissed`, `resigned`, `in_full` or `for_cause`."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for `hr`'s read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, body = AwardView),
        (status = BAD_REQUEST, description = "Not a reason for leaving", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No salary recorded", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn end_of_service_for(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
    Query(query): Query<GratuityQuery>,
) -> Result<Json<AwardView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, hr::GROUP_NAME, locale)
        .await?;
    let employee = parse_id(&id, locale)?;
    let reason = leaving(&query.reason, locale)?;

    // **The projection, not the aggregate** (L7). The first version loaded the
    // employee stream, on the argument that this is money about to be paid and
    // a salary changed this morning has to count — which is the right instinct
    // aimed at the wrong operation. This is a *question*; recording that the
    // money was paid is a payroll line, and `payroll` does read the aggregate
    // inside the transaction that posts.
    //
    // A caller who has just changed a salary and wants the new figure passes
    // `?consistent_after=`, which is what it is for.
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let details = hr::pay_details(&mut conn, employee.as_str())
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    let Some(details) = details else {
        // **No row covers two different failures**, and a client needs them
        // apart: somebody who does not exist is a 404, and somebody real whose
        // salary nobody has entered is a 422 they can act on. One extra read,
        // and only on the path that is already going to fail.
        let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
        let known = hr::employee(&mut conn, &id)
            .await
            .map_err(|e| database(&e, locale))?
            .is_some();
        drop(conn);

        return Err(if known {
            Problem::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                &erp_i18n::Message::new(crate::messages::NO_SALARY)
                    .with("id", erp_i18n::MessageArg::text(id.clone())),
                locale,
                &CATALOG,
            )
        } else {
            not_found(&id, locale)
        });
    };

    // **The whole wage, which is what the Labour Law says.** Basic alone is the
    // common shortcut and it underpays; a caller who needs a different
    // definition is having a contract argument, not an arithmetic one.
    let wage = details.gross;

    // Service runs to the day they left, or to today if they have not — which
    // is what a business asking "what would we owe her" means.
    let from = details.hired_on.date_naive();
    let until = details
        .left_at
        .unwrap_or_else(chrono::Utc::now)
        .date_naive();
    let days = (until - from).num_days();

    let award = crate::end_of_service(wage, days, reason).map_err(|_| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::AMOUNT_OUT_OF_RANGE),
            locale,
            &CATALOG,
        )
    })?;

    Ok(Json(AwardView {
        entitlement: cash(award.entitlement),
        payable: cash(award.payable),
        days: award.days,
        wage: cash(wage),
    }))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn cash(amount: Money) -> SaCash {
    SaCash {
        minor: amount.minor(),
        currency: amount.currency().to_string(),
    }
}

fn view(schedule: Schedule, configured: bool) -> ScheduleView {
    ScheduleView {
        saudi_employee_bp: schedule.saudi_employee_bp,
        saudi_employer_bp: schedule.saudi_employer_bp,
        non_saudi_employee_bp: schedule.non_saudi_employee_bp,
        non_saudi_employer_bp: schedule.non_saudi_employer_bp,
        ceiling_minor: schedule.ceiling_minor,
        configured,
    }
}

fn footing(raw: &str, locale: Locale) -> Result<Footing, Problem> {
    match raw {
        "saudi" => Ok(Footing::Saudi),
        "non_saudi" => Ok(Footing::NonSaudi),
        other => Err(bad_request(
            crate::messages::UNKNOWN_FOOTING,
            "footing",
            other,
            locale,
        )),
    }
}

fn leaving(raw: &str, locale: Locale) -> Result<Leaving, Problem> {
    match raw {
        "dismissed" => Ok(Leaving::Dismissed),
        "resigned" => Ok(Leaving::Resigned),
        "in_full" => Ok(Leaving::InFull),
        "for_cause" => Ok(Leaving::ForCause),
        other => Err(bad_request(
            crate::messages::UNKNOWN_LEAVING,
            "reason",
            other,
            locale,
        )),
    }
}

fn not_found(id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(crate::messages::NO_SUCH_EMPLOYEE)
            .with("id", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "hr_sa read failed");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}
