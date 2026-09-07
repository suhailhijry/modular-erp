//! **The tenant's clock**, as a setting somebody can see and change.
//!
//! `erp_types::Calendar` is read by every module that turns an instant into a
//! day — the diary, the rota, the tax return, the payroll, the reports — and
//! stamped onto every event when it is written. It had no setter: every tenant
//! was in Riyadh whether or not they were. This is the setter. Like every other
//! setting, a `GET` answers with the version as `ETag` and a `PUT` takes
//! `If-Match`.

use axum::http::StatusCode;
use erp_types::Calendar;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::{
    Allowed, AppState, IfMatch, Json, Language, ManageTenant, Problem, Read, Versioned,
    config_problem,
};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(tenant_calendar, set_tenant_calendar))
}

/// An IANA timezone, by name. Daylight saving, where the zone has it, is the
/// zone's business and not a setting.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({ "zone": "Asia/Riyadh" }))]
struct CalendarView {
    zone: String,
}

/// The clock every day, month and period on this tenant is read by.
#[utoipa::path(
    get,
    path = "/v1/tenant/calendar",
    tag = "tenants",
    params(("Host" = String, Header, description = "The tenant's host.")),
    responses(
        (status = OK, description = "`Asia/Riyadh` until somebody sets it.", body = CalendarView, headers(("ETag" = String, description = "The version of this setting. Send it back as `If-Match` to write only if nobody else has since."))),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn tenant_calendar(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Versioned<CalendarView>, Problem> {
    let mut conn = tenant.db.read().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    let version = erp_eventlog::configuration::version_of(&mut conn, Calendar::KEY)
        .await
        .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?;
    let calendar = erp_eventlog::configuration::calendar(&mut conn)
        .await
        .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?;
    Ok(Versioned(
        version,
        CalendarView {
            zone: calendar.name().to_owned(),
        },
    ))
}

/// Set it.
///
/// **Applies to what happens next.** Every event already written carries the
/// clock it was written under, so nothing that has happened moves; a report or
/// a return computed from now on reads new instants by the new clock.
#[utoipa::path(
    put,
    path = "/v1/tenant/calendar",
    tag = "tenants",
    params(
        ("Host" = String, Header, description = "The tenant's host."),
        ("If-Match" = Option<String>, Header, description = "The `ETag` a GET answered with. With it, the write happens only if the setting is still at that version; without it, unconditionally."),
    ),
    request_body = CalendarView,
    responses(
        (status = NO_CONTENT, description = "Set."),
        (status = BAD_REQUEST, description = "Not a name in the IANA zone database", body = Problem),
        (status = PRECONDITION_FAILED, description = "`If-Match` named a version that is no longer current; reload and try again", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_tenant_calendar(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    IfMatch(expected): IfMatch,
    Json(body): Json<CalendarView>,
) -> Result<StatusCode, Problem> {
    let calendar = Calendar::named(&body.zone).map_err(|e| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(erp_web::messages::NOT_A_ZONE)
                .with("zone", erp_i18n::MessageArg::text(e.0)),
            locale,
            &crate::CATALOG,
        )
    })?;
    let mut conn = tenant.db.acquire().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    erp_eventlog::configuration::set(
        &mut conn,
        Calendar::KEY,
        &calendar,
        Some(&tenant.session.identity.to_string()),
        expected,
    )
    .await
    .map_err(|e| config_problem(&e, locale, &crate::CATALOG))?;
    Ok(StatusCode::NO_CONTENT)
}
