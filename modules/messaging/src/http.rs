//! This module's HTTP surface.
//!
//! Translation only, like every module's.
//!
//! # Templates are a `PUT` and sending is a `POST`
//!
//! Saving a template is idempotent on its name — send it twice and the tenant
//! has one template — and sending is the act that happens once, under the
//! caller's own key. The methods say which is which without a reader having to
//! know the module.

use std::collections::BTreeMap;

use axum::extract::Path;
use axum::http::StatusCode;
use erp_eventlog::{ConfigError, configuration as config};
use erp_i18n::{Locale, Localize};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::{Allowed, Language, ManageTenant, PostEntries, Read};
use erp_web::{Json, Query, bad_request, parse_id, require_module};

use crate::audience::{Audience, Subject, Topic};
use crate::budget::Budget;
use crate::channel::Channel;
use crate::push::Platform;
use crate::settings::Settings;
use crate::template::{Body, Template, TemplateError, Templates};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_templates))
        .routes(routes!(vocabulary))
        .routes(routes!(get_template, put_template, delete_template))
        .routes(routes!(messaging_settings, set_messaging_settings))
        .routes(routes!(messaging_budget, set_messaging_budget))
        .routes(routes!(messaging_spend))
        .routes(routes!(register_device))
        .routes(routes!(send_message))
}

/// **What this module's routes can answer with.**
///
/// Its own failures, the four modules it reads to resolve an audience, and
/// everything any route can produce.
static CATALOG: erp_i18n::Composite = erp_i18n::Composite::new(&[
    &crate::CATALOG,
    &crm::CATALOG,
    &hr::CATALOG,
    &branches::CATALOG,
    &booking::CATALOG,
    &sales::CATALOG,
    &erp_web::CATALOG,
]);

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, ToSchema)]
struct BodyView {
    /// Only on email. Empty everywhere else.
    #[serde(default)]
    subject: String,
    text: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "channel": "sms",
    "topic": "reservation",
    "audience": "client",
    "active": true,
    "bodies": {
        "en": {"text": "{{ business }}: your appointment is at {{ reservation.starts_at }}. {{ link }}"},
        "ar": {"text": "{{ business }}: موعدك {{ reservation.starts_at }}. {{ link }}"}
    }
}))]
struct TemplateView {
    channel: String,
    /// What it is about — `reservation`, `invoice`, `customer` or `employee`.
    /// Decides which bindings and which audiences are allowed.
    topic: String,
    /// Who it goes to — `client`, `worker`, `branch_manager` or `operator`.
    /// **Not an address.**
    audience: String,
    /// One body per language. **Both are required**, per D12: neither is a
    /// translation of the other.
    bodies: BTreeMap<String, BodyView>,
    #[serde(default = "yes")]
    active: bool,
}

const fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, ToSchema)]
struct NamedTemplate {
    name: String,
    #[serde(flatten)]
    template: TemplateView,
    /// Every `{{ binding }}` this template uses, in the order it uses them.
    /// Answered so a caller editing one does not have to parse the bodies.
    uses: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Vocabulary {
    topic: String,
    /// Everything a template about this topic may say.
    bindings: Vec<String>,
    /// Everybody it may be addressed to.
    audiences: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({"business": "صالون بسمة", "language": "arabic"}))]
struct SettingsView {
    /// What to sign messages as. Not the slug and not the legal name — what a
    /// customer would recognise.
    business: String,
    /// `english` or `arabic`. What messages are written in when nothing says
    /// otherwise.
    language: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({"sms": 2000, "whatsapp": 2000, "email": null, "push": null}))]
struct BudgetView {
    /// **Segments, not messages.** An Arabic message over 70 characters is two.
    sms: Option<i32>,
    whatsapp: Option<i32>,
    email: Option<i32>,
    push: Option<i32>,
    /// Whether a person chose these, or they are what shipped. Read-only.
    #[serde(default)]
    configured: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct SpendView {
    period: String,
    channel: String,
    messages: i32,
    /// What is billed. Equal to `messages` on every channel but SMS.
    segments: i32,
    /// The cap in force, if there is one.
    limit: Option<i32>,
    /// What is left of it. `null` when there is no cap.
    remaining: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({"token": "fcm:abc…", "recipient": "CUST-0001", "platform": "fcm"}))]
