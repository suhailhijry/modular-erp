//! Inbound callbacks.
//!
//! # The first inbound surface
//!
//! Every integration before this has been the system talking. A provider talks
//! back, and **a callback that is trusted without being verified is somebody
//! else's command executed under your authority** — anybody who can reach the
//! URL can say a payment succeeded.
//!
//! So the signature is checked before the body is treated as meaning anything,
//! and a callback that does not verify is refused rather than queued for
//! somebody to look at. See [`erp_web::webhook`] for what verification is.
//!
//! # A webhook is a command with the provider's id as its key
//!
//! It will be delivered more than once, out of order, and replayed by anybody
//! who kept a copy. All three are answered by two decisions: verify the
//! signature, and record the id.
//!
//! # Accepted fast, processed as an effect
//!
//! A provider that times out retries, and a retry storm is self-inflicted. So
//! the route verifies, records and promises — and answers `202` — leaving the
//! work to whichever module registered a handler for `webhook.<provider>`.
//!
//! **Nothing registers one yet.** Payments are 12a and a delivery receipt is
//! `messaging`'s to claim; until one does, a verified callback is recorded and
//! its effect waits in the outbox, which is the dispatcher's documented
//! behaviour for a kind nobody handles. `GET .../events` is what makes that
//! visible in the meantime.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use erp_i18n::{Locale, Localize};
use erp_types::{EffectKind, Timestamp};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use erp_web::AppState;
use erp_web::Problem;
use erp_web::webhook::{TOLERANCE_SECONDS, WebhookError};
use erp_web::{Allowed, Language, ManageTenant, Public, Read};
use erp_web::{Json, bad_request};

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(receive_callback))
        .routes(routes!(set_webhook_secret))
        .routes(routes!(list_webhook_events))
}

/// The header a provider signs in.
const SIGNATURE: &str = "x-webhook-signature";
/// …and the one carrying what was signed with it.
const TIMESTAMP: &str = "x-webhook-timestamp";

/// How many events one listing gives back.
const PAGE: i64 = 200;

/// Where a provider's shared secret is sealed.
fn secret_key(provider: &str) -> String {
    format!("webhooks.{provider}")
}

