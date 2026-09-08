# Real time, part one: the signal stream (Phase 13a + 13b)

*Design, 2026-09-08. Every section was approved in conversation before it was
written down. Phase 13c (notifications) and 13d (conversations) are separate
specs, in that order, after this lands.*

## Goal

A customer books from a phone and every counter screen shows it within a
second, without anybody polling. The phone that made the booking learns that
its deposit was confirmed the same way. **Exit:** two browsers and a phone
agree about a schedule within a second of a booking, and nobody polled.

## What exists, and what this composes

| piece | where | used for |
|---|---|---|
| Redis channel with publish/subscribe and a resubscribing consumer loop | `crates/erp-control/src/shared.rs` (`Invalidate`, `publish`, `subscribe`, `apply_invalidations_in_background`) | the shape of the new channel and its consumer |
| The projection job's commit | `crates/erp-worker/src/jobs.rs:96`, the `Progress::Advanced` arm | the one moment the read model is queryable through a position |
| `projection_checkpoint` and `checkpoint_of` | `crates/erp-projection/src/runner.rs` | "group G is queryable through N", already what `?consistent_after` waits on |
| `request_visit` | `crates/erp-control/src/lib.rs` | waking a dormant tenant's worker when a stream opens |
| `Allowed<Read>`, `TenantDb::has_module`, `ModuleSetup.groups` | `erp-web`, `erp-tenant` | who may watch, and which groups they may see |
| `Public` extractor, already charging the public limiter per open | `crates/erp-web/src/extract.rs` | the customer's surface |
| axum 0.8 `response::sse`, `tokio::sync::broadcast`, `futures-util` | already in the workspace | no new dependency |

`pg_notify` stays refused (D4). Redis is the real-time transport.

## Decisions