struct NewDevice {
    /// The token the platform issued.
    token: String,
    /// Whoever the device belongs to, in whatever id space the caller uses.
    recipient: String,
    /// `apns`, `fcm` or `web`.
    platform: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[schema(example = json!({
    "template": "booking.reminder",
    "topic": "reservation",
    "subject": "BK-1041",
    "key": "booking.reminder.BK-1041"
}))]
struct NewMessage {
    /// Which template.
    template: String,
    /// What it is about, and which one. The **only** thing a caller supplies
    /// about content.
    topic: String,
    subject: String,
    /// **The caller's key**, and what makes a retry one message rather than
    /// two.
    key: String,
    /// Which identity, when the template is addressed to an operator.
    #[serde(default)]
    operator: Option<String>,
    /// Anything the caller resolved itself. `link` is the usual one.
    #[serde(default)]
    extra: BTreeMap<String, String>,
    /// `english` or `arabic`. Omitted takes the tenant's own.
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct SentView {
    channel: String,
    /// How many people the audience resolved to. It can be more than one.
    recipients: usize,
    /// **How many this call actually promised.** Fewer when the same key has
    /// been sent before, which is what makes retrying safe.
    promised: usize,
    /// What it cost, in billable units. Zero on a repeat.
    units: i32,
}

#[derive(Debug, Deserialize, ToSchema)]
struct Month {
    /// `YYYY-MM`. Omitted is the month the request arrives in.
    #[serde(default)]
    period: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// **What a template may say, and who it may be addressed to.**
///
/// The other half of "bindings are declared": an editor showing somebody the
/// list is why a wrong one is a typo they see rather than a gap a customer
/// reads. One entry per topic, because the vocabulary is per topic.
#[utoipa::path(
    get,
    path = "/v1/messaging/vocabulary",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = Vec<Vocabulary>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn vocabulary(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<Vec<Vocabulary>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    Ok(Json(
        Topic::ALL
            .into_iter()
            .map(|topic| Vocabulary {
                topic: topic.as_str().to_owned(),
                bindings: crate::template::vocabulary(topic)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                audiences: topic
                    .audiences()
                    .iter()
                    .map(|a| a.as_str().to_owned())
                    .collect(),
            })
            .collect(),
    ))
}

/// Every template, with what each one says and what it may say.
#[utoipa::path(
    get,
    path = "/v1/messaging/templates",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = Vec<NamedTemplate>),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_templates(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<Vec<NamedTemplate>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let templates = read(&tenant, locale).await?;

    Ok(Json(
        templates
            .entries
            .into_iter()
            .map(|(name, template)| named(name, template))
            .collect(),
    ))
}

/// One of them, and the vocabulary it is allowed to draw on.
#[utoipa::path(
    get,
    path = "/v1/messaging/templates/{name}",
    tag = "messaging",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("name" = String, Path, description = "The template's name."),
    ),
    responses(
        (status = OK, body = NamedTemplate),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, description = "No such template", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn get_template(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Path(name): Path<String>,
) -> Result<Json<NamedTemplate>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let templates = read(&tenant, locale).await?;

    let template = templates
        .entries
        .get(&name)
        .cloned()
        .ok_or_else(|| refused(&TemplateError::NoSuchTemplate(name.clone()), locale))?;

    Ok(Json(named(name, template)))
}

/// Save one.
///
/// **Every binding is checked here**, which is the whole point of declaring
/// them: an unresolvable `{{ … }}` is a `400` on the author's screen rather
/// than a gap in a message a customer is reading.
#[utoipa::path(
    put,
    path = "/v1/messaging/templates/{name}",
    tag = "messaging",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("name" = String, Path, description = "Lower case, digits, dots and underscores."),
    ),
    request_body = TemplateView,
    responses(
        (status = NO_CONTENT, description = "Saved."),
        (status = BAD_REQUEST, description = "An unknown binding, a missing language, an audience this topic does not have", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn put_template(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path(name): Path<String>,
    Json(body): Json<TemplateView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let template = decoded(&body, locale)?;
    template.check(&name).map_err(|e| refused(&e, locale))?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    // Read-modify-write, in one connection. Two people editing two different
    // templates at the same instant is the only race, and it is a settings
    // screen — `configuration` is one row per key and the loser's edit is the
    // one they are looking at.
    let mut templates = config::get::<Templates>(&mut conn, crate::template::KEY)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .map(|c| c.value)
        .unwrap_or_default();
    templates.entries.insert(name, template);

    config::set(
        &mut conn,
        crate::template::KEY,
        &templates,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| unavailable(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Remove one.
///
/// Removing a template that is not there is `204`, because deleting twice is
/// the same world either way (L8).
#[utoipa::path(
    delete,
    path = "/v1/messaging/templates/{name}",
    tag = "messaging",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("name" = String, Path, description = "The template's name."),
    ),
    responses(
        (status = NO_CONTENT, description = "Gone, or was never there."),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn delete_template(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Path(name): Path<String>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;

    let mut templates = config::get::<Templates>(&mut conn, crate::template::KEY)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .map(|c| c.value)
        .unwrap_or_default();

    if templates.entries.remove(&name).is_some() {
        config::set(
            &mut conn,
            crate::template::KEY,
            &templates,
            Some(&tenant.session.identity.to_string()),
        )
        .await
        .map_err(|e| unavailable(&e, locale))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// What the business is called, and what language it writes in.
#[utoipa::path(
    get,
    path = "/v1/messaging/settings",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, body = SettingsView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn messaging_settings(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<SettingsView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let settings = config::get::<Settings>(&mut conn, crate::settings::KEY)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .map(|c| c.value)
        .unwrap_or_default();

    Ok(Json(SettingsView {
        business: settings.business,
        language: language_name(settings.language).to_owned(),
    }))
}

/// Choose them.
#[utoipa::path(
    put,
    path = "/v1/messaging/settings",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    request_body = SettingsView,
    responses(
        (status = NO_CONTENT, description = "Stored."),
        (status = BAD_REQUEST, description = "Not a language this system speaks", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_messaging_settings(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Json(body): Json<SettingsView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let settings = Settings {
        business: body.business,
        language: language_of(&body.language, locale)?,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    config::set(
        &mut conn,
        crate::settings::KEY,
        &settings,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| unavailable(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// What may be sent in a month.
#[utoipa::path(
    get,
    path = "/v1/messaging/budget",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    responses(
        (status = OK, description = "`configured` says whether anybody chose these", body = BudgetView),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn messaging_budget(
    tenant: Allowed<Read>,
    Language(locale): Language,
) -> Result<Json<BudgetView>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let budget = config::get::<Budget>(&mut conn, crate::budget::KEY)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .map_or_else(Budget::default, |c| c.value);

    Ok(Json(BudgetView {
        sms: budget.sms,
        whatsapp: budget.whatsapp,
        email: budget.email,
        push: budget.push,
        configured: budget.configured,
    }))
}

/// Set it.
///
/// Saving marks the budget **configured**, which is how the read above can say
/// whether these numbers are a decision or what shipped.
#[utoipa::path(
    put,
    path = "/v1/messaging/budget",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    request_body = BudgetView,
    responses(
        (status = NO_CONTENT, description = "Stored."),
        (status = BAD_REQUEST, description = "A negative limit", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn set_messaging_budget(
    tenant: Allowed<ManageTenant>,
    Language(locale): Language,
    Json(body): Json<BudgetView>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    for limit in [body.sms, body.whatsapp, body.email, body.push]
        .into_iter()
        .flatten()
    {
        if limit < 0 {
            return Err(bad_request(
                crate::messages::NEGATIVE_BUDGET,
                "limit",
                &limit.to_string(),
                locale,
            ));
        }
    }

    let budget = Budget {
        sms: body.sms,
        whatsapp: body.whatsapp,
        email: body.email,
        push: body.push,
        // Not the caller's to claim: saving *is* configuring.
        configured: true,
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    config::set(
        &mut conn,
        crate::budget::KEY,
        &budget,
        Some(&tenant.session.identity.to_string()),
    )
    .await
    .map_err(|e| unavailable(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// What has been sent this month, and what is left.
#[utoipa::path(
    get,
    path = "/v1/messaging/spend",
    tag = "messaging",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("period" = Option<String>, Query, description = "`YYYY-MM`. Omitted is this month."),
    ),
    responses(
        (status = OK, description = "One row per channel, including channels that have sent nothing", body = Vec<SpendView>),
        (status = BAD_REQUEST, description = "Not a month", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn messaging_spend(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Query(month): Query<Month>,
) -> Result<Json<Vec<SpendView>>, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let period = match month.period {
        Some(period) => {
            if period.len() != 7 || !period.is_char_boundary(4) {
                return Err(bad_request(
                    crate::messages::NOT_A_MONTH,
                    "period",
                    &period,
                    locale,
                ));
            }
            period
        }
        None => crate::budget::period(now()),
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    let spent = crate::budget::spent(&mut conn, &period)
        .await
        .map_err(|e| sending_refused(&crate::SendError::Spend(e), locale))?;

    Ok(Json(
        spent
            .into_iter()
            .map(|s| SpendView {
                remaining: s.remaining(),
                period: s.period,
                channel: s.channel.as_str().to_owned(),
                messages: s.messages,
                segments: s.segments,
                limit: s.limit,
            })
            .collect(),
    ))
}

/// Register a device for push.
///
/// Idempotent on the token (L8), which is what an app calling this on every
/// launch needs. Re-registering also brings back a token the platform once
/// rejected: the device is offering it again, and refusing to believe the device
/// about its own token leaves somebody permanently unreachable.
#[utoipa::path(
    post,
    path = "/v1/messaging/devices",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    request_body = NewDevice,
    responses(
        (status = NO_CONTENT, description = "Registered."),
        (status = BAD_REQUEST, description = "Not a platform this system knows", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn register_device(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Json(body): Json<NewDevice>,
) -> Result<StatusCode, Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;
    let platform: Platform = body
        .platform
        .parse()
        .map_err(|e: crate::push::UnknownPlatform| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Message::new(crate::messages::UNKNOWN_PLATFORM)
                    .with("platform", erp_i18n::MessageArg::text(&e.0)),
                locale,
                &CATALOG,
            )
        })?;

    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    crate::push::register(&mut conn, &body.token, &body.recipient, platform, now())
        .await
        .map_err(|e| database(&e, locale))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Send one now.
///
/// Resolves the audience, renders the template, charges the meter and promises
/// the effect — **in one transaction**. A refusal writes nothing, including no
/// spend.
#[utoipa::path(
    post,
    path = "/v1/messaging/messages",
    tag = "messaging",
    params(("Host" = String, Header, description = "The tenant's subdomain.")),
    request_body = NewMessage,
    responses(
        (status = ACCEPTED, description = "Promised. Delivery is an effect, so this is not a delivery receipt — and `promised` is 0 when this key had already been sent.", body = SentView),
        (status = BAD_REQUEST, description = "No such template, an unusable subject, or nobody reachable", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = PAYMENT_REQUIRED, description = "This month's budget for that channel is spent", body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn send_message(
    tenant: Allowed<PostEntries>,
    Language(locale): Language,
    Json(body): Json<NewMessage>,
) -> Result<(StatusCode, Json<SentView>), Problem> {
    require_module(&tenant.db, &crate::module_id(), locale)?;

    let topic: Topic = body
        .topic
        .parse()
        .map_err(|e: crate::audience::UnknownTopic| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Message::new(crate::messages::UNKNOWN_TOPIC)
                    .with("topic", erp_i18n::MessageArg::text(&e.0)),
                locale,
                &CATALOG,
            )
        })?;
    let subject = Subject::new(topic, parse_id(&body.subject, locale)?);
    let language = body
        .language
        .as_deref()
        .map(|name| language_of(name, locale))
        .transpose()?;

    let sending = crate::Sending {
        template: body.template,
        subject,
        key: body.key,
        operator: body.operator,
        extra: body.extra,
        locale: language,
        at: now(),
    };

    let mut tx = tenant.db.begin().await.map_err(|e| pool(&e, locale))?;
    let sent = match crate::send(&mut tx, &sending).await {
        Ok(sent) => sent,
        Err(e) => {
            // **Rolled back, including the meter.** `send` writes the spend
            // before it checks the budget, because that write is the lock — so
            // a refusal that committed would charge for a message nobody got.
            let _ = tx.rollback().await;
            return Err(sending_refused(&e, locale));
        }
    };

    tx.commit().await.map_err(|e| {
        Problem::from_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_eventlog::ConfigError::Database(e),
            locale,
            &CATALOG,
        )
    })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SentView {
            channel: sent.channel.as_str().to_owned(),
            recipients: sent.recipients,
            promised: sent.promised,
            units: sent.units,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// The one clock reading in this module, and it is a request timestamp.
///
/// A person pressing "send" has no instant to give, unlike a command that
/// records when something happened. Everything downstream takes it as a
/// parameter, which is what keeps the meter and the template testable against a
/// fixture date.
fn now() -> Timestamp {
    chrono::Utc::now()
}

async fn read(tenant: &erp_web::Allowed<Read>, locale: Locale) -> Result<Templates, Problem> {
    let mut conn = tenant.db.acquire().await.map_err(|e| pool(&e, locale))?;
    Ok(config::get::<Templates>(&mut conn, crate::template::KEY)
        .await
        .map_err(|e| unavailable(&e, locale))?
        .map(|c| c.value)
        .unwrap_or_default())
}

fn named(name: String, template: Template) -> NamedTemplate {
    let mut uses: Vec<String> = template
        .bodies
        .values()
        .flat_map(|body| {
            crate::template::placeholders(&body.subject)
                .into_iter()
                .chain(crate::template::placeholders(&body.text))
        })
        .collect();
    uses.sort();
    uses.dedup();

    NamedTemplate {
        name,
        uses,
        template: TemplateView {
            channel: template.channel.as_str().to_owned(),
            topic: template.topic.as_str().to_owned(),
            audience: template.audience.as_str().to_owned(),
            active: template.active,
            bodies: template
                .bodies
                .into_iter()
                .map(|(locale, body)| {
                    (
                        locale,
                        BodyView {
                            subject: body.subject,
                            text: body.text,
                        },
                    )
                })
                .collect(),
        },
    }
}

fn decoded(view: &TemplateView, locale: Locale) -> Result<Template, Problem> {
    let channel: Channel = view.channel.parse().map_err(|e: crate::UnknownChannel| {
        Problem::new(
            StatusCode::BAD_REQUEST,
            &erp_i18n::Message::new(crate::messages::UNKNOWN_CHANNEL)
                .with("channel", erp_i18n::MessageArg::text(&e.0)),
            locale,
            &CATALOG,
        )
    })?;
    let topic: Topic = view
        .topic
        .parse()
        .map_err(|e: crate::audience::UnknownTopic| {
            Problem::new(
                StatusCode::BAD_REQUEST,
                &erp_i18n::Message::new(crate::messages::UNKNOWN_TOPIC)
                    .with("topic", erp_i18n::MessageArg::text(&e.0)),
                locale,
                &CATALOG,
            )
        })?;
    let audience: Audience =
        view.audience
            .parse()
            .map_err(|e: crate::audience::UnknownAudience| {
                Problem::new(
                    StatusCode::BAD_REQUEST,
                    &erp_i18n::Message::new(crate::messages::UNKNOWN_AUDIENCE)
                        .with("audience", erp_i18n::MessageArg::text(&e.0)),
                    locale,
                    &CATALOG,
                )
            })?;

    Ok(Template {
        channel,
        topic,
        audience,
        active: view.active,
        bodies: view
            .bodies
            .iter()
            .map(|(code, body)| {
                (
                    code.clone(),
                    Body {
                        subject: body.subject.clone(),
                        text: body.text.clone(),
                    },
                )
            })
            .collect(),
    })
}

fn language_name(locale: Locale) -> &'static str {
    match locale {
        Locale::English => "english",
        Locale::Arabic => "arabic",
    }
}

fn language_of(name: &str, locale: Locale) -> Result<Locale, Problem> {
    Locale::ALL
        .into_iter()
        .find(|l| language_name(*l) == name || l.code() == name)
        .ok_or_else(|| bad_request(crate::messages::UNKNOWN_LANGUAGE, "language", name, locale))
}

/// **`402` for a spent budget**, and `400` for everything else.
///
/// A budget is not a malformed request: the caller did everything right and the
/// month is out of money, which is a different thing for a client to branch on.
fn sending_refused(error: &crate::SendError, locale: Locale) -> Problem {
    let status = match error {
        crate::SendError::Spend(crate::SpendError::Refused(_)) => StatusCode::PAYMENT_REQUIRED,
        crate::SendError::Database(_)
        | crate::SendError::Config(_)
        | crate::SendError::Enqueue(_)
        | crate::SendError::Spend(_) => StatusCode::SERVICE_UNAVAILABLE,
        crate::SendError::Template(_) | crate::SendError::Unreachable { .. } => {
            StatusCode::BAD_REQUEST
        }
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn refused(error: &TemplateError, locale: Locale) -> Problem {
    let status = match error {
        TemplateError::NoSuchTemplate(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    Problem::new(status, &error.message(), locale, &CATALOG)
}

fn pool(error: &erp_tenant::PoolError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn unavailable(error: &ConfigError, locale: Locale) -> Problem {
    Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, error, locale, &CATALOG)
}

fn database(error: &sqlx::Error, locale: Locale) -> Problem {
    tracing::warn!(%error, "messaging could not read the database");
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(crate::messages::DATABASE),
        locale,
        &CATALOG,
    )
}