#[derive(Debug, Deserialize, ToSchema)]
struct NewSecret {
    /// The shared secret, as the provider shows it. Stored sealed, and never
    /// readable again.
    secret: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct EventRecord {
    provider: String,
    /// **The provider's own id.** What makes a retry a retry.
    event_id: String,
    kind: Option<String>,
    #[schema(value_type = chrono::DateTime<chrono::Utc>)]
    received_at: Timestamp,
    /// How many times they have sent it. More than one means their retries are
    /// not being acknowledged, or they simply retry regardless.
    deliveries: i32,
}

#[derive(Debug, Serialize, ToSchema)]
struct Accepted {
    /// The provider's id, echoed so a caller can correlate.
    event_id: String,
    /// **True when this exact event had already arrived.** Still a `202`: the
    /// provider did nothing wrong, and answering an error would make them retry
    /// something that is already done.
    duplicate: bool,
}

/// **Receive a callback.**
///
/// Public, because a provider has no account here — the tenant is the subdomain
/// and the signature is the credential.
///
/// The signed message is `<x-webhook-timestamp>.<body>`, HMAC-SHA256, hex, in
/// `x-webhook-signature`. The timestamp is inside the signature, so a copy
/// somebody kept cannot be re-sent with a fresh one.
#[utoipa::path(
    post,
    path = "/v1/hooks/{provider}",
    tag = "service",
    security(),
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("provider" = String, Path, description = "Whose callback this is — the name the secret was stored under."),
        ("x-webhook-signature" = String, Header, description = "HMAC-SHA256 of `<timestamp>.<body>`, hex."),
        ("x-webhook-timestamp" = String, Header, description = "Unix seconds. Must be within five minutes, and it is inside the signature."),
    ),
    request_body(content = String, description = "The provider's own payload, verbatim.", content_type = "application/json"),
    responses(
        (status = ACCEPTED, description = "Verified and recorded. `duplicate` says whether it had already arrived.", body = Accepted),
        (status = BAD_REQUEST, description = "Not JSON, or no event id in it", body = Problem),
        (status = UNAUTHORIZED, description = "Did not verify. Unsigned, wrong signature, and expired are one answer.", body = Problem),
        (status = NOT_FOUND, description = "No such tenant", body = Problem),
        (status = TOO_MANY_REQUESTS, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "No secret configured for that provider, or the database is unwell", body = Problem),
    ),
)]
async fn receive_callback(
    tenant: Public,
    Language(locale): Language,
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Accepted>), Problem> {
    let provider = named(&provider, locale)?;

    // **Before the body is read as anything.** A deployment with no sealing key
    // refuses rather than accepting unverified callbacks — the same call every
    // other secret makes.
    let Some(sealing) = state.sealing.clone() else {
        return Err(unverifiable(&WebhookError::NoSecret, &provider, locale));
    };
    let mut conn = tenant.db.acquire().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;

    let secret = erp_eventlog::secrets::get(&mut conn, &sealing, &secret_key(&provider))
        .await
        .map_err(|e| {
            tracing::warn!(%e, provider, "a webhook secret could not be unsealed");
            unverifiable(&WebhookError::NoSecret, &provider, locale)
        })?
        .ok_or_else(|| unverifiable(&WebhookError::NoSecret, &provider, locale))?;

    erp_web::webhook::verify(
        &secret,
        header(&headers, TIMESTAMP),
        &body,
        header(&headers, SIGNATURE),
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| refused(&e, locale))?;

    // Only now is the body worth parsing.
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            &e.to_string(),
            locale,
        )
    })?;
    let event_id = event_id(&payload).ok_or_else(|| {
        bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "no id in the payload",
            locale,
        )
    })?;
    let kind = payload
        .get("type")
        .or_else(|| payload.get("event"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    // **The dedupe and the promise, in one transaction.** A row written whose
    // effect was not promised is a callback nothing will ever process, and the
    // provider will not send it again.
    let mut tx = tenant.db.begin().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;

    let fresh = sqlx::query!(
        "INSERT INTO webhook_event (provider, event_id, kind, payload)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider, event_id) DO UPDATE
             SET deliveries = webhook_event.deliveries + 1
         RETURNING (xmax = 0) as \"fresh!\"",
        provider,
        event_id,
        kind,
        payload,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| unwell(e, locale))?
    .fresh;

    if fresh {
        let kind = EffectKind::new(format!("webhook.{provider}")).map_err(|_| {
            bad_request(erp_web::messages::INVALID_ID, "provider", &provider, locale)
        })?;
        let effect = erp_eventlog::Effect::with_key(
            kind,
            // **The provider's id is the key.** Arriving twice promises once,
            // which is the same mechanism the row above uses and the reason
            // both are in one transaction.
            format!("{provider}.{event_id}"),
            payload,
        );
        erp_eventlog::enqueue(&mut tx, None, std::slice::from_ref(&effect))
            .await
            .map_err(|e| {
                Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
            })?;
    }

    tx.commit().await.map_err(|e| unwell(e, locale))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            event_id,
            duplicate: !fresh,
        }),
    ))
}

/// Store the shared secret a provider signs with.
///
/// Sealed, and never readable again — the same posture as a ZATCA signing key,
/// and for the same reason: a secret that can be read back has as many copies as
/// there are people who can read it.
#[utoipa::path(
    put,
    path = "/v1/hooks/{provider}/secret",
    tag = "service",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("provider" = String, Path, description = "Lower case, digits and underscores."),
    ),
    request_body = NewSecret,
    responses(
        (status = NO_CONTENT, description = "Stored, sealed."),
        (status = BAD_REQUEST, description = "Not a provider name, or an empty secret", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no sealing key, so nothing was stored", body = Problem),
    ),
)]
async fn set_webhook_secret(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(provider): Path<String>,
    Json(body): Json<NewSecret>,
) -> Result<StatusCode, Problem> {
    let provider = named(&provider, locale)?;
    if body.secret.trim().is_empty() {
        return Err(bad_request(
            erp_web::messages::MALFORMED_BODY,
            "reason",
            "an empty secret",
            locale,
        ));
    }

    // **Refuses rather than storing it in the clear** (L6).
    let Some(sealing) = state.sealing.clone() else {
        return Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            &erp_i18n::Message::new(erp_web::messages::NO_SEALING_KEY),
            locale,
            &crate::CATALOG,
        ));
    };

    let mut conn = tenant.db.acquire().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;
    erp_eventlog::secrets::put(
        &mut conn,
        &sealing,
        &secret_key(&provider),
        body.secret.trim().as_bytes(),
    )
    .await
    .map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// What has arrived recently.
