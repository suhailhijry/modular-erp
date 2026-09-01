//! The booking module's HTTP surface.
//!
//! Translation only, like every module's. See [`ledger::http`] for why these
//! live in the module rather than in the composition root.
//!
//! # Why the timetable is lists on the wire and bit fields underneath
//!
//! `{"weekdays": [1,2,3,4,5], "opens_at": 540, "closes_at": 1020}` is a rule a
//! person can read and a client can build. A `u8` with five bits set is the
//! same rule, indexes, and is nobody's idea of an API. The conversion is in
//! [`crate::Availability::from_parts`], which is also where the ranges are
//! checked, so there is one place that knows Monday is 1.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_occupancy::Span;
use erp_tenant::CommandError;
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{After, Allowed, Language, ManageTenant, Paged, PostEntries, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, metadata, parse_id, require_module};

use crate::{Availability, BookingError, Details, Draft, Held, Kind, Line, Stage};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_bookables, declare_bookable))
        .routes(routes!(get_bookable, amend_bookable))
        .routes(routes!(set_opening_hours))
        .routes(routes!(withdraw_bookable, restore_bookable))
        .routes(routes!(list_reservations, take_reservation))
        .routes(routes!(get_reservation))
        .routes(routes!(move_reservation))
        .routes(routes!(reschedule_reservation))
        .routes(routes!(assign_unit))
        // Unauthenticated on purpose, like `ledger::list_charts`: a signup form
        // needs to show a salon what a salon gets before anybody has an
        // account. It is product information, not data.
        .routes(routes!(list_trades))
        .routes(routes!(fit_out))
}

/// This module's own failures plus everything any route can produce.
///
/// `erp_occupancy`'s is in here because a booking's most common refusal comes
/// from the engine — "that stylist is already holding one of one" — and it has
/// to reach a person in their own language with the numbers intact.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &erp_occupancy::CATALOG,
    &crm::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// One window in a timetable.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[schema(example = json!({
    "weekdays": [6, 7, 1, 2, 3],
    "opens_at": 540,
    "closes_at": 1260
}))]
struct OpeningHours {
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
    /// Minutes past local midnight, exclusive. `1020` is 17:00, and a booking
    /// may run right up to it.
    closes_at: u16,
    /// The first day these hours apply. Absent means they always have.
    #[schema(value_type = Option<String>, format = Date)]
    from: Option<chrono::NaiveDate>,
    /// The last day, **inclusive**. Absent means they always will.
    #[schema(value_type = Option<String>, format = Date)]
    until: Option<chrono::NaiveDate>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "stylist-noura",
    "name": "نورة",
    "name_latin": "Noura",
    "kind": "person",
    "capacity": 1
}))]
struct NewBookable {
    /// Your key for this resource. Declaring the same one twice is a no-op.
    id: String,
    name: String,
    name_latin: Option<String>,
    /// `person`, `place` or `thing`. Display only — no rule branches on it.
    kind: String,
    /// How many can be held at once. One stylist, six covers at a table, eight
    /// rooms of a type, five hundred places in a museum slot.
    capacity: u16,
}

