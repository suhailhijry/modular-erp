//! Conversations, over HTTP.
//!
//! # Why reading needs `PostEntries`
//!
//! **The one place in this API where reading is not the most permissive
//! capability, and it is deliberate.** A thread holds staff's private notes
//! about a customer. `Read` is the role for an external accountant at year end
//! — somebody with every reason to see the books and none to see what the front
//! desk wrote about a client.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use erp_eventlog::ExecuteError;
use erp_i18n::{Locale, Localize};
use erp_tenant::CommandError;
use messaging::{Channel, Subject, Topic};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::Problem as ApiProblem;
use erp_web::{After, Allowed, Language, Paged, PostEntries};
use erp_web::{Consistency, nudge};
use erp_web::{Json, Query, metadata, parse_id, require_module};

use crate::ConversationError;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_unmatched))
        .routes(routes!(assign_unmatched))
        .routes(routes!(read_conversation))
        .routes(routes!(add_note))
        .routes(routes!(send_in_conversation))
}

static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &messaging::CATALOG, &erp_web::CATALOG]);

const PAGE: i64 = 50;
const MAX_PAGE: i64 = 200;
/// How many tray threads one listing shows.
const TRAY: i64 = 100;

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
struct ConversationLine {
    /// `note` — internal, `said` — sent to the customer, `heard` — they replied.
    kind: String,
    body: String,
    /// Set on `said` and `heard`.
    channel: Option<String>,
    address: Option<String>,
    /// The identity that wrote it. Absent on `heard`.
    who: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    said_at: erp_types::Timestamp,
}

#[derive(Debug, Serialize, ToSchema)]
struct UnmatchedView {
    /// The number nobody on the books has.
    address: String,
    messages: i32,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    last_at: erp_types::Timestamp,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "text": "Called, moving to Thursday." }))]
struct NewNote {
    text: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "text": "Thursday at 10 works.", "channel": "sms" }))]
struct OutwardMessage {
    text: String,
    /// `sms` or `email`. `WhatsApp` and push are refused — see the error.
    channel: String,
    #[serde(default)]
    #[schema(value_type = Option<chrono::DateTime<chrono::Utc>>)]
    at: Option<erp_types::Timestamp>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({ "topic": "reservation", "id": "BK-1" }))]
