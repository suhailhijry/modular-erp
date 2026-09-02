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
use erp_web::{After, Allowed, IdempotencyKey, Language, ManageTenant, Paged, PostEntries, Read};
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
        .routes(routes!(list_skills, record_skills))
        .routes(routes!(record_salary))
        .routes(routes!(list_rota, record_shifts))
        .routes(routes!(timesheet, record_day))
        .routes(routes!(list_leave, record_leave))
}

static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    // A shift that would not parse says why in the caller's language, and the
    // rule's refusals live with the type in `erp-recurrence`.
    &erp_recurrence::CATALOG,
    &branches::CATALOG,
    &erp_web::CATALOG,
]);

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
#[schema(example = json!({"claim": "hr:approve_leave", "branch": "BR-OLAYA"}))]
struct NewClaim {
    /// The granting module's own vocabulary — `hr:approve_leave`.
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

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"skills": ["SERVICE-CUT", "SERVICE-COLOUR"]}))]
struct NewSkills {
    /// **The whole set**, replacing what was there.
    ///
    /// One at a time is deliberately not offered: an empty list means *no
    /// restriction*, so recording the first skill starts restricting — and a
    /// caller adding one would eventually give somebody a single skill they
    /// were only trying to note and take everything else away.
    ///
    /// Each is the id of the bookable resource that service is.
    skills: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Skills {
    /// What they may perform. **Empty means anything**, not nothing.
    items: Vec<String>,
    /// Whether the list restricts them, which is the field that stops `[]`
    /// being read the wrong way round.
    restricted: bool,
}

/// One part of what somebody is paid, or has taken off.
#[derive(Debug, Deserialize, ToSchema)]
struct NewComponent {
    /// The business's own word for it: `بدل سكن`, `Transport`, `Advance`.
    what: String,
    amount: erp_web::Amount,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "basic": { "minor": 800_000, "currency": "SAR" },
    "allowances": [{ "what": "بدل سكن", "amount": { "minor": 200_000, "currency": "SAR" } }]
}))]
struct NewSalary {
    /// Basic pay for a full month.
    basic: erp_web::Amount,
    /// Added: housing, transport, a phone.
    ///
    /// **Amounts, not percentages.** A housing allowance quoted as 25% of basic
    /// is sent as the riyals it comes to, because a rate stored here would be
    /// recomputed on every run and a basic-pay rise would silently restate last
    /// month's payslip.
    #[serde(default)]
    allowances: Vec<NewComponent>,
    /// Taken off: an advance being repaid, a loan.
    ///
    /// **Not tax and not GOSI.** Those are statutory and computed from the
    /// gross by the country module; a business that could type them in here
    /// would be able to get them wrong.
    #[serde(default)]
    deductions: Vec<NewComponent>,
    /// What fraction of the work they perform they earn, in basis points.
    /// `500` is five per cent. Absent or zero is a business that pays none.
    ///
    /// **A rate and not an amount**, unlike the allowances above: a commission
    /// is a share of a number that changes every month, and storing the amount
    /// would mean restating it each period.
    #[serde(default)]
    commission_bp: u32,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

/// One window in a repeating rota.
///
/// **The same shape a bookable resource's opening hours take**, because it is
/// the same problem: which days, and between which two times on those days. The
/// type is `erp-recurrence`, below both modules.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct ShiftWindow {
    /// 1 is January. Empty means every month.
    #[serde(default)]
    months: Vec<u8>,
    /// 1 is Monday, 7 is Sunday. Empty means every day of the week.
    #[serde(default)]
    weekdays: Vec<u8>,
    /// 1 to 31. Empty means every day of the month.
    #[serde(default)]
    days: Vec<u8>,
    /// Minutes past local midnight. `540` is 09:00.
    opens_at: u16,
    /// Minutes past local midnight, exclusive. `1020` is 17:00.
    closes_at: u16,
    /// The first day this applies. Absent means it always has.
    #[schema(value_type = Option<String>, format = Date)]
    from: Option<chrono::NaiveDate>,
    /// The last day, **inclusive**. Absent means it always will.
    #[schema(value_type = Option<String>, format = Date)]
    until: Option<chrono::NaiveDate>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "shifts": [{ "weekdays": [1, 2, 3, 4, 5], "opens_at": 540, "closes_at": 1020 }]
}))]
struct NewShifts {
    /// **The whole pattern**, replacing what was there.
    ///
    /// One at a time is not offered, for the reason skills are not: an empty
    /// list means *no pattern recorded*, so adding one window would restrict
    /// somebody a caller was only trying to note a Saturday for.
    shifts: Vec<ShiftWindow>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Rota {
    items: Vec<ShiftWindow>,
    /// Whether a pattern has been recorded. **An empty list means "no pattern",
    /// not "never works"** — and this is the field that stops it being read the
    /// wrong way round.
    rostered: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"minutes": 480, "note": "غطّت وردية المساء"}))]