///
/// The answer to "a provider says it sent us something" — which is the question
/// after an outage, and the reason the payload is kept.
#[utoipa::path(
    get,
    path = "/v1/hooks/{provider}/events",
    tag = "service",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain."),
        ("provider" = String, Path, description = "Whose callbacks."),
    ),
    responses(
        (status = OK, body = Vec<EventRecord>),
        (status = BAD_REQUEST, description = "Not a provider name", body = Problem),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = SERVICE_UNAVAILABLE, body = Problem),
    ),
)]
async fn list_webhook_events(
    tenant: Allowed<Read>,
    Language(locale): Language,
    Path(provider): Path<String>,
) -> Result<Json<Vec<EventRecord>>, Problem> {
    let provider = named(&provider, locale)?;
    let mut conn = tenant.db.read().await.map_err(|e| {
        Problem::from_error(StatusCode::SERVICE_UNAVAILABLE, &e, locale, &crate::CATALOG)
    })?;

    let rows = sqlx::query!(
        r#"SELECT provider as "provider!", event_id as "event_id!", kind,
                  received_at as "received_at!", deliveries as "deliveries!"
             FROM webhook_event
            WHERE provider = $1
            ORDER BY received_at DESC
            LIMIT $2"#,
        provider,
        PAGE,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| unwell(e, locale))?;

    Ok(Json(
        rows.into_iter()
            .map(|row| EventRecord {
                provider: row.provider,
                event_id: row.event_id,
                kind: row.kind,
                received_at: row.received_at,
                deliveries: row.deliveries,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// The provider's own id for this event, wherever they put it.
///
/// `id` and `event_id` are what every provider this would plausibly be pointed
/// at uses. **A payload with neither is refused**, because without it there is
/// nothing to deduplicate on and "arriving twice does nothing twice" would stop
/// being true.
fn event_id(payload: &serde_json::Value) -> Option<String> {
    for field in ["id", "event_id", "eventId"] {
        if let Some(value) = payload.get(field) {
            return match value {
                serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
                serde_json::Value::Number(number) => Some(number.to_string()),
                _ => None,
            };
        }
    }
    None
}

/// The database is unwell, which is ours and never the caller's.
fn unwell(error: sqlx::Error, locale: Locale) -> Problem {
    tracing::warn!(%error, "a webhook could not be recorded");
    erp_web::ApiError::Access(erp_control::AccessError::Database(error))
        .into_problem(locale, &crate::CATALOG)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

fn named(provider: &str, locale: Locale) -> Result<String, Problem> {
    let usable = !provider.is_empty()
        && provider.len() <= 40
        && provider.starts_with(|c: char| c.is_ascii_lowercase())
        && provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');

    if usable {
        Ok(provider.to_owned())
    } else {
        Err(bad_request(
            erp_web::messages::INVALID_ID,
            "provider",
            provider,
            locale,
        ))
    }
}

/// **`401` for everything a stranger can provoke.**
///
/// Unsigned, wrong signature, unreadable timestamp and expired are one answer,
/// because telling them which they got is an oracle for guessing the rest.
fn refused(error: &WebhookError, locale: Locale) -> Problem {
    tracing::debug!(%error, tolerance = TOLERANCE_SECONDS, "a callback did not verify");
    Problem::new(
        StatusCode::UNAUTHORIZED,
        &error.message(),
        locale,
        &crate::CATALOG,
    )
}

/// …and `503` for the one that is ours: no secret to verify against.
fn unverifiable(error: &WebhookError, provider: &str, locale: Locale) -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &error
            .message()
            .with("provider", erp_i18n::MessageArg::text(provider)),
        locale,
        &crate::CATALOG,
    )
}
