//! Watching a tenant live: the two streams a projection advance reaches.
//!
//! # A signal, not the data
//!
//! What goes down the wire is *group `booking` is queryable through position
//! N*. The client re-fetches through the ordinary API with
//! `?consistent_after=N`, which already does authorization, localization and
//! paging; a payload stream would need all three again and would make the log
//! a query engine (L7).
//!
//! # A stream holds only ids
//!
//! `TenantDb` is deliberately not `Clone`, and a stream must not hold a
//! database connection for ten minutes. So the handler reads what it needs —
//! the checkpoints for `ready`, the module list for filtering — and the stream
//! keeps a receiver, a deadline and a list of module ids. A watcher that falls
//! behind the buffer is sent `reconnect` and closed; its reconnect is the
//! fresh `ready`.
//!
//! # Two surfaces, two registries
//!
//! Staff watch a tenant; a phone watches its reservation. The hub keeps them in
//! separate registries with separate caps (see `erp_web::realtime`), so the
//! public route here shares code with the staff one and shares no budget.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, KeepAliveStream, Sse};
use erp_control::shared::Advanced;
use erp_i18n::Locale;
use erp_types::{ModuleId, StreamId, TenantId};
use erp_web::realtime::{Full, Hub};
use erp_web::{Allowed, AppState, Language, Problem, Public, Read, nudge};
use futures_util::{Stream, StreamExt as _};
use tokio::sync::broadcast;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::CATALOG;

pub(crate) fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(event_stream))
        .routes(routes!(public_reservation_events))
}

/// What a stream keeps: nothing that touches a database.
pub(crate) enum Watch {
    /// Every group of every module this tenant had when the stream opened.
    Staff { modules: Vec<ModuleId> },
    /// One subject; the hub has already filtered.
    Subject,
}

type Live = Sse<KeepAliveStream<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>>;

struct Open {
    receiver: broadcast::Receiver<Advanced>,
    deadline: tokio::time::Instant,
    watch: Watch,
    done: bool,
}

/// The `ready` event first, then every signal the watch wants, then
/// `reconnect` at the hub's lifetime or when the watcher lagged.
pub(crate) fn live(
    hub: &Hub,
    ready: Event,
    receiver: broadcast::Receiver<Advanced>,
    watch: Watch,
) -> Live {
    let open = Open {
        receiver,
        deadline: tokio::time::Instant::now() + hub.lifetime(),
        watch,
        done: false,
    };
    let signals = futures_util::stream::unfold(open, |mut open| async move {
        if open.done {
            return None;
        }
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(open.deadline) => {
                    open.done = true;
                    return Some((Ok(Event::default().event("reconnect")), open));
                }
                next = open.receiver.recv() => match next {
                    Ok(signal) => {
                        let data = match &open.watch {
                            Watch::Staff { modules } => {
                                if !modules.contains(&signal.module) {
                                    continue;
                                }
                                serde_json::json!({ "group": signal.group, "position": signal.position })
                            }
                            Watch::Subject => serde_json::json!({ "position": signal.position }),
                        };
                        let event = Event::default().event("advanced").data(data.to_string());
                        return Some((Ok(event), open));
                    }
                    // Behind the buffer: a fresh snapshot is what it needs, and
                    // the reconnect is how it gets one without this stream
                    // touching a database.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        open.done = true;
                        return Some((Ok(Event::default().event("reconnect")), open));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(futures_util::stream::iter([Ok(ready)]).chain(signals));
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(hub.keep_alive())
            .text("keep-alive"),
    )
}

pub(crate) fn no_realtime(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(erp_web::messages::NO_REALTIME),
        locale,
        &CATALOG,
    )
}

/// The cap is reached. The wait is in the message, as it is for every other
/// public refusal here; nothing in this API sets `Retry-After`.
pub(crate) fn full(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::TOO_MANY_REQUESTS,
        &erp_i18n::Message::new(erp_web::messages::TOO_MANY_STREAMS),
        locale,
        &CATALOG,
    )
}

/// The hub, or the refusal a deployment without Redis gets.
pub(crate) fn hub_of(state: &AppState, locale: Locale) -> Result<&Arc<Hub>, Problem> {
    state.realtime.as_ref().ok_or_else(|| no_realtime(locale))
}