/// The same fields without the id, which cannot change.
#[derive(Debug, Deserialize, ToSchema)]
struct AmendBookable {
    name: String,
    name_latin: Option<String>,
    capacity: u16,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Timetable {
    /// Every window this resource is offered in. **An empty list means always**,
    /// which is what a hotel room and a museum slot want.
    hours: Vec<OpeningHours>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct WithdrawBookable {
    /// Out for repair, on leave, sold. Shown next to it on the calendar.
    why: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BookableRecord {
    id: String,
    name: String,
    name_latin: Option<String>,
    kind: String,
    capacity: u16,
    withdrawn: bool,
    withdrawn_why: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BookableDetail {
    #[serde(flatten)]
    bookable: BookableRecord,
    hours: Vec<OpeningHours>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    declared_on: Timestamp,
}

/// One thing a trade would declare.
#[derive(Debug, Serialize, ToSchema)]
struct TradeResourceView {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    /// How many can be held at once. One stylist, six covers, five hundred
    /// tickets in a slot.
    capacity: u16,
}

/// A ready-made rota.
#[derive(Debug, Serialize, ToSchema)]
struct TradeView {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    /// Everything it would declare, so a signup form can show it before
    /// anybody commits to anything.
    resources: Vec<TradeResourceView>,
    /// Opening hours, as minutes past local midnight.
    hours: Vec<TradeHoursView>,
}

#[derive(Debug, Serialize, ToSchema)]
struct TradeHoursView {
    /// ISO weekdays, Monday as 1. Empty means every day.
    weekdays: Vec<u8>,
    opens_at: u16,
    closes_at: u16,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"trade": "salon"}))]
struct FitOut {
    /// The trade's id, from `GET /v1/booking/trades`.
    trade: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct FittedOutView {
    declared: usize,
    /// Already there, and left exactly as they were. Not a failure — fitting
    /// out twice is meant to be harmless.
    skipped: usize,
    scheduled: usize,
}

/// A resource a line takes, and how much of it.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
struct BookedResource {
    resource: String,
    /// Defaults to one, which is what a stylist and a chair are.
    #[serde(default = "one")]
    quantity: u16,
}

const fn one() -> u16 {
    1
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewReservationLine {
    /// What the business calls it. Never looked at by any rule here.
    what: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    /// Exclusive, so a line ending at 11:00 and one starting at 11:00 are
    /// back to back and do not clash.
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    /// Everything this line takes at once — the stylist *and* the chair.
    takes: Vec<BookedResource>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "BK-0001",
    "customer": "CUST-0001",
    "customer_name": "سارة",
    "lines": [{
        "what": "قص وتصفيف",
        "from": "2026-09-02T07:00:00Z",
        "until": "2026-09-02T08:00:00Z",
        "takes": [{"resource": "stylist-noura"}, {"resource": "chair-1"}]
    }]
}))]
struct NewReservation {
    /// Your key for this booking. Taking the same one twice is a no-op, and
    /// takes no capacity the second time.
    id: String,
    /// The `crm` record, when there is one. A walk-in has none.
    customer: Option<String>,
    /// What the diary prints. Frozen, so a customer changing their name next
    /// year does not rewrite last year's calendar.
    customer_name: String,
    customer_phone: Option<String>,
    lines: Vec<NewReservationLine>,
    #[serde(default)]
    note: String,
    /// When the booking was taken. Defaults to now.
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"stage": "arrived"}))]
struct MoveReservation {
    /// `reserved`, `confirmed`, `arrived`, `in_service`, `completed`,
    /// `cancelled` or `no_show`. Moving to the stage it is already in is a
    /// no-op.
    stage: String,
    /// Why. Kept and shown, and it is what a cancellation reason goes in.
    #[serde(default)]
    why: String,
}

#[derive(Debug, Deserialize, ToSchema)]
struct RescheduleReservation {
    /// The whole set, replacing what was there. Assignments are dropped: a
    /// line that moved has to be given a unit again at its new hour.
    lines: Vec<NewReservationLine>,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"unit": "room-302"}))]
struct AssignUnit {
    /// The resource picked out of the pool this line booked.
    unit: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReservationRecord {
    id: String,
    customer: Option<String>,
    customer_name: String,
    customer_phone: Option<String>,
    stage: String,
    stage_why: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    starts_at: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    ends_at: Timestamp,
    note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReservationRecordLine {
    line: u16,
    what: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    from: Timestamp,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    until: Timestamp,
    takes: Vec<BookedResource>,
    /// The unit picked out of the pool, once one has been.
    unit: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ReservationRecordDetail {
    #[serde(flatten)]
    reservation: ReservationRecord,
    lines: Vec<ReservationRecordLine>,
}

#[derive(Debug, Serialize, ToSchema)]
struct BookingAccepted {
    id: String,
    /// The log position this landed at. Pass it to a read as
    /// `?consistent_after=` to see it in the diary.
    position: Option<i64>,
}

/// Paging, plus the one flag the resource list needs.
#[derive(Debug, Deserialize)]
struct BookableQuery {
    #[serde(flatten)]
    page: After,
    /// Include resources that are out of service.
    #[serde(default)]
    withdrawn: bool,
}

/// Paging, plus the window and the stage a diary filters on.
#[derive(Debug, Deserialize)]
struct DiaryQuery {
    #[serde(flatten)]
    page: After,
    from: Option<Timestamp>,
    until: Option<Timestamp>,
    stage: Option<String>,
}

// ---------------------------------------------------------------------------
// Routes — what can be booked
// ---------------------------------------------------------------------------

/// Everything that can be booked, people first.
#[utoipa::path(
    get,
    path = "/v1/booking/resources",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("withdrawn" = Option<bool>, Query, description = "Include what is out of service."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = Paged<BookableRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable booking", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn list_bookables(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<BookableQuery>,
) -> Result<Json<Paged<BookableRecord>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::resources(
        &mut conn,
        query.withdrawn,
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, bookable)))
}

