//! The bell, over HTTP.
//!
//! # Why every route here is `Read`, including the three that write
//!
//! A viewer must be able to clear their own bell and say they do not want SMS.
//! Neither is an administrative act on the tenant: nothing here changes what
//! anybody may do, or what anybody else sees. Requiring `PostEntries` would
//! make the most junior person's inbox read-only, which is the opposite of the
//! point.
//!
//! # Why the caller is never a parameter
//!
//! Every route acts on `tenant.session.identity` and nothing accepts a
//! recipient. There is no way to spell "somebody else's bell" in this API, so
//! there is nothing to get wrong in a handler.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use messaging::Channel;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{After, Allowed, Language, Paged, Read};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, metadata, parse_id, require_module};

use crate::{Kind, NotificationError};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_notifications))
        .routes(routes!(mark_all_read))
        .routes(routes!(mark_read))
        .routes(routes!(get_preferences, set_preferences))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &messaging::CATALOG, &erp_web::CATALOG]);

/// How many notifications one page holds by default.
const PAGE: i64 = 30;
const MAX_PAGE: i64 = 100;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct NotificationView {
    id: String,
    /// One of `booking_reserved`, `payments_settled`, `payments_failed`,
    /// `tax_refused` or `document_expiring`.
    kind: String,
    /// What it is about: `reservation`, `invoice`, `customer` or `employee`,
    /// and which one. Fetch the record itself through its own route.
    topic: String,
    subject_id: String,
    /// In the language this request asked for.
    title: String,
    body: String,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    announced_at: erp_types::Timestamp,
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    read_at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Bell {
    #[serde(flatten)]
    page: Paged<NotificationView>,
    /// **The number on the badge**, for the whole inbox — not for this page.
    unread: i64,
}

#[derive(Debug, Deserialize)]
struct BellQuery {
    /// Only what has not been seen.
    #[serde(default)]
    unread: bool,
    #[serde(flatten)]
    page: After,
}

#[derive(Debug, Serialize, ToSchema)]
struct PreferenceView {
    /// One of the kinds from `GET /v1/notifications/preferences`.
    kind: &'static str,
    /// Where this person wants to hear about it. `in_system` is the bell.
    channels: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "entries": [{ "kind": "booking_reserved", "channels": ["in_system", "sms"] }]
}))]
struct SetPreferences {
    /// **The whole grid.** A kind left out goes back to its default, which is
    /// the bell and nothing that costs money.
    entries: Vec<PreferenceEntry>,
}

#[derive(Debug, Deserialize, ToSchema)]
struct PreferenceEntry {
    kind: String,
    channels: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Marked {
    /// How many were marked read.
    marked: usize,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// **Your own notifications**, newest first.
///
/// Nothing here can name somebody else: the recipient is the session's own
/// identity, and the read filters on it in SQL rather than after the fact.
#[utoipa::path(
    get,
    path = "/v1/notifications",
    tag = "notifications",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("unread" = Option<bool>, Query, description = "Only what you have not seen."),
        ("limit" = Option<i64>, Query, description = "Up to 100. Defaults to 30."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Bell),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_notifications(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
    Query(query): Query<BellQuery>,
) -> Result<Json<Bell>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let me = tenant.session.identity.to_string();
    let after = query.page.cursor(locale)?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let page = crate::inbox(
        &mut conn,
        &me,
        query.unread,
        query.page.limit(PAGE, MAX_PAGE),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;
    let unread = crate::unread(&mut conn, &me)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Bell {
        page: Paged::of(page, |row| view(row, locale)),
        unread,
    }))
}