/// Watch this business live.
///
/// A stream of **signals, not data**. The first event is `ready`, naming every
/// projection group this business has a module for and the position each is
/// queryable through. After that, `advanced` with a group and a position each
/// time the worker commits, and `reconnect` when the stream has lived ten
/// minutes — reconnect, and `ready` says what moved. Re-fetch through the
/// ordinary API with `?consistent_after=<position>`; never apply a delta, the
/// stream carries none. A keep-alive comment every fifteen seconds.
#[utoipa::path(
    get,
    path = "/v1/events",
    tag = "service",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, description = "An event stream: `ready`, then `advanced`, then `reconnect`.", content_type = "text/event-stream", body = String),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = TOO_MANY_REQUESTS, description = "Too many streams open for this business on this server. Try again in a moment.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no Redis, so nothing can be watched live.", body = Problem),
    ),
)]
async fn event_stream(
    tenant: Allowed<Read>,
    State(state): State<AppState>,
    Language(locale): Language,
) -> Result<Live, Problem> {
    let hub = hub_of(&state, locale)?;
    let tenant_id: TenantId = tenant.db.tenant();
    let receiver = hub.watch_tenant(tenant_id).map_err(|Full| full(locale))?;
    nudge(&state, tenant_id).await;

    // What this tenant may see, and where each group stands — read once, on a
    // connection released before the stream begins.
    let visible: Vec<(String, ModuleId)> = crate::modules::available()
        .into_iter()
        .filter(|(_, setup)| tenant.db.has_module(&setup.module))
        .flat_map(|(_, setup)| {
            setup
                .groups
                .iter()
                .map(move |(group, _)| ((*group).to_owned(), setup.module.clone()))
        })
        .collect();
    let mut groups = serde_json::Map::new();
    {
        let mut conn = tenant
            .db
            .read()
            .await
            .map_err(|e| erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
        for (group, _) in &visible {
            let position = erp_projection::checkpoint_of(&mut conn, group)
                .await
                .map_err(|e| erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
            groups.insert(group.clone(), serde_json::Value::from(position.get()));
        }
    }
    let modules: Vec<ModuleId> = visible.into_iter().map(|(_, module)| module).collect();
    let ready = Event::default()
        .event("ready")
        .data(serde_json::json!({ "groups": groups }).to_string());

    Ok(live(hub, ready, receiver, Watch::Staff { modules }))
}

/// Watch one booking live.
///
/// For the phone that booked: `ready` with the position `booking` is queryable
/// through, then `advanced` each time this reservation moves — a deposit
/// confirmed, a stage changed — and `reconnect` after ten minutes. Re-fetch the
/// deposit status with `?consistent_after=<position>`. Keyed on the
/// reservation's id, which only the phone that booked it holds, and bounded by
/// the same per-origin and per-business limits as every public route.
#[utoipa::path(
    get,
    path = "/v1/booking/public/reservations/{reservation}/events",
    tag = "booking",
    params(
        ("Host" = String, Header, description = "The tenant's subdomain — which is how a public request names the business."),
        ("reservation" = String, Path, description = "From `POST /v1/booking/public/reservations`."),
    ),
    security(),
    responses(
        (status = OK, description = "An event stream: `ready`, then `advanced`, then `reconnect`.", content_type = "text/event-stream", body = String),
        (status = BAD_REQUEST, description = "Not an id", body = Problem),
        (status = NOT_FOUND, description = "No such business, or it does not take bookings online", body = Problem),
        (status = TOO_MANY_REQUESTS, description = "This surface is bounded per origin and per business, and streams per business on this server.", body = Problem),
        (status = SERVICE_UNAVAILABLE, description = "This deployment has no Redis, so nothing can be watched live.", body = Problem),
    ),
)]
async fn public_reservation_events(
    caller: Public,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(reservation): Path<String>,
) -> Result<Live, Problem> {
    let hub = hub_of(&state, locale)?;
    if !caller.db.has_module(&booking::module_id()) {
        return Err(crate::deposits::nothing_here(locale));
    }
    let reservation = erp_web::parse_id(reservation.trim(), locale)?;
    if !crate::deposits::public_settings(&caller, locale)
        .await?
        .open
    {
        return Err(crate::deposits::nothing_here(locale));
    }

    let tenant_id: TenantId = caller.db.tenant();
    let stream = StreamId::new(
        <booking::Reservation as erp_eventlog::Aggregate>::domain(),
        reservation.clone(),
    );
    let receiver = hub
        .watch_subject(tenant_id, stream)
        .map_err(|Full| full(locale))?;
    nudge(&state, tenant_id).await;

    let position = {
        let mut conn = caller
            .db
            .read()
            .await
            .map_err(|e| erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG))?;
        erp_projection::checkpoint_of(
            &mut conn,
            <booking::Booking as erp_projection::ProjectionGroup>::NAME,
        )
        .await
        .map_err(|e| erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG))?
    };
    let ready = Event::default().event("ready").data(
        serde_json::json!({ "reservation": reservation.as_str(), "position": position.get() })
            .to_string(),
    );
    Ok(live(hub, ready, receiver, Watch::Subject))
}