struct Assignment {
    /// `reservation`, `invoice`, `customer`, `employee` or `lot`.
    topic: String,
    /// The record's own id.
    id: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Written {
    position: Option<i64>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Everything said about one thing, oldest first.
#[utoipa::path(
    get,
    path = "/v1/conversations/{topic}/{subject}",
    tag = "conversations",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("topic" = String, Path, description = "`reservation`, `invoice`, `customer`, `employee` or `lot`."),
        ("subject" = String, Path, description = "The record's own id."),
        ("limit" = Option<i64>, Query, description = "Up to 200. Defaults to 50."),
        ("after" = Option<String>, Query, description = "The `next` from the previous page."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Paged<ConversationLine>),
        (status = BAD_REQUEST, description = "Not a topic, or not an id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, description = "A thread holds private notes; reading one needs more than `read`", body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn read_conversation(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    consistency: Consistency,
    Path(path): Path<std::collections::HashMap<String, String>>,
    Query(page): Query<After>,
) -> Result<Json<Paged<ConversationLine>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let subject = subject_of(&path, locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let after = page.cursor(locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let lines = crate::messages(
        &mut conn,
        crate::thread_id(&subject).as_str(),
        page.limit(PAGE, MAX_PAGE),
        after.as_ref(),
    )
    .await
    .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(Paged::of(lines, view)))
}

/// Write something down. **Internal — it never leaves.**
#[utoipa::path(
    post,
    path = "/v1/conversations/{topic}/{subject}/notes",
    tag = "conversations",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("topic" = String, Path, description = "`reservation`, `invoice`, `customer`, `employee` or `lot`."),
        ("subject" = String, Path, description = "The record's own id."),
    ),
    request_body = NewNote,
    responses(
        (status = OK, body = Written),
        (status = BAD_REQUEST, description = "Nothing in it, or not a topic", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn add_note(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(path): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<NewNote>,
) -> Result<Json<Written>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let subject = subject_of(&path, locale)?;

    let committed = crate::note(
        &tenant.db,
        &subject,
        &body.text,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(Written {
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Say something to the customer this is about.
///
/// Charged to the messaging meter, and refused when the month's budget is
/// spent. **`sms` or `email`**: `WhatsApp` takes approved templates outside a
/// 24-hour window and push reaches a device rather than a person.
#[utoipa::path(
    post,
    path = "/v1/conversations/{topic}/{subject}/messages",
    tag = "conversations",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("topic" = String, Path, description = "`reservation`, `invoice`, `customer`, `employee` or `lot`."),
        ("subject" = String, Path, description = "The record's own id."),
    ),
    request_body = OutwardMessage,
    responses(
        (status = OK, description = "Recorded and promised, in one transaction.", body = Written),
        (status = BAD_REQUEST, description = "Nothing in it, not a channel, or not a topic", body = Problem),
        (status = PAYMENT_REQUIRED, description = "This month's budget for that channel is spent", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Nobody to send to, or no address on that channel", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn send_in_conversation(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(path): Path<std::collections::HashMap<String, String>>,
    Json(body): Json<OutwardMessage>,
) -> Result<Json<Written>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let subject = subject_of(&path, locale)?;
    let channel: Channel = body.channel.parse().map_err(|_| {
        ApiProblem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(messaging::messages::UNKNOWN_CHANNEL)
                .with("channel", erp_i18n::MessageArg::text(body.channel.clone())),
            locale,
            &CATALOG,
        )
    })?;

    let committed = crate::say(
        &tenant.db,
        &subject,
        &body.text,
        channel,
        body.at.unwrap_or_else(chrono::Utc::now),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(Written {
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

/// Replies from numbers nobody on the books has.
#[utoipa::path(
    get,
    path = "/v1/conversations/unmatched",
    tag = "conversations",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("consistent_after" = Option<i64>, Query, description = "Wait for the read models to reach this log position."),
    ),
    responses(
        (status = OK, body = Vec<UnmatchedView>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_unmatched(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    consistency: Consistency,
) -> Result<Json<Vec<UnmatchedView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    consistency
        .wait_for(&tenant.db, crate::GROUP_NAME, locale)
        .await?;

    let mut conn = tenant.db.read().await.map_err(|e| pool(&e, locale))?;
    let waiting = crate::unmatched(&mut conn, TRAY)
        .await
        .map_err(|e| database(&e, locale))?;
    drop(conn);

    Ok(Json(
        waiting
            .into_iter()
            .map(|row| UnmatchedView {
                address: row.address,
                messages: row.messages,
                last_at: row.last_at,
            })
            .collect(),
    ))
}

/// Say what an unmatched conversation was about.
///
/// **Moves what has arrived, and binds nothing.** The next message from that
/// number lands here again — putting it on the customer's record is what stops
/// that, and `crm` is where a customer's number lives.
#[utoipa::path(
    post,
    path = "/v1/conversations/unmatched/{address}/assign",
    tag = "conversations",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("address" = String, Path, description = "The number, from `GET /v1/conversations/unmatched`."),
    ),
    request_body = Assignment,
    responses(
        (status = OK, description = "Moved onto that thread.", body = Written),
        (status = BAD_REQUEST, description = "Not a topic, or not an id", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = UNPROCESSABLE_ENTITY, description = "Already assigned", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn assign_unmatched(
    tenant: Allowed<PostEntries>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(address): Path<String>,
    Json(body): Json<Assignment>,
) -> Result<Json<Written>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let topic: Topic = body
        .topic
        .parse()
        .map_err(|_| unknown_topic(&body.topic, locale))?;
    let id = parse_id(&body.id, locale)?;

    let committed = crate::assign(
        &tenant.db,
        &address,
        &Subject::new(topic, id),
        chrono::Utc::now(),
        &metadata(&tenant),
    )
    .await
    .map_err(|e| refused(&e, locale))?;

    nudge(&state, tenant.db.tenant()).await;
    Ok(Json(Written {
        position: committed.at.map(erp_types::LogPosition::get),
    }))
}

// ---------------------------------------------------------------------------
// Views and refusals
// ---------------------------------------------------------------------------

fn view(line: crate::Line) -> ConversationLine {
    ConversationLine {
        kind: line.kind,
        body: line.body,
        channel: line.channel,
        address: line.address,
        who: line.who,
        said_at: line.said_at,
    }
}

/// The subject two path segments name.
fn subject_of(
    path: &std::collections::HashMap<String, String>,
    locale: Locale,
) -> Result<Subject, Problem> {
    let topic = path.get("topic").map_or("", String::as_str);
    let subject = path.get("subject").map_or("", String::as_str);
    let topic: Topic = topic.parse().map_err(|_| unknown_topic(topic, locale))?;
    Ok(Subject::new(topic, parse_id(subject, locale)?))
}

fn unknown_topic(topic: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::BAD_REQUEST,
        &erp_i18n::Message::new(messaging::messages::UNKNOWN_TOPIC)
            .with("topic", erp_i18n::MessageArg::text(topic.to_owned())),
        locale,
        &CATALOG,
    )
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::error!(%error, "a conversation could not be read");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}

fn refused(error: &CommandError<ConversationError>, locale: Locale) -> Problem {
    let (status, message) = match error {
        CommandError::Execute(ExecuteError::Rejected(rejection)) => (
            match rejection {
                // Malformed: nothing in it, or a channel a person may not type
                // into.
                ConversationError::NothingToSay | ConversationError::NotAChannelForThis(_) => {
                    StatusCode::BAD_REQUEST
                }
                // **`402` for a spent budget**, which is not a malformed
                // request: the caller did everything right and the month is out
                // of money, which is a different thing to branch on.
                ConversationError::OverBudget { .. } => StatusCode::PAYMENT_REQUIRED,
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
        other => {
            tracing::error!(error = %other, "a conversation command failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                erp_i18n::Message::new(crate::messages::DATABASE),
            )
        }
    };
    Problem::new(status, &message, locale, &CATALOG)
}
