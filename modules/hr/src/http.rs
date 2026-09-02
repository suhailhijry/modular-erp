//! The hr module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Granting answers with everyone who gained it
//!
//! `POST /v1/hr/employees/{employee}/claims` returns the whole list, not an
//! acknowledgement. A grant at a leaf escalates every ancestor, and a screen
//! that showed only the person being granted would hide exactly what somebody
//! needs to see before they click. The API makes that impossible to omit by
//! putting it in the response.

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
use erp_web::{Json, Query, bad_request, creating, metadata, parse_id, require_module};

use crate::claims::Claim;
use crate::{Details, Hire, HrError};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_employees, hire_employee))
        .routes(routes!(get_employee, amend_employee))
        .routes(routes!(reparent_employee))
        .routes(routes!(transfer_employee))
        .routes(routes!(record_leaving))
        .routes(routes!(list_claims, grant_claim))
        .routes(routes!(revoke_claim))
        .routes(routes!(record_document))
        .routes(routes!(expiring_documents))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &branches::CATALOG, &erp_web::CATALOG]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "name": "سارة الأحمد",
    "name_latin": "Sara Alahmad",
    "phone": "+966500000000",
    "reports_to": "EMP-0001",
    "branch": "BR-OLAYA"
}))]
struct NewEmployee {
    name: String,
    #[serde(default)]
    name_latin: Option<String>,
    #[serde(default)]
    national_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    /// Who they report to. Absent makes them the root, of which a business has
    /// one — whoever nobody reports to.
    #[serde(default)]
    reports_to: Option<String>,
    /// **Where they work**, which is not the `X-Branch` on this request. That
    /// one says where the request happened; this says where the person is.
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct AmendEmployee {
    name: String,
    #[serde(default)]
    name_latin: Option<String>,
    #[serde(default)]
    national_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"reports_to": "EMP-0002", "why": "نقل إلى فريق العمليات"}))]
struct Reparent {
    /// Absent makes them the root. Refused if it would put somebody under
    /// their own team.
    #[serde(default)]
    reports_to: Option<String>,
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Transfer {
    /// Absent makes the role company-wide.
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Leaving {
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"claim": "hr.approve_leave", "branch": "BR-OLAYA"}))]
struct NewClaim {
    /// The granting module's own vocabulary — `hr.approve_leave`.
    claim: String,
    /// Where it applies. Absent is company-wide, which is not the same as some
    /// particular branch.
    #[serde(default)]
    branch: Option<String>,
    /// Whether it travels up the reporting line. Defaults to yes, which is the
    /// rule; a claim on the segregation-of-duties list never travels whatever
    /// is sent.
    #[serde(default = "yes")]
    propagates: bool,
}

const fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, ToSchema)]
struct EmployeeRecord {
    id: String,
    name: String,
    name_latin: Option<String>,
    email: Option<String>,
    phone: Option<String>,
    /// Who they report to. Absent for the root.
    reports_to: Option<String>,
    /// Where they work.
    branch: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    hired_on: Timestamp,
    /// Set when they left. **The record stays** — they are on last year's
    /// payroll and whatever they approved.
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    left_at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct HeldClaim {
    claim: String,
    branch: Option<String>,
    /// Themselves, or somebody beneath them. **The first question anybody asks
    /// of an inherited permission is where it came from.**
    source: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ClaimChanged {
    /// **Everyone who now holds this claim**, not just the person named.
    ///
    /// A grant at a leaf escalates every ancestor. A screen that showed only
    /// the person being granted would hide the thing somebody has to see.
    holders: Vec<String>,
    /// Whether it travels up the reporting line. `false` when the claim is on
    /// the segregation-of-duties list, whatever was asked for.
    propagates: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"number": "2312345678", "expires_on": "2027-05-31"}))]