struct NewDay {
    /// Minutes worked. **Zero is an absence somebody recorded deliberately**,
    /// which is a different fact from a day with no record at all.
    minutes: u16,
    #[serde(default)]
    note: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct WorkedDayRecord {
    #[schema(value_type = String, format = Date)]
    on: chrono::NaiveDate,
    minutes: i32,
    note: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "kind": "annual", "from": "2026-06-03", "until": "2026-06-05"
}))]
struct NewLeave {
    /// `annual`, `sick`, `unpaid` or `statutory`.
    kind: String,
    #[schema(value_type = String, format = Date)]
    from: chrono::NaiveDate,
    /// **Inclusive**: the 3rd to the 5th is three days.
    #[schema(value_type = String, format = Date)]
    until: chrono::NaiveDate,
    #[serde(default)]
    why: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct LeaveRecord {
    kind: String,
    #[schema(value_type = String, format = Date)]
    from: chrono::NaiveDate,
    #[schema(value_type = String, format = Date)]
    until: chrono::NaiveDate,
    days: i32,
    why: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct LeaveSummary {
    items: Vec<LeaveRecord>,
    /// Days taken per kind over the window. **What a balance is drawn down
    /// by** — how much somebody is *entitled* to is statute, and belongs to the
    /// country module.
    taken: std::collections::BTreeMap<String, i64>,
}

/// A window of dates. Both ends inclusive, like every date range here.
#[derive(Debug, Deserialize)]
struct DateWindow {
    from: chrono::NaiveDate,
    until: chrono::NaiveDate,
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

/// What somebody is qualified to do.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}/skills",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, description = "`restricted: false` means the empty list is *anything*, not nothing.", body = Skills),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_skills(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<Skills>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;
    let employee = parse_id(&id, locale)?;

    // **The projection, not the aggregate** (L7). Loading a stream to answer a
    // query makes the cost of the answer grow with its length, which is what a
    // read model exists to stop — and the guard in `erp-eventlog` caught the
    // first version of this doing exactly that.
    //
    // The rule this renders is the same one `booking` acts on, and the two are
    // asserted to agree rather than assumed to: see
    // `the_who_can_do_this_list_matches_what_assign_would_allow`. A skill
    // recorded a second ago may not be here yet, which is what
    // `?consistent_after=` is for.
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let items = crate::skills(&mut conn, employee.as_str())
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Skills {
        restricted: !items.is_empty(),
        items,
    }))
}