/// Mark one of yours read.
///
/// **404 for somebody else's**, not 403: a "forbidden" would confirm that a
/// notification with that id exists and that it was not yours.
#[utoipa::path(
    post,
    path = "/v1/notifications/{notification}/read",
    tag = "notifications",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("notification" = String, Path, description = "From `GET /v1/notifications`."),
    ),
    responses(
        (status = NO_CONTENT, description = "Read. Already-read is the same answer."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No notification of yours has that id", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn mark_read(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let notification = parse_id(&id, locale)?;
    let me = tenant.session.identity.to_string();

    crate::read(
        &tenant.db,
        &notification,
        &me,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Clear the bell: everything of yours that is unread, now read.
#[utoipa::path(
    post,
    path = "/v1/notifications/read",
    tag = "notifications",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    responses(
        (status = OK, description = "Cleared. An empty bell is the same answer.", body = Marked),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn mark_all_read(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Json<Marked>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let me = tenant.session.identity.to_string();

    // What the read model says is unread, for the count only. The event covers
    // whatever is unread when it applies, which is the exact set — see
    // `crate::person`.
    let marked = {
        let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
        crate::unread_ids(&mut conn, &me, crate::commands::MARK_ALL_LIMIT)
            .await
            .map_err(|e| database(&e, locale))?
            .len()
    };

    crate::read_all(&tenant.db, &me, chrono::Utc::now(), &metadata(&tenant))
        .await
        .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(Marked { marked }))
}

/// What you have asked to hear about, and where.
///
/// **Every kind, with the defaults filled in** — a client rendering the grid
/// should not have to know what this system's defaults are.
#[utoipa::path(
    get,
    path = "/v1/notifications/preferences",
    tag = "notifications",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<PreferenceView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_preferences(
    tenant: Allowed<Read>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<PreferenceView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let me = tenant.session.identity.to_string();
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let said = crate::preferences(&mut conn, &me)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(
        Kind::ALL
            .into_iter()
            .map(|kind| PreferenceView {
                kind: kind.as_str(),
                channels: said.get(kind.as_str()).map_or_else(
                    || {
                        crate::DEFAULT_CHANNELS
                            .iter()
                            .map(|c| c.as_str().to_owned())
                            .collect()
                    },
                    |channels| channels.iter().map(|c| c.as_str().to_owned()).collect(),
                ),
            })
            .collect(),
    ))
}

/// Say what you want to hear about, and where.
///
/// **The whole grid, replacing.** A kind left out of the body goes back to its
/// default — the bell and nothing billable — which is what makes two open tabs
/// unable to produce a setting nobody chose.
#[utoipa::path(
    put,
    path = "/v1/notifications/preferences",
    tag = "notifications",
    params(("Host" = String, Header, description = "The tenant's subdomain."),),
    request_body = SetPreferences,
    responses(
        (status = NO_CONTENT, description = "Saved. The same grid again is the same answer."),
        (status = BAD_REQUEST, description = "No such kind, or no such channel", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_preferences(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
    Json(body): Json<SetPreferences>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let mut entries: BTreeMap<String, Vec<Channel>> = BTreeMap::new();
    for entry in body.entries {
        let kind: Kind = entry
            .kind
            .parse()
            .map_err(|_| bad_request(crate::messages::UNKNOWN_KIND, "kind", &entry.kind, locale))?;
        let mut channels = Vec::new();
        for name in &entry.channels {
            channels.push(name.parse::<Channel>().map_err(|_| {
                bad_request(crate::messages::UNKNOWN_CHANNEL, "channel", name, locale)
            })?);
        }
        entries.insert(kind.as_str().to_owned(), channels);
    }

    let me = tenant.session.identity.to_string();
    crate::set_preferences(
        &tenant.db,
        &me,
        entries,
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Views and refusals
// ---------------------------------------------------------------------------

fn view(row: crate::InboxRow, locale: Locale) -> NotificationView {
    // Both languages were rendered when it was announced, so this is a lookup
    // rather than a render. English when the language asked for is missing,
    // which can only happen to a notification announced by an older build.
    let said = row
        .wording
        .get(locale.code())
        .or_else(|| row.wording.get(Locale::DEFAULT.code()));
    NotificationView {
        id: row.id,
        kind: row.kind,
        topic: row.topic,
        subject_id: row.subject_id,
        title: said.map(|w| w.title.clone()).unwrap_or_default(),
        body: said.map(|w| w.body.clone()).unwrap_or_default(),
        announced_at: row.announced_at,
        read_at: row.read_at,
    }
}

fn bad_request(code: erp_i18n::MessageCode, arg: &str, value: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::BAD_REQUEST,
        &erp_i18n::Message::new(code).with(arg, erp_i18n::MessageArg::text(value.to_owned())),
        locale,
        &CATALOG,
    )
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "notifications read failed");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn refused(error: &CommandError<NotificationError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        // **The only refusal, and it is a 404.** See `crate::commands::read`.
        CommandError::Execute(ExecuteError::Rejected(rejection)) => {
            (StatusCode::NOT_FOUND, rejection.message())
        }
        CommandError::Pool(e @ erp_tenant::PoolError::Overloaded { .. }) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.message())
        }
        CommandError::Execute(ExecuteError::Contended { .. }) => (
            StatusCode::CONFLICT,
            erp_i18n::Message::new(erp_eventlog::messages::CONCURRENT_MODIFICATION),
        ),
        other => {
            tracing::error!(error = %other, "notifications command failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                erp_i18n::Message::new(crate::messages::DATABASE),
            )
        }
    };
    Problem::new(status, &message, locale, &CATALOG)
}