struct NewDocument {
    /// Its number, as printed on it.
    number: String,
    /// **The last day it is valid**, inclusive — a document that says it expires
    /// on the 30th is valid on the 30th, which is what it means and what the
    /// person holding it will argue.
    ///
    /// A date and not a timestamp: an iqama expires on a day in Riyadh, not at
    /// an hour in UTC.
    #[schema(value_type = String, format = Date, example = "2027-05-31")]
    expires_on: chrono::NaiveDate,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ExpiringDocument {
    employee: String,
    name: String,
    branch: Option<String>,
    /// `identity`, `work_permit`, `medical` or `licence`.
    kind: String,
    number: String,
    #[schema(value_type = String, format = Date)]
    expires_on: chrono::NaiveDate,
    /// **Negative once it has gone.** A screen that showed "0 days left" for
    /// both tomorrow and last March is the screen somebody stops reading.
    days_left: i32,
}

#[derive(Debug, Deserialize)]
struct ExpiryWindow {
    /// How far ahead to look. Sixty days by default, which is roughly the
    /// notice an iqama renewal needs.
    within_days: Option<i32>,
}

#[derive(Debug, Serialize, ToSchema)]
struct HrAccepted {
    id: String,
    position: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StaffQuery {
    /// One branch's people. Defaults to the `X-Branch` this request carries;
    /// send it explicitly to look wider. **A filter, not a wall** — an org
    /// chart is company-wide by nature.
    branch: Option<String>,
    /// Send `all` to ignore the branch entirely, which is what a payroll run
    /// and an org chart both want.
    #[serde(default)]
    scope: Option<String>,
    #[serde(flatten)]
    page: After,
    #[serde(default)]
    leavers: bool,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Everyone, alphabetically.
///
/// **Scoped by default, crossable on request.** The list narrows to the
/// caller's `X-Branch`; `?branch=` looks at another and `?scope=all` looks at
/// the company.
#[utoipa::path(
    get,
    path = "/v1/hr/employees",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("branch" = Option<String>, Query, description = "One branch's people. Defaults to `X-Branch`."),
        ("scope" = Option<String>, Query, description = "`all` ignores the branch entirely — an org chart is company-wide."),
        ("leavers" = Option<bool>, Query, description = "Include people who have left."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<EmployeeRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable hr", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_employees(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<StaffQuery>,
) -> Result<Json<Paged<EmployeeRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let branch = if query.scope.as_deref() == Some("all") {
        None
    } else {
        query
            .branch
            .clone()
            .or_else(|| tenant.branch.as_ref().map(|b| b.as_str().to_owned()))
    };

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::employees(
        &mut conn,
        branch.as_deref(),
        query.leavers,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, record)))
}