/// Record something that can be booked.
#[utoipa::path(
    post,
    path = "/v1/booking/resources",
    tag = "booking",
    request_body = NewBookable,
    responses(
        (status = CREATED, body = BookingAccepted),
        (status = BAD_REQUEST, description = "A missing name, or a kind that is not one", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable booking", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn declare_bookable(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewBookable>,
) -> Result<(StatusCode, Json<BookingAccepted>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = parse_id(&body.id, locale)?;
    let details = Details {
        name: body.name,
        name_latin: body.name_latin,
        kind: kind(&body.kind, locale)?,
        capacity: body.capacity,
    };

    let committed = crate::declare_resource(
        &tenant.db,
        &id,
        &details,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(BookingAccepted {
            id: body.id,
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One resource, with the timetable it is offered on.
#[utoipa::path(
    get,
    path = "/v1/booking/resources/{resource}",
    tag = "booking",
    params(("resource" = String, Path, description = "The id you declared it under.")),
    responses(
        (status = OK, body = BookableDetail),
        (status = NOT_FOUND, description = "No such resource, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_bookable(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<BookableDetail>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::resource(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    let detail = found.ok_or_else(|| no_such_resource(&id, locale))?;
    Ok(Json(BookableDetail {
        bookable: bookable(detail.summary),
        hours: detail.availability.iter().map(hours).collect(),
        declared_on: detail.declared_on,
    }))
}

/// Change a resource's name or how much of it there is.
#[utoipa::path(
    patch,
    path = "/v1/booking/resources/{resource}",
    tag = "booking",
    params(("resource" = String, Path, description = "The id you declared it under.")),
    request_body = AmendBookable,
    responses(
        (status = OK, body = BookingAccepted),
        (status = BAD_REQUEST, description = "A missing name", body = Problem),
        (status = NOT_FOUND, description = "No such resource", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn amend_bookable(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<AmendBookable>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;

    // No kind: it is set once, at declaration. A chair does not become a
    // person, so there is no event that changes it and no field here to send.
    let amendment = crate::Amendment {
        name: body.name,
        name_latin: body.name_latin,
        capacity: body.capacity,
    };
    let committed = crate::amend_resource(
        &tenant.db,
        &aggregate,
        &amendment,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Set the whole timetable a resource is offered on.
#[utoipa::path(
    put,
    path = "/v1/booking/resources/{resource}/availability",
    tag = "booking",
    params(("resource" = String, Path, description = "The id you declared it under.")),
    request_body = Timetable,
    responses(
        (status = OK, body = BookingAccepted),
        (status = BAD_REQUEST, description = "A window that closes before it opens, or a month that is not one", body = Problem),
        (status = NOT_FOUND, description = "No such resource", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn set_opening_hours(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<Timetable>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;

    let rules: Vec<Availability> = body
        .hours
        .iter()
        .map(|h| {
            Availability::from_parts(
                &h.months,
                &h.weekdays,
                &h.days,
                h.opens_at,
                h.closes_at,
                h.from,
                h.until,
            )
        })
        .collect::<Result<_, _>>()
        .map_err(|e| Problem::new(StatusCode::BAD_REQUEST, &e.message(), locale, &CATALOG))?;

    let committed = crate::schedule_resource(
        &tenant.db,
        &aggregate,
        &rules,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Take a resource out of service. Bookings already against it stand.
#[utoipa::path(
    post,
    path = "/v1/booking/resources/{resource}/withdrawal",
    tag = "booking",
    params(("resource" = String, Path, description = "The id you declared it under.")),
    request_body = WithdrawBookable,
    responses(
        (status = OK, body = BookingAccepted),
        (status = NOT_FOUND, description = "No such resource", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn withdraw_bookable(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<WithdrawBookable>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::withdraw_resource(
        &tenant.db,
        &aggregate,
        body.why.as_deref().unwrap_or_default(),
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Put it back, at the capacity it had.
#[utoipa::path(
    delete,
    path = "/v1/booking/resources/{resource}/withdrawal",
    tag = "booking",
    params(("resource" = String, Path, description = "The id you declared it under.")),
    responses(
        (status = OK, body = BookingAccepted),
        (status = NOT_FOUND, description = "No such resource", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn restore_bookable(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let committed = crate::restore_resource(
        &tenant.db,
        &aggregate,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

// ---------------------------------------------------------------------------
// Routes — the diary
// ---------------------------------------------------------------------------

/// The diary: bookings that overlap a window, earliest first.
#[utoipa::path(
    get,
    path = "/v1/booking/reservations",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("after" = Option<String>, Query, description = "From a previous page's `next`."),
        ("limit" = Option<i64>, Query, description = "Rows per page. Clamped, never refused."),
        ("from" = Option<String>, Query, description = "Only bookings that have not finished by then."),
        ("until" = Option<String>, Query, description = "Only bookings that start before then."),
        ("stage" = Option<String>, Query, description = "Only bookings in this stage."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read model to reach this log position. From a write's `position`."),
    ),
    responses(
        (status = OK, description = "One page. `next` is absent when the list ended.", body = Paged<ReservationRecord>),
        (status = BAD_REQUEST, description = "An unreadable cursor", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable booking", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure, or the projection did not catch up in time. Retryable.", body = Problem),
    ),
)]
async fn list_reservations(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<DiaryQuery>,
) -> Result<Json<Paged<ReservationRecord>>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = query.page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::reservations(
        &mut conn,
        query.from,
        query.until,
        query.stage.as_deref(),
        query.page.limit(50, 200),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;

    Ok(Json(Paged::of(page, view)))
}

/// Take a booking.
#[utoipa::path(
    post,
    path = "/v1/booking/reservations",
    tag = "booking",
    request_body = NewReservation,
    responses(
        (status = CREATED, body = BookingAccepted),
        (status = BAD_REQUEST, description = "No lines, no name, or a time that ends before it starts", body = Problem),
        (status = NOT_FOUND, description = "No such customer or resource, or the tenant did not enable booking", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Nothing free then, or the resource is not open then", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn take_reservation(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<NewReservation>,
) -> Result<(StatusCode, Json<BookingAccepted>), Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let id = parse_id(&body.id, locale)?;
    let customer = body
        .customer
        .as_deref()
        .map(|c| parse_id(c, locale))
        .transpose()?;

    let draft = Draft {
        customer: crate::Customer {
            id: customer,
            name: body.customer_name,
            phone: body.customer_phone,
        },
        lines: lines(&body.lines, locale)?,
        note: body.note,
        at: body.at.unwrap_or_else(chrono::Utc::now),
    };

    let committed = crate::reserve(&tenant.db, &id, &draft, &metadata(&tenant))
        .await
        .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok((
        StatusCode::CREATED,
        Json(BookingAccepted {
            id: body.id,
            position: committed.at.map(erp_types::LogPosition::get),
        }),
    ))
}

/// One booking and its lines.
#[utoipa::path(
    get,
    path = "/v1/booking/reservations/{reservation}",
    tag = "booking",
    params(("reservation" = String, Path, description = "The id you booked it under.")),
    responses(
        (status = OK, body = ReservationRecordDetail),
        (status = NOT_FOUND, description = "No such booking, or the projection has not caught up", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn get_reservation(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Path(id): Path<String>,
) -> Result<Json<ReservationRecordDetail>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let found = crate::reservation(&mut conn, &id)
        .await
        .map_err(|e| database(&e, locale))?;

    let detail = found.ok_or_else(|| {
        Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(crate::messages::NO_SUCH_RESERVATION)
                .with("reservation", erp_i18n::MessageArg::text(id.clone())),
            locale,
            &CATALOG,
        )
    })?;

    Ok(Json(ReservationRecordDetail {
        reservation: view(detail.summary),
        lines: detail
            .lines
            .into_iter()
            .map(|l| ReservationRecordLine {
                line: l.line,
                what: l.what,
                from: l.starts_at,
                until: l.ends_at,
                takes: l.takes.iter().map(taken).collect(),
                unit: l.unit,
            })
            .collect(),
    }))
}

/// Move a booking along its lifecycle.
#[utoipa::path(
    post,
    path = "/v1/booking/reservations/{reservation}/stage",
    tag = "booking",
    params(("reservation" = String, Path, description = "The id you booked it under.")),
    request_body = MoveReservation,
    responses(
        (status = OK, body = BookingAccepted),
        (status = BAD_REQUEST, description = "Not a stage", body = Problem),
        (status = NOT_FOUND, description = "No such booking", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "A move the lifecycle does not allow", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn move_reservation(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<MoveReservation>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let target: Stage = body.stage.parse().map_err(|e: crate::UnknownStage| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::UNKNOWN_STAGE)
                .with("value", erp_i18n::MessageArg::text(e.0)),
            locale,
            &CATALOG,
        )
    })?;

    let committed = crate::move_to(
        &tenant.db,
        &aggregate,
        target,
        &body.why,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Move a booking in time, or onto different resources.
#[utoipa::path(
    put,
    path = "/v1/booking/reservations/{reservation}/lines",
    tag = "booking",
    params(("reservation" = String, Path, description = "The id you booked it under.")),
    request_body = RescheduleReservation,
    responses(
        (status = OK, body = BookingAccepted),
        (status = BAD_REQUEST, description = "No lines, or a time that ends before it starts", body = Problem),
        (status = NOT_FOUND, description = "No such booking or resource", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Nothing free then, the resource is not open then, or the booking is already over", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn reschedule_reservation(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<RescheduleReservation>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let lines = lines(&body.lines, locale)?;

    let committed = crate::reschedule(
        &tenant.db,
        &aggregate,
        &lines,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Pick a unit out of the pool a line booked.
#[utoipa::path(
    put,
    path = "/v1/booking/reservations/{reservation}/lines/{line}/unit",
    tag = "booking",
    params(
        ("reservation" = String, Path, description = "The id you booked it under."),
        ("line" = u16, Path, description = "Which line, counting from zero."),
    ),
    request_body = AssignUnit,
    responses(
        (status = OK, body = BookingAccepted),
        (status = NOT_FOUND, description = "No such booking, line or unit", body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "That unit is taken then, or the booking is already over", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn assign_unit(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path((id, line)): Path<(String, String)>,
    Json(body): Json<AssignUnit>,
) -> Result<Json<BookingAccepted>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let aggregate = parse_id(&id, locale)?;
    let unit = parse_id(&body.unit, locale)?;

    // **Parsed here rather than by `Path<(String, u16)>`.** Axum rejects a
    // path segment it cannot parse with a plain-text 400, and every failure
    // this API produces is `application/problem+json` — a promise the contract
    // test checks and this route was quietly breaking.
    let line: u16 = line.parse().map_err(|_| {
        Problem::new(
            StatusCode::NOT_FOUND,
            &erp_i18n::Message::new(crate::messages::NO_SUCH_LINE)
                .with("line", erp_i18n::MessageArg::text(line.clone())),
            locale,
            &CATALOG,
        )
    })?;

    let committed = crate::assign(
        &tenant.db,
        &aggregate,
        line,
        &unit,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(BookingAccepted {
        id,
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Ready-made rotas, in the caller's language.
///
/// Unauthenticated: a signup form needs to show the choices before anyone has
/// an account. Everything a trade declares is renameable and withdrawable
/// afterwards, so this is a starting point rather than a commitment.
#[utoipa::path(
    get,
    path = "/v1/booking/trades",
    tag = "booking",
    security(),
    responses((status = OK, body = Vec<TradeView>)),
)]
async fn list_trades(Language(locale): Language) -> Json<Vec<TradeView>> {
    Json(
        crate::TRADES
            .iter()
            .map(|t| TradeView {
                id: t.id,
                name: t.name(locale),
                description: t.description(locale),
                resources: t
                    .resources
                    .iter()
                    .map(|r| TradeResourceView {
                        id: r.id,
                        name: r.name(locale),
                        kind: r.kind.as_str(),
                        capacity: r.capacity,
                    })
                    .collect(),
                hours: t
                    .hours
                    .iter()
                    .map(|h| TradeHoursView {
                        weekdays: h.weekdays.to_vec(),
                        opens_at: h.opens_at,
                        closes_at: h.closes_at,
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// Fit the tenant out for a trade: declare its rota and set its hours.
///
/// Safe to run twice. Anything already there is left as it is, including a name
/// somebody changed.
#[utoipa::path(
    post,
    path = "/v1/booking/fit-out",
    tag = "booking",
    request_body = FitOut,
    responses(
        (status = OK, body = FittedOutView),
        (status = BAD_REQUEST, description = "No trade by that name", body = Problem),
        (status = NOT_FOUND, description = "The tenant did not enable booking", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "Backpressure. Retryable.", body = Problem),
    ),
)]
async fn fit_out(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<FitOut>,
) -> Result<Json<FittedOutView>, Problem> {
    require_module(&tenant, &crate::module_id(), locale)?;
    let trade = crate::trade(&body.trade).ok_or_else(|| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::NO_SUCH_TRADE)
                .with("trade", erp_i18n::MessageArg::text(body.trade.clone())),
            locale,
            &CATALOG,
        )
    })?;

    let fitted = crate::fit_out(
        &tenant.db,
        trade,
        locale,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| problem_for(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(FittedOutView {
        declared: fitted.declared,
        skipped: fitted.skipped,
        scheduled: fitted.scheduled,
    }))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

fn lines(sent: &[NewReservationLine], locale: Locale) -> Result<Vec<Line>, Problem> {
    sent.iter()
        .map(|line| {
            let span = Span::new(line.from, line.until).map_err(|e| {
                Problem::new(StatusCode::BAD_REQUEST, &e.message(), locale, &CATALOG)
            })?;
            let takes = line
                .takes
                .iter()
                .map(|t| {
                    Ok(Held {
                        resource: parse_id(&t.resource, locale)?,
                        quantity: t.quantity,
                    })
                })
                .collect::<Result<Vec<_>, Problem>>()?;
            Ok(Line {
                what: line.what.clone(),
                span,
                takes,
            })
        })
        .collect()
}

fn kind(sent: &str, locale: Locale) -> Result<Kind, Problem> {
    sent.parse().map_err(|e: crate::UnknownKind| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::UNKNOWN_KIND)
                .with("value", erp_i18n::MessageArg::text(e.0)),
            locale,
            &CATALOG,
        )
    })
}

fn hours(rule: &Availability) -> OpeningHours {
    OpeningHours {
        months: rule.months(),
        weekdays: rule.weekdays(),
        days: rule.days(),
        opens_at: rule.opens_at(),
        closes_at: rule.closes_at(),
        from: rule.starting(),
        until: rule.ending(),
    }
}

fn taken(held: &Held) -> BookedResource {
    BookedResource {
        resource: held.resource.to_string(),
        quantity: held.quantity,
    }
}

fn bookable(r: crate::ResourceSummary) -> BookableRecord {
    BookableRecord {
        id: r.id,
        name: r.name,
        name_latin: r.name_latin,
        kind: r.kind,
        capacity: r.capacity,
        withdrawn: r.withdrawn,
        withdrawn_why: r.withdrawn_why,
    }
}

fn view(r: crate::ReservationSummary) -> ReservationRecord {
    ReservationRecord {
        id: r.id,
        customer: r.customer_id,
        customer_name: r.customer_name,
        customer_phone: r.customer_phone,
        stage: r.stage,
        stage_why: r.stage_why,
        starts_at: r.starts_at,
        ends_at: r.ends_at,
        note: r.note,
    }
}

fn no_such_resource(id: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::NOT_FOUND,
        &erp_i18n::Message::new(crate::messages::NO_SUCH_RESOURCE)
            .with("resource", erp_i18n::MessageArg::text(id.to_owned())),
        locale,
        &CATALOG,
    )
}

fn problem_for(error: &CommandError<BookingError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Well-formed, and about something that is not there.
                BookingError::NoSuchCustomer(_)
                | BookingError::NoSuchResource(_)
                | BookingError::NoSuchReservation(_)
                | BookingError::NoSuchLine(_) => StatusCode::NOT_FOUND,

                // Well-formed, and refused on the state of the world. **This is
                // where a double booking lands**, and it is the most common
                // refusal this module makes: the request was fine, the chair
                // was not free.
                BookingError::Occupancy(_)
                | BookingError::NotOffered { .. }
                | BookingError::Withdrawn(_)
                | BookingError::Over { .. }
                | BookingError::CannotMove { .. } => StatusCode::UNPROCESSABLE_ENTITY,

                _ => StatusCode::BAD_REQUEST,
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

        other => {
            tracing::error!(error = %other, "booking command failed");
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
    tracing::error!(error = %error, "booking read failed");
    Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        &erp_i18n::Message::new(erp_tenant::messages::INTERNAL),
        locale,
        &CATALOG,
    )
}