| question | decision |
|---|---|
| Transport | Server-sent events. One-directional, reconnects by itself. |
| What is sent | A signal, never data: *group G is queryable through position N*. The client re-fetches through the ordinary API. |
| When it is sent | After the projection job commits, once per tick, with the committed position. Never on append. |
| Fan-out | One Redis message per advance; one subscriber task per API node; per-tenant and per-subject `broadcast` senders in memory. A stream holds no database connection. |
| Staff and customers | Two surfaces on one hub, with separate senders and separate caps, so neither can starve the other. Staff: one stream per tenant. Public: one stream per reservation, keyed by the reservation's id the way the deposit status is. |
| Which streams a batch touched | The signal carries the unique stream ids of the batch, or "many" past 256, so a phone is woken only for its own reservation and no stream reads the database to find out. |
| Reconnect | Every stream closes itself after ten minutes; the browser reconnects; the first event of a stream is a snapshot, so reconnection and first connection are the same path. No `Last-Event-ID`. |
| Without Redis | The route refuses (503). A stream that would be silent is worse than none (L6). |
| Caps | `REALTIME_STAFF_STREAMS_PER_TENANT` default 256 and `REALTIME_PUBLIC_STREAMS_PER_TENANT` default 4096, per node. Past them, 429 whose message says to wait a moment (nothing in this API sets `Retry-After`; the public limiter's 429 is the same shape). The public limiter's per-caller window applies on every public open, as it does to every public route. |

## Components

### 1. The signal (`erp-control`, `erp-projection`, `erp-worker`)

**`erp_projection::Progress::Advanced` gains `streams`.** While `run_once_in`
applies a batch it already visits every envelope; it now collects
`envelope.stream` into a set, bounded:

```rust
pub enum Progress {
    UpToDate { at: LogPosition },
    Advanced {
        from: LogPosition,
        to: LogPosition,
        events: usize,
        /// The distinct streams this batch touched, or `None` when there were
        /// more than [`TOUCHED_STREAMS_CAP`] (256) — "many; re-check".
        streams: Option<Vec<StreamId>>,
    },
    Busy,
}
```

`Copy` comes off the derive (`Clone` stays); every existing match uses `{ .. }`
or names `events`, so nothing else changes.

**`erp_control::shared::Advanced`**, beside `Invalidate`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advanced {
    pub tenant: TenantId,
    pub group: String,
    pub module: ModuleId,
    pub position: LogPosition,
    /// `None` means many.
    pub streams: Option<Vec<StreamId>>,
}
```

on channel `erp:advanced`, with `Shared::publish_advanced(&Advanced)` and
`Shared::subscribe_advanced() -> PubSub` mirroring the invalidation pair.
`StreamId` gains `Serialize`/`Deserialize` (its two fields already have them).

**`erp_worker::Signals`**, a trait with one method, so the job's publish can
be proven with a recording fake and no Redis:

```rust
#[async_trait]
pub trait Signals: Send + Sync {
    async fn advanced(&self, signal: &Advanced);
}
impl Signals for Shared { /* publish_advanced */ }
```

`ProjectionJob<G>` gains `signals: Option<Arc<dyn Signals>>` and a builder
`.signalling(Arc<dyn Signals>)`. In `tick`, after `tx.commit().await?` in the
`Advanced` arm, it builds the signal from `G::NAME`, `self.module` (every group
has one), `to` and `streams`, and awaits `signals.advanced(&signal)`. A publish
that fails is a `warn!` inside `Shared`'s implementation and nothing else: the
commit happened, a screen misses one tick, the next signal or the next `ready`
catches it up. `module_jobs()` becomes `module_jobs(signals: Option<Arc<dyn
Signals>>)` and both binaries pass `control.shared()` through it.

### 2. The hub (`erp-web`, new `realtime.rs`)

```rust
pub struct Hub {
    staff: Mutex<HashMap<TenantId, broadcast::Sender<Advanced>>>,
    subjects: Mutex<HashMap<(TenantId, StreamId), broadcast::Sender<Advanced>>>,
    caps: Caps,           // staff_per_tenant, public_per_tenant
    lifetime: Duration,   // 10 min; a test shortens it
    keep_alive: Duration, // 15 s
}
```

- `watch_tenant(tenant) -> Result<broadcast::Receiver<Advanced>, Full>` and
  `watch_subject(tenant, stream) -> Result<…, Full>`: create the sender on
  first use, refuse with `Full` when `receiver_count()` is at the cap. A sender
  whose last receiver dropped is removed on the next `publish` that finds
  `receiver_count() == 0`. Capacity 64 per sender: a receiver that falls that
  far behind sees `RecvError::Lagged`, and the stream answers with `reconnect`.
- `publish(&Advanced)`: sends to the tenant's staff sender, then to the subject
  senders: when `streams` is `Some`, one map lookup per named stream; when it
  is `None`, every subject sender of that tenant. Public senders never receive
  through the staff sender and vice versa.
- The hub does not implement `erp_worker::Signals`: `erp-web` must not depend
  on `erp-worker`, and `erp-worker` depends on `erp-api`, so the API's tests
  cannot reach the worker crate either. `Hub::publish(&Advanced)` is the seam;
  the worker's own test proves the job publishes, and the API's end-to-end test
  plays the worker (below).
- `listen_in_background(control: &Arc<ControlPlane>, hub: Arc<Hub>)`: the
  same loop as `apply_invalidations_in_background` — subscribe, forward each
  `Advanced` to `hub.publish`, log and resubscribe after two seconds on loss,
  stop when the control plane is gone.
- `AppState.realtime: Option<Arc<Hub>>`, set by `streaming_through(hub)`.
  `api.rs` builds one when `Shared::from_env` returned a client and starts the
  listener; tests build one and push into it directly.
- Caps are read once in `api.rs` from the two environment variables, parsed
  as `usize`, defaulting as above; a value that does not parse is a startup
  error, not a silent default.

### 3. The routes (`erp-api`, new `realtime.rs`; `deposits.rs`)

**Staff: `GET /v1/events`**, `Allowed<Read>`, every role. In order:

1. `state.realtime` absent → 503 `request.no_realtime`.
2. `hub.watch_tenant(tenant)` → `Full` → 429 `request.too_many_streams`,
   `Retry-After: 30`.
3. `nudge(&state, tenant)` (`request_visit`).
4. Read `checkpoint_of` for every group of every module the tenant has —
   `erp_api::modules::available()` filtered by `db.has_module`, each
   `ModuleSetup.groups` — on one read connection, released before the stream
   starts. A group with no checkpoint row yet is 0. Send `event: ready`,
   `data: {"groups": {"booking": 1234, "sales": 980, …}}`.
5. Loop on the receiver: `Ok(signal)` → if `db.has_module(&signal.module)`
   send `event: advanced`, `data: {"group": "booking", "position": 1235}`,
   else drop it; `Err(Lagged(_))` → send `reconnect` and end (the reconnect's
   `ready` is the fresh snapshot — `TenantDb` is not `Clone`, and no stream
   touches a database; decided while planning); `Err(Closed)` → end.
6. `KeepAlive` comment every 15 seconds; at 10 minutes send `event: reconnect`
   with empty data and end the stream. The receiver drops with the stream.

**Public: `GET /v1/booking/public/reservations/{reservation}/events`**, the
`Public` extractor (which has already charged the public limiter and refused
with 429 if the caller or tenant is over its window). In order:

1. `state.realtime` absent → 503 `request.no_realtime`.
2. Parse `{reservation}` as an `AggregateId` (400 otherwise);
   `hub.watch_subject(tenant, StreamId::new(booking_reservation, id))` →
   `Full` → 429 `request.too_many_streams`, `Retry-After: 30`. No check that
   the reservation exists: the stream for an id nobody booked is silent and
   costs a sender, which the cap bounds. Knowing the id is the capability,
   exactly as for the deposit status.
3. `nudge`.
4. `event: ready`, `data: {"reservation": "<id>", "position": <checkpoint of
   booking>}`.
5. `advanced` with `data: {"position": N}` for each signal received (the hub
   has already filtered by subject); `Lagged` → `reconnect` and end.
6. Same keep-alive and lifetime as staff.

**`GET /v1/booking/public/reservations/{reservation}/deposit` gains
`?consistent_after=`** through the `Consistency` extractor, waiting on the
`booking` group, so the phone's re-fetch after `advanced` cannot read a lagging
row. Additive; the compatibility gate passes.

**Messages** (`erp_web::messages`, English and Arabic):
`request.no_realtime` — "This deployment has no Redis, so nothing can be
watched live. Set REDIS_URL and try again."; `request.too_many_streams` —
"Too many live streams are open for this business on this server. Try again in
a moment."

**OpenAPI**: both operations documented with a `text/event-stream` 200 whose
body is described as text, plus 400/401/403/404/429/503 as applicable; the
public one, `public_reservation_events`, joins the `PUBLIC` list in
`tests/openapi.rs`; the role matrix gains `("event_stream", ALL_ROLES)` and
asserts 217. Handlers have doc comments.

### 4. The client contract (for the React projects)

- Open `GET /v1/events` with the session; keep it open; let `EventSource`
  reconnect on close.
- On `ready`: for each group the screen watches, if the position is greater
  than the last one it re-fetched at, re-fetch through the ordinary API with
  `?consistent_after=<position>`. Remember the position.
- On `advanced` for a watched group: re-fetch with
  `?consistent_after=<position>`. Ignore groups the screen does not watch.
- Never apply a delta; the stream carries none.
- A phone waiting on a deposit opens the reservation's stream and re-fetches
  the deposit status on `ready` and `advanced`, with `consistent_after`.
- Which branch and day a screen re-fetches is the screen's own knowledge. A
  signal says the tenant's `booking` moved, nothing finer.

## Error handling

| failure | where | what happens |
|---|---|---|
| Redis unreachable when publishing | worker | `warn!`; the commit stands; screens catch up on the next signal or `ready` |
| Redis subscription lost | API node | logged, resubscribed after 2 s; open streams stay open and silent meanwhile; their next `ready` (lifetime or reconnect) reconciles |
| No Redis configured | API | 503 on open |
| Tenant or caller over the cap or the public window | API | 429 with `Retry-After` |
| Client falls 64 signals behind | API | `reconnect` and the stream ends; the reconnect's `ready` is the snapshot |
| Client's session expires mid-stream | API | the stream runs to its lifetime; the reconnect re-runs the extractor and is refused then |
| A batch touches more than 256 streams | worker → hub | `streams: None`; every subject stream of the tenant wakes and re-fetches once |

## Tests (each falsified: revert the fix, watch it fail, restore)

`crates/erp-projection/tests/projection.rs`
- `an_advance_names_the_streams_it_touched` — three events on two streams →
  `streams` is those two; 257 distinct streams → `None`.

`crates/erp-control/tests/shared.rs` (Redis, like its neighbours)
- `an_advance_published_is_received_by_a_subscriber` — round trip of
  `Advanced` including `streams`.

`crates/erp-worker/tests/modules.rs`
- `a_projection_that_advances_signals_once_with_the_committed_position` — a
  recording `Signals`; a tick with events publishes one signal at `to` with the
  touched streams; a tick with none publishes nothing.

`crates/erp-web/src/realtime.rs` unit tests
- `a_tenant_signal_reaches_every_staff_watcher_and_no_subject` /
  `a_subject_signal_reaches_only_its_stream_and_many_reaches_all` /
  `the_staff_and_public_caps_are_counted_apart` /
  `a_lagged_watcher_is_told_so`.

`crates/erp-api/tests/http.rs` (a hub injected with `streaming_through`; SSE
frames read from `fixture.raw(...)` body with a timeout)
- `two_screens_and_a_phone_agree_within_a_second_and_nobody_polled` — **the
  exit criterion.** Two staff streams and the phone's reservation stream are
  open; a reservation is made through the public route; the test plays the
  worker exactly as `ProjectionJob::tick` does — `run_once_in` for `booking`
  in a transaction, commit, then `hub.publish` of an `Advanced` built from the
  returned `Progress` (that wiring's own guard is the worker test above); all
  three streams receive `advanced` at the committed position; a
  `consistent_after` read of that day shows the booking, and the phone's
  deposit status answers with it too.
- `ready_names_every_group_the_tenant_may_see_and_nothing_else`.
- `a_signal_for_a_module_the_tenant_lacks_is_not_delivered`.
- `opening_a_stream_asks_for_a_visit` — `next_visit_at` is now.
- `a_phone_hears_only_its_own_reservation` — two reservations, two streams,
  one signal naming one of them.
- `without_redis_nothing_can_be_watched` — 503.
- `the_caps_refuse_the_stream_past_them` — a hub with caps of one; second
  staff open → 429; the public cap counted separately.
- `a_stream_ends_after_its_lifetime_with_reconnect` — a hub with a lifetime of
  one second (the lifetime is a `Hub` field so a test can shorten it).

Gates: `just openapi`, the openapi and compatibility suites, the role matrix
(217), the three source-scan meta-tests, `just prepare` (new sqlx query only if
any; none expected), fmt, clippy `-D warnings`.

## Documentation

- `docs/IMPLEMENTATION.md`: Phase 13a and 13b boxes ticked (the public stream
  under 13b's "a booking made anywhere reaches every screen watching");
  §46 recording what was built, the two-surface decision and why, and the test
  names.
- `docs/RUNNING.md`: "Watching a tenant live" — the two `curl -N` lines and
  what the events look like.
- `docs/ARCHITECTURE.md` §1.3: one sentence — Redis carries the real-time
  signal; `pg_notify` stays refused; a stream holds no database connection.

## Out of scope, by decision

- 13c notifications and 13d conversations (next, in that order).
- `Last-Event-ID` replay; per-branch or per-day filtering server-side.
- Public streams for anything but a reservation (the hub's subject registry is
  ready for them).
- Running public and staff streams on separate nodes (left open; nothing
  prevents it).