/// Put somebody on the books.
#[utoipa::path(
    post,
    path = "/v1/hr/employees",
    tag = "hr",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = NewEmployee,
    responses(
        (status = CREATED, body = HrAccepted),
        (status = BAD_REQUEST, description = "No name, or no way to reach them", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable hr", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such manager, or no such open branch", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn hire_employee(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    key: IdempotencyKey,
    Json(body): Json<NewEmployee>,
) -> Result<(StatusCode, Json<HrAccepted>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let id = key.id().clone();

    let hiring = Hire {
        details: Details {
            name: body.name,
            name_latin: body.name_latin,
            national_id: body.national_id,
            email: body.email,
            phone: body.phone,
        },
        reports_to: body
            .reports_to
            .as_deref()
            .map(|m| parse_id(m, locale))
            .transpose()?,
        branch: body
            .branch
            .as_deref()
            .map(|b| parse_id(b, locale))
            .transpose()?,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::hire(&tenant.db, &id, &hiring, &creating(&tenant, &key))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(HrAccepted {
            id: id.to_string(),
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One of them.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = EmployeeRecord),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_employee(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<EmployeeRecord>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    crate::employee(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?
        .map(|e| Json(record(e)))
        .ok_or_else(|| not_found(&id, locale))
}

/// Change what is known about somebody. **Never their reporting line.**
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = AmendEmployee,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "No name, or no way to reach them", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn amend_employee(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AmendEmployee>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;

    let details = Details {
        name: body.name,
        name_latin: body.name_latin,
        national_id: body.national_id,
        email: body.email,
        phone: body.phone,
    };

    let committed = crate::amend_employee(
        &tenant.db,
        &employee,
        &details,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(HrAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Move somebody in the chart.
///
/// Its own operation because it moves everything they carry: every claim in
/// their subtree stops reaching their old manager and starts reaching the new
/// one. Refused if it would put somebody under their own team.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/manager",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = Reparent,
    responses(
        (status = OK, body = HrAccepted),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such manager, or a reporting line that would loop", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn reparent_employee(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Reparent>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let manager = body
        .reports_to
        .as_deref()
        .map(|m| parse_id(m, locale))
        .transpose()?;

    let committed = crate::reparent(
        &tenant.db,
        &employee,
        manager.as_ref(),
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(HrAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Move somebody to another branch.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/branch",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = Transfer,
    responses(
        (status = OK, body = HrAccepted),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such open branch", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn transfer_employee(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Transfer>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let branch = body
        .branch
        .as_deref()
        .map(|b| parse_id(b, locale))
        .transpose()?;

    let committed = crate::transfer(
        &tenant.db,
        &employee,
        branch.as_ref(),
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(HrAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Record that somebody has left.
///
/// **Their claims stop and their record does not.** Their team keeps reporting
/// to them until the business moves it, which is a decision a resignation does
/// not get to make.
#[utoipa::path(
    post,
    path = "/v1/hr/employees/{employee}/leaving",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = Leaving,
    responses(
        (status = OK, description = "Recorded. Already-left is the same answer.", body = HrAccepted),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_leaving(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Leaving>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;

    let committed = crate::record_leaving(
        &tenant.db,
        &employee,
        &body.why,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(HrAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Everything somebody holds, and where each came from.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}/claims",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    responses(
        (status = OK, body = Vec<HeldClaim>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_claims(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<Vec<HeldClaim>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;

    // **The write side, not `proj_hr`.** This is the same table a command
    // checks, so what a screen shows and what an approval does cannot disagree.
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let held = crate::effective(&mut conn, &employee)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(
        held.into_iter()
            .map(|h| HeldClaim {
                claim: h.claim.name,
                branch: h.claim.branch,
                source: h.source,
            })
            .collect(),
    ))
}

/// Grant a claim, and see **everyone who gained it**.
#[utoipa::path(
    post,
    path = "/v1/hr/employees/{employee}/claims",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = NewClaim,
    responses(
        (status = OK, description = "Granted. `holders` is everyone who now has it, which is more than the person named.", body = ClaimChanged),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "No such employee, or one who has left", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn grant_claim(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewClaim>,
) -> Result<Json<ClaimChanged>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let claim = read_claim(&body.claim, body.branch.clone(), locale)?;

    // Whatever was asked, a segregated claim does not travel — and the answer
    // says so rather than silently doing something other than what was sent.
    let propagates = body.propagates && !crate::is_segregated(&claim.name);

    let holders = crate::grant_claim(&tenant.db, &employee, &claim, body.propagates)
        .await
        .map_err(|e| problem_for(&e, locale))?;

    Ok(Json(ClaimChanged {
        holders,
        propagates,
    }))
}

/// Take a claim back, and see everyone who lost it.
#[utoipa::path(
    delete,
    path = "/v1/hr/employees/{employee}/claims/{claim}",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("claim" = String, Path, description = "The claim name."),
        ("branch" = Option<String>, Query, description = "Which branch's grant. Absent means the company-wide one."),
    ),
    responses(
        (status = OK, description = "Gone. `holders` is everyone who lost it.", body = ClaimChanged),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn revoke_claim(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path((id, claim)): Path<(String, String)>,
    Query(scope): Query<BranchScope>,
) -> Result<Json<ClaimChanged>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let claim = read_claim(&claim, scope.branch, locale)?;

    let holders = crate::revoke_claim(&tenant.db, &employee, &claim)
        .await
        .map_err(|e| problem_for(&e, locale))?;

    Ok(Json(ClaimChanged {
        holders,
        propagates: false,
    }))
}

/// Record a document, or renew one.
///
/// **One operation for both**, because a renewal is the same fact with a later
/// date. Sending the same number and date twice writes nothing.
///
/// Once recorded, a lapsed document **stops this person being rostered** —
/// `booking` refuses to assign them. That is not a warning somebody may
/// override: an expired iqama means a person who may not legally work.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/documents/{kind}",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("kind" = String, Path, description = "`identity`, `work_permit`, `medical` or `licence`."),
    ),
    request_body = NewDocument,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "Not a kind this system tracks, or a document with no number", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_document(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((id, kind)): Path<(String, String)>,
    Json(body): Json<NewDocument>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let kind: crate::DocumentKind = kind.parse().map_err(|e: crate::UnknownDocument| {
        bad_request(crate::messages::UNKNOWN_DOCUMENT, "kind", &e.0, locale)
    })?;

    let committed = crate::record_document(
        &tenant.db,
        &employee,
        kind,
        &body.number,
        body.expires_on,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(HrAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Documents that have lapsed, or are about to.
///
/// **Soonest first, and the ones that have gone come first of all** — they are
/// not warnings that were ignored, they are people who may not be rostered, and
/// sorting them below the upcoming ones is how they stay buried.
#[utoipa::path(
    get,
    path = "/v1/hr/documents/expiring",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("within_days" = Option<i32>, Query, description = "How far ahead to look. 60 by default."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<ExpiringDocument>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable hr", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn expiring_documents(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(window): Query<ExpiryWindow>,
) -> Result<Json<Vec<ExpiringDocument>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    // Clamped rather than refused, the way every limit here is. A caller asking
    // for ten years is asking for the whole table and can have it.
    let within = window.within_days.unwrap_or(60).clamp(0, 3650);

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let rows = crate::expiring(&mut conn, within, 500)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(
        rows.into_iter()
            .map(|r| ExpiringDocument {
                employee: r.employee,
                name: r.name,
                branch: r.branch,
                kind: r.kind,
                number: r.number,
                expires_on: r.expires_on,
                days_left: r.days_left,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct BranchScope {
    branch: Option<String>,
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn record(e: crate::EmployeeSummary) -> EmployeeRecord {
    EmployeeRecord {
        id: e.id,
        name: e.name,
        name_latin: e.name_latin,
        email: e.email,
        phone: e.phone,
        reports_to: e.reports_to,
        branch: e.branch,
        hired_on: e.hired_on,
        left_at: e.left_at,
    }
}

/// A claim name is checked for shape and never for meaning.
///
/// The vocabulary belongs to whichever module grants it — `hr` does not know
/// what `sales.approve_credit_note` is for, and a list of permitted names here
/// would be a second place every module had to register.
fn read_claim(name: &str, branch: Option<String>, locale: Locale) -> Result<Claim, Problem> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(bad_request(
            crate::messages::NOT_A_CLAIM,
            "claim",
            name,
            locale,
        ));
    }
    Ok(Claim {
        name: name.to_owned(),
        branch: branch
            .map(|b| b.trim().to_owned())
            .filter(|b| !b.is_empty()),
    })
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

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "hr read failed");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<HrError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                HrError::NoSuchEmployee(_) => StatusCode::NOT_FOUND,
                HrError::Details(_) => StatusCode::BAD_REQUEST,
                // Well-formed, and refused on the state of the world: no such
                // manager, a branch that is not open, a line that would loop.
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
            tracing::error!(error = %other, "hr command failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                erp_i18n::Message::new(crate::messages::DATABASE),
            )
        }
    };
    Problem::new(status, &message, locale, &CATALOG)
}