/// Record what somebody is qualified to do.
///
/// **The whole set at once.** Sending the same set again writes nothing.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/skills",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = NewSkills,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "An unusable service id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_skills(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewSkills>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let skills = body
        .skills
        .iter()
        .map(|s| parse_id(s, locale))
        .collect::<Result<Vec<_>, _>>()?;

    let committed = crate::record_skills(
        &tenant.db,
        &employee,
        &skills,
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

/// Record what somebody is paid.
///
/// **Its own operation and its own event.** A rise is not an amendment of a
/// phone number, and it is the change somebody will ask to see dated. Sending
/// the same salary again writes nothing.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/salary",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = NewSalary,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "Basic pay that is not positive, parts in more than one currency, or deductions that come to more than the pay", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_salary(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewSalary>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;

    let salary = crate::Salary {
        basic: body.basic.parse(locale)?,
        allowances: components(&body.allowances, locale)?,
        deductions: components(&body.deductions, locale)?,
        commission_bp: body.commission_bp,
    };

    let committed = crate::record_salary(
        &tenant.db,
        &employee,
        &salary,
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

fn components(sent: &[NewComponent], locale: Locale) -> Result<Vec<crate::Component>, Problem> {
    sent.iter()
        .map(|part| {
            Ok(crate::Component {
                what: part.what.clone(),
                amount: part.amount.parse(locale)?,
            })
        })
        .collect()
}

/// When somebody works.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}/shifts",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, description = "`rostered: false` means no pattern is recorded, not that they never work.", body = Rota),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_rota(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<Rota>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let rules = crate::shifts(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    let items: Vec<ShiftWindow> = rules.iter().map(window).collect();
    Ok(Json(Rota {
        rostered: !items.is_empty(),
        items,
    }))
}

/// Record when somebody works.
///
/// **The whole pattern at once.** A rota is read as "when is Sara in", never as
/// a sequence of amendments.
///
/// **This restricts nothing.** A shift is what somebody is scheduled for, and
/// people cover, swap and stay late — a system telling a manager she cannot ask
/// somebody to stay is not a rule it gets to make. What refuses is a lapsed work
/// document, where the law does.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/shifts",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = NewShifts,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "A window that closes before it opens, runs past midnight, or names a day that is not one", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_shifts(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewShifts>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;

    let shifts: Vec<erp_recurrence::Availability> = body
        .shifts
        .iter()
        .map(|w| {
            erp_recurrence::Availability::from_parts(
                &w.months,
                &w.weekdays,
                &w.days,
                w.opens_at,
                w.closes_at,
                w.from,
                w.until,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Localize::message(&e),
                locale,
                &CATALOG,
            )
        })?;

    let committed = crate::record_shifts(
        &tenant.db,
        &employee,
        &shifts,
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

fn window(rule: &erp_recurrence::Availability) -> ShiftWindow {
    ShiftWindow {
        months: rule.months(),
        weekdays: rule.weekdays(),
        days: rule.days(),
        opens_at: rule.opens_at(),
        closes_at: rule.closes_at(),
        from: rule.starting(),
        until: rule.ending(),
    }
}

/// One person's timesheet over a window.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}/days",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("from" = String, Query, description = "First day, inclusive."),
        ("until" = String, Query, description = "Last day, inclusive."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<WorkedDayRecord>),
        (status = BAD_REQUEST, description = "A date that did not parse", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn timesheet(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
    Query(window): Query<DateWindow>,
) -> Result<Json<Vec<WorkedDayRecord>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let days = crate::worked(&mut conn, &id, window.from, window.until)
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(Json(
        days.into_iter()
            .map(|d| WorkedDayRecord {
                on: d.on,
                minutes: d.minutes,
                note: d.note,
            })
            .collect(),
    ))
}

/// Record a day somebody worked.
///
/// **The whole day at once**, not a clock-in and a clock-out. A half-recorded
/// day is a state every attendance system has and none handles well: it is
/// somebody who forgot, somebody who left early, or a device that lost power,
/// and nothing can tell which.
///
/// The same day with the same minutes writes nothing. The same day with
/// different minutes is a **correction**, and the timesheet takes the latest.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/days/{day}",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("day" = String, Path, description = "The day, `YYYY-MM-DD`. A shift running to 02:00 belongs to the day it started."),
    ),
    request_body = NewDay,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "Not a date, or more minutes than a day has", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_day(
    // **`PostEntries`, not `ManageTenant`.** A supervisor approving a timesheet
    // is recording what happened, not restructuring the business — the same
    // judgement that puts taking a booking and ringing a sale with the clerk.
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((id, day)): Path<(String, String)>,
    Json(body): Json<NewDay>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let on: chrono::NaiveDate = day
        .parse()
        .map_err(|_| bad_request(erp_web::messages::INVALID_ID, "day", &day, locale))?;

    let committed = crate::record_day(
        &tenant.db,
        &employee,
        on,
        body.minutes,
        &body.note,
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

/// Leave touching a window, and how many days of each kind it comes to.
///
/// **Touching, not starting in.** A fortnight beginning in March is leave in
/// April too, and a rota that only found the ones starting inside the window
/// would show somebody who is on a beach.
#[utoipa::path(
    get,
    path = "/v1/hr/employees/{employee}/leave",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
        ("from" = String, Query, description = "First day of the window, inclusive."),
        ("until" = String, Query, description = "Last day, inclusive."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position."),
    ),
    responses(
        (status = OK, description = "`taken` is days per kind — what a balance is drawn down by.", body = LeaveSummary),
        (status = BAD_REQUEST, description = "A date that did not parse", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_leave(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
    Query(window): Query<DateWindow>,
) -> Result<Json<LeaveSummary>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let items = crate::leave(&mut conn, &id, window.from, window.until)
        .await
        .map_err(|e| database(&e, locale))?;
    let taken = crate::leave_taken(&mut conn, &id, window.from, window.until)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(LeaveSummary {
        items: items
            .into_iter()
            .map(|l| LeaveRecord {
                kind: l.kind,
                from: l.from,
                until: l.until,
                days: l.days,
                why: l.why,
            })
            .collect(),
        taken: taken.into_iter().collect(),
    }))
}

/// Record leave taken, or booked.
#[utoipa::path(
    post,
    path = "/v1/hr/employees/{employee}/leave",
    tag = "hr",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("employee" = String, Path, description = "Their id."),
    ),
    request_body = NewLeave,
    responses(
        (status = OK, body = HrAccepted),
        (status = BAD_REQUEST, description = "Not a kind of leave, or an end before the start", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such employee", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn record_leave(
    // Recording that somebody was away is the same kind of act as recording
    // that they worked. See `record_day`.
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<NewLeave>,
) -> Result<Json<HrAccepted>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let employee = parse_id(&id, locale)?;
    let kind: crate::Leave = body.kind.parse().map_err(|e: crate::UnknownLeave| {
        bad_request(crate::messages::UNKNOWN_LEAVE, "kind", &e.0, locale)
    })?;

    let committed = crate::record_leave(
        &tenant.db,
        &employee,
        kind,
        body.from,
        body.until,
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
/// what `sales:approve_credit_note` is for, and a list of permitted names here
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
