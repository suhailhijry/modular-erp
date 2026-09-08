# Real-Time Signal Stream (Phase 13a + 13b) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every counter screen, and the phone that booked, learns within a second that a projection advanced, without polling: the worker signals after each commit, Redis fans it out, and two SSE routes (staff per tenant, public per reservation) carry `ready`/`advanced`/`reconnect` signals that clients answer by re-fetching with `consistent_after`.

**Architecture:** `erp_projection::Progress::Advanced` learns which streams a batch touched. The worker's `ProjectionJob` publishes an `erp_control::shared::Advanced` on Redis channel `erp:advanced` right after its commit, through a one-method `Signals` trait. Each API node runs one subscriber task feeding `erp_web::realtime::Hub`, which holds independent per-tenant (staff) and per-subject (public) `tokio::sync::broadcast` senders with separate caps. Two handlers in `erp_api::realtime` turn a receiver into an SSE body that holds only ids, never a database connection.

**Tech Stack:** Rust 2024 (1.97), axum 0.8.9 `response::sse`, `tokio::sync::broadcast`, `futures-util`, `redis` pub/sub via the existing `Shared`, sqlx `query!` (no new tables or columns), utoipa.

## Global Constraints

- **Do not commit.** Leave everything in the working tree.
- **Every guard is falsified**: revert the fix, run the test, see it fail, restore, see it pass. Record the line reverted.
- Run targeted suites plus the three source-scan meta-tests (`erp-api::idempotence`, `erp-projection::purity`, `erp-eventlog::write_side an_instant_becomes…`); hand the user `just check` at the end.
- Environment for every cargo command:
  `export SQLX_OFFLINE=false DATABASE_URL="postgres://postgres:postgres@localhost:55432/erp_typecheck" REDIS_URL=redis://127.0.0.1:56379/`
  Postgres and Redis come from `docker compose up -d --wait pg-primary redis`.
- Clippy `-D warnings`; `too_many_lines` at 100 (tests take `#[expect(clippy::too_many_lines, reason = "…")]`); no `unwrap`/`expect` in non-test `erp-api`/`erp-web` code; no minting identity in `http.rs`/`commands.rs`; no `.format("%Y-%m-%d")` on instants.
- Every handler has a doc comment. The role matrix asserts an exact count (216 → **217**). Public routes are listed in `tests/openapi.rs`.
- Numbers from the spec: broadcast capacity **64**; caps **256** staff / **4096** public per tenant per node from `REALTIME_STAFF_STREAMS_PER_TENANT` / `REALTIME_PUBLIC_STREAMS_PER_TENANT`; keep-alive **15 s**; lifetime **10 min**; touched-streams cap **256**; `Retry-After: 30`.
- **One deviation from the spec, decided while planning:** `TenantDb` is not `Clone` (deliberately), so a stream cannot re-read checkpoints after a lag. A lagged receiver gets `event: reconnect` and the stream ends; the client's reconnect delivers the fresh `ready`. Same reconcile, one hop later, and no database access inside any stream. Task 7 records this in the spec.
- **A second deviation, found while building:** nothing in this API sets a `Retry-After` header (the public limiter's 429 puts the wait in its message), so the stream's 429 does the same rather than teaching `Problem` about headers.
- **Found while running the role matrix:** an event stream never ends before its lifetime, so the test fixture's `send` no longer reads the body of a `text/event-stream` response; the status is the answer there, and `open_stream` is for the body.
- Spec: `docs/superpowers/specs/2026-09-08-realtime-signal-stream-design.md`.

---

## File map

| file | responsibility |
|---|---|
| `crates/erp-projection/src/runner.rs` | `Progress::Advanced.streams`, `TOUCHED_STREAMS_CAP` |
| `crates/erp-control/src/shared.rs` | `Advanced`, `publish_advanced`, `subscribe_advanced`, channel `erp:advanced` |
| `crates/erp-worker/src/jobs.rs`, `lib.rs`, `bin/worker.rs` | `Signals` trait, `ProjectionJob::signalling`, publish after commit, wiring |
| `crates/erp-web/src/realtime.rs` (new), `state.rs`, `lib.rs`, `messages.rs`, `Cargo.toml` | `Hub`, `Caps`, `Full`, `listen_in_background`, `AppState.realtime`, two message codes |
| `crates/erp-api/src/realtime.rs` (new), `lib.rs`, `routes.rs`, `deposits.rs`, `bin/api.rs`, `Cargo.toml` | both SSE routes, `consistent_after` on the deposit status, hub wiring and caps |
| tests: `crates/erp-projection/tests/projection.rs`, `crates/erp-control/tests/shared.rs`, `crates/erp-worker/tests/modules.rs`, `crates/erp-web/src/realtime.rs` (unit), `crates/erp-api/tests/{http,openapi}.rs` | guards |
| docs: `docs/IMPLEMENTATION.md`, `docs/RUNNING.md`, `docs/ARCHITECTURE.md`, the spec | §46, the curls, D4, the deviation |

---

### Task 1: A projection advance names the streams it touched

**Files:**
- Modify: `crates/erp-projection/src/runner.rs` (imports line 3; `Progress` ~line 27; `run_once_in` loop ~line 190; the `Ok(Progress::Advanced {…})` ~line 234)
- Test: `crates/erp-projection/tests/projection.rs`

**Interfaces:**
- Produces: `Progress::Advanced { from, to, events, streams: Option<Vec<StreamId>> }` (no longer `Copy`); `pub const TOUCHED_STREAMS_CAP: usize = 256`.

- [x] **Step 1: Write the failing test** in `crates/erp-projection/tests/projection.rs`, after `a_group_advances_and_records_where_it_got_to`:

```rust
/// **An advance says which streams it touched**, bounded. A subject stream on
/// the API wakes a phone only for its own reservation from this list; past the
/// cap it says "many" and every subject re-checks once.
#[tokio::test]
async fn an_advance_names_the_streams_it_touched() {
    let db = fixture().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    post(&mut conn, "cash", 100, 0).await;
    post(&mut conn, "bank", 50, 0).await;
    post(&mut conn, "cash", 100, 1).await;
    drop(conn);

    let projections: Vec<&dyn Projection<Group = Ledger>> = vec![&Balances];
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 100)
        .await
        .expect("runs");
    let Progress::Advanced { streams, .. } = progress else {
        panic!("{progress:?}");
    };
    let named: Vec<String> = streams
        .expect("a bounded batch names its streams")
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    assert_eq!(named, vec!["bank".to_owned(), "cash".to_owned()], "each once, ordered");

    // Past the cap it is "many", which a subscriber treats as "re-check".
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..=erp_projection::TOUCHED_STREAMS_CAP {
        post(&mut conn, &format!("acct-{i:04}"), 1, 0).await;
    }
    drop(conn);
    let progress = run_once::<Ledger>(db.pool(), &projections, &upcasters(), 1_000)
        .await
        .expect("runs");
    assert!(
        matches!(progress, Progress::Advanced { streams: None, .. }),
        "{progress:?}"
    );
}
```

- [x] **Step 2: Run it, expect a compile error** (`no field streams`):

```bash
cargo nextest run -p erp-projection --test projection an_advance_names_the_streams
```

- [x] **Step 3: Implement.** In `runner.rs`: change the import to `use erp_types::{LogPosition, StreamId};`. Replace the `Progress` definition:

```rust
/// How many distinct streams one advance will name before it says "many".
///
/// A subject stream on the API wakes a phone only for its own reservation, and
/// it decides from this list without touching the database. A batch that
/// touches more streams than this — a replay, a bulk import — is not worth
/// carrying on the wire: every subject re-checks once instead.
pub const TOUCHED_STREAMS_CAP: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    UpToDate {
        at: LogPosition,
    },
    Advanced {
        from: LogPosition,
        to: LogPosition,
        events: usize,
        /// The distinct streams this batch touched, in order, or `None` when
        /// there were more than [`TOUCHED_STREAMS_CAP`]: "many; re-check".
        streams: Option<Vec<StreamId>>,
    },
    Busy,
}
```

In `run_once_in`, before the `for envelope in &batch` loop add:

```rust
    let mut touched = std::collections::BTreeSet::new();
    let mut many = false;
```

Inside the loop, right after `to = envelope.position;`:

```rust
        if !many && !touched.contains(&envelope.stream) {
            if touched.len() < TOUCHED_STREAMS_CAP {
                touched.insert(envelope.stream.clone());
            } else {
                many = true;
                touched.clear();
            }
        }
```

And the return:

```rust
    Ok(Progress::Advanced {
        from,
        to,
        events: batch.len(),
        streams: if many {
            None
        } else {
            Some(touched.into_iter().collect())
        },
    })
```

Export the constant from `crates/erp-projection/src/lib.rs` beside `Progress` (find the `pub use runner::{…}` line and add `TOUCHED_STREAMS_CAP`).

- [x] **Step 4: Run** the test and the crate (the other matches use `{ .. }`):

```bash
cargo nextest run -p erp-projection
cargo clippy -p erp-projection -p erp-worker -p erp-api -p erp-demo --all-targets -- -D warnings
```
Expected: pass; clippy clean (nothing relied on `Copy`).

- [x] **Step 5: Falsify.** Replace `touched.insert(envelope.stream.clone());` with nothing (leave the `if`) → the first assertion fails (empty list). Restore. Change `many = true;` to `many = false;` … no: change `if touched.len() < TOUCHED_STREAMS_CAP` to `if true` → the second assertion fails (`Some`). Restore; pass.

---

### Task 2: `Advanced` travels over Redis

**Files:**
- Modify: `crates/erp-control/src/shared.rs` (imports ~line 47; `CHANNEL` ~line 64; `impl Shared` publish/subscribe ~line 292–314)
- Test: `crates/erp-control/tests/shared.rs`

**Interfaces:**
- Produces: `erp_control::shared::Advanced { tenant: TenantId, group: String, module: ModuleId, position: LogPosition, streams: Option<Vec<StreamId>> }`; `Shared::publish_advanced(&self, &Advanced)`; `Shared::subscribe_advanced(&self) -> Result<redis::aio::PubSub, redis::RedisError>`.

- [x] **Step 1: Write the failing test** in `crates/erp-control/tests/shared.rs` (add `use erp_control::shared::Advanced;`, `use erp_types::{AggregateId, DomainName, LogPosition, ModuleId, StreamId, TenantId};`, `use futures_util::StreamExt as _;` to the imports if absent):

```rust
/// **A projection advance published on one node reaches a subscriber on
/// another.** Every open stream in the fleet learns of it from this.
#[tokio::test]
async fn an_advance_published_is_received_by_a_subscriber() {
    let shared = shared().await;
    let mut pubsub = shared.subscribe_advanced().await.expect("subscribes");
    let mut messages = pubsub.on_message();

    let signal = Advanced {
        tenant: TenantId::new(),
        group: "booking".to_owned(),
        module: ModuleId::new("booking").expect("a module"),
        position: LogPosition::new(42).expect("a position"),
        streams: Some(vec![StreamId::new(
            DomainName::new("booking_reservation").expect("a domain"),
            AggregateId::new("r-1").expect("an id"),
        )]),
    };
    shared.publish_advanced(&signal).await;

    let message = tokio::time::timeout(Duration::from_secs(5), messages.next())
        .await
        .expect("arrives in time")
        .expect("a message");
    let raw: String = message.get_payload().expect("a payload");
    assert_eq!(serde_json::from_str::<Advanced>(&raw).expect("parses"), signal);
}
```

- [x] **Step 2: Run, expect a compile error**:

```bash
cargo nextest run -p erp-control --test shared an_advance_published
```

- [x] **Step 3: Implement** in `shared.rs`. Imports: `use erp_types::{IdentityId, LogPosition, ModuleId, StreamId, TenantId};`. After `const CHANNEL: &str = "erp:invalidate";`:

```rust
/// The channel a projection advance is announced on. Its own, so the
/// invalidation consumer never sees a message it has no variant for.
const ADVANCED_CHANNEL: &str = "erp:advanced";

/// **A projection group is queryable through a position.** Published by the
/// worker after the commit that made it true, fanned out to every open stream.
///
/// A signal, not data: the group and where it got to. `streams` names what
/// the batch touched so a subject stream can wake one phone rather than all of
/// them; `None` means more than the runner's cap, and every subject re-checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Advanced {
    pub tenant: TenantId,
    pub group: String,
    pub module: ModuleId,
    pub position: LogPosition,
    #[serde(default)]
    pub streams: Option<Vec<StreamId>>,
}
```

In `impl Shared`, after `subscribe`:

```rust
    /// Announces an advance. A failure is a warning: the commit stands, and an
    /// open stream catches up on its next `ready`.
    pub async fn publish_advanced(&self, what: &Advanced) {
        let Ok(encoded) = serde_json::to_string(what) else {
            return;
        };
        let mut conn = self.conn.clone();
        if let Err(e) = conn.publish::<_, _, ()>(ADVANCED_CHANNEL, encoded).await {
            tracing::warn!(
                error = %e, group = %what.group, tenant = %what.tenant,
                "could not announce a projection advance; open streams catch up on their next ready"
            );
        }
    }

    /// Every advance announced from now on. A dedicated connection, as
    /// [`Self::subscribe`] is.
    pub async fn subscribe_advanced(&self) -> Result<redis::aio::PubSub, redis::RedisError> {
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(ADVANCED_CHANNEL).await?;
        Ok(pubsub)
    }
```

- [x] **Step 4: Run → PASS**; clippy `-p erp-control`.

- [x] **Step 5: Falsify.** In `subscribe_advanced`, subscribe to `CHANNEL` instead of `ADVANCED_CHANNEL` → the test times out ("arrives in time" fails). Restore; pass.

---

### Task 3: The worker signals once per tick, after the commit

**Files:**
- Modify: `crates/erp-worker/src/jobs.rs` (imports, `ProjectionJob` struct/new/builders ~lines 1–66, `tick` ~lines 77–105)
- Modify: `crates/erp-worker/src/lib.rs:37`
- Modify: `crates/erp-worker/src/bin/worker.rs` (`module_jobs` ~line 1490 and its three call sites at ~133, ~1747, ~1775; wiring near line 48)
- Test: `crates/erp-worker/tests/modules.rs`

**Interfaces:**
- Consumes: `Progress::Advanced { to, streams, .. }` (Task 1); `erp_control::shared::{Advanced, Shared}` (Task 2).
- Produces: `pub trait erp_worker::Signals { async fn advanced(&self, signal: &Advanced); }` (object-safe, `Send + Sync + Debug`), `impl Signals for Shared`; `ProjectionJob::signalling(self, Option<Arc<dyn Signals>>) -> Self`; `fn module_jobs(signals: Option<Arc<dyn erp_worker::Signals>>)`.

- [x] **Step 1: Write the failing test** at the end of `crates/erp-worker/tests/modules.rs`:

```rust
// ---------------------------------------------------------------------------
// The signal after the commit
// ---------------------------------------------------------------------------

struct Tiny;
impl erp_projection::ProjectionGroup for Tiny {
    const NAME: &'static str = "tiny";
    const SCHEMA: &'static str = "proj_tiny";
}

/// Applies nothing. The signal is about the commit, not the tables.
struct Noop;
#[async_trait::async_trait]
impl erp_projection::Projection for Noop {
    type Group = Tiny;
    fn name(&self) -> &'static str {
        "noop"
    }
    async fn apply(
        &self,
        _ctx: &erp_projection::ProjectionCtx<'_>,
        _envelope: &erp_eventlog::Envelope,
        _conn: &mut sqlx::PgConnection,
    ) -> Result<(), erp_projection::ProjectionError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Recorder(std::sync::Mutex<Vec<erp_control::shared::Advanced>>);
#[async_trait::async_trait]
impl erp_worker::Signals for Recorder {
    async fn advanced(&self, signal: &erp_control::shared::Advanced) {
        self.0.lock().expect("not poisoned").push(signal.clone());
    }
}

fn tiny_event() -> erp_eventlog::NewEvent {
    erp_eventlog::NewEvent::new(
        erp_types::EventName::new("tiny.happened").expect("a name"),
        erp_types::SchemaVersion::new(1).expect("a version"),
        serde_json::json!({}),
    )
}

/// **A projection that advances signals once, with the committed position and
/// the streams it touched; one that is up to date signals nothing.** The
/// signal is what every open stream in the fleet is waiting for.
#[tokio::test]
async fn a_projection_that_advances_signals_once_with_the_committed_position() {
    let mut fixture = Fixture::new().await;
    let tenant = fixture.tenant("tiny").await;
    let db = fixture.db(tenant).await;

    let mut conn = db.acquire().await.expect("connection");
    erp_projection::ensure_group_schema::<Tiny>(&mut conn)
        .await
        .expect("schema");
    for (stream, sequence) in [("a", 0), ("b", 0), ("a", 1)] {
        erp_eventlog::append(
            &mut conn,
            &erp_types::StreamId::new(
                erp_types::DomainName::new("tiny").expect("a domain"),
                erp_types::AggregateId::new(stream).expect("an id"),
            ),
            erp_types::Sequence::new(sequence).expect("a sequence"),
            &[tiny_event()],
            &erp_eventlog::Metadata::default(),
        )
        .await
        .expect("appends");
    }
    drop(conn);

    let recorder = Arc::new(Recorder::default());
    let upcasters = erp_eventlog::Upcasters::new().declare(
        &erp_types::EventName::new("tiny.happened").expect("a name"),
        erp_types::SchemaVersion::new(1).expect("a version"),
    );
    let job = erp_worker::ProjectionJob::<Tiny>::new(vec![Arc::new(Noop)], Arc::new(upcasters), 100)
        .for_module(module("tiny"))
        .signalling(Some(Arc::clone(&recorder) as Arc<dyn erp_worker::Signals>));

    assert_eq!(job.tick(&db).await.expect("ticks"), Activity::Worked);
    let signals = recorder.0.lock().expect("not poisoned").clone();
    assert_eq!(signals.len(), 1, "{signals:?}");
    assert_eq!(signals[0].tenant, tenant);
    assert_eq!(signals[0].group, "tiny");
    assert_eq!(signals[0].module, module("tiny"));
    assert_eq!(signals[0].position.get(), 3, "the committed position");
    let named: Vec<&str> = signals[0]
        .streams
        .as_ref()
        .expect("named")
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(named, vec!["a", "b"]);

    // Up to date: nothing committed, nothing announced.
    assert_eq!(job.tick(&db).await.expect("ticks"), Activity::Idle);
    assert_eq!(recorder.0.lock().expect("not poisoned").len(), 1);

    drop(db);
    fixture.cleanup().await;
}
```

(`Fixture`, `module`, `Activity`, `Arc` and the tenant helpers already exist in that file; `erp_eventlog`, `erp_projection`, `erp_types`, `sqlx`, `serde_json`, `async_trait` are dependencies of `erp-worker`.)

- [x] **Step 2: Run, expect a compile error** (`no method signalling`, `Signals` missing):

```bash
cargo nextest run -p erp-worker --test modules a_projection_that_advances_signals_once
```

- [x] **Step 3: Implement** in `jobs.rs`. Imports: add `use erp_control::shared::{Advanced, Shared};`. Before `pub struct ProjectionJob`:

```rust
/// **Who is told that a projection advanced.** One method, so the job can be
/// proven with a recording fake and the real one is Redis.
///
/// The worker announces after the commit that made the read model queryable
/// through the position — never on append, which is before. Every open stream
/// on every API node is waiting for exactly this.
#[async_trait::async_trait]
pub trait Signals: Send + Sync + std::fmt::Debug {
    async fn advanced(&self, signal: &Advanced);
}

#[async_trait::async_trait]
impl Signals for Shared {
    async fn advanced(&self, signal: &Advanced) {
        self.publish_advanced(signal).await;
    }
}
```

`ProjectionJob` gains a field `signals: Option<Arc<dyn Signals>>,` (init `signals: None,` in `new`) and a builder after `for_module`:

```rust
    /// The same job, announcing each advance. `None` — a deployment without
    /// Redis — announces nothing, and the streams that would listen refuse to
    /// open (L6) rather than sit silent.
    #[must_use]
    pub fn signalling(mut self, signals: Option<Arc<dyn Signals>>) -> Self {
        self.signals = signals;
        self
    }
```

In `tick`, replace the `Progress::Advanced { .. } => { tx.commit().await?; Ok(Activity::Worked) }` arm with:

```rust
            Progress::Advanced { to, streams, .. } => {
                tx.commit().await?;
                // **After the commit, never before.** This is the moment the
                // guarantee "queryable through `to`" becomes true; a signal on
                // the append would send a screen to read a row not yet there.
                if let (Some(signals), Some(module)) = (&self.signals, &self.module) {
                    signals
                        .advanced(&Advanced {
                            tenant: db.tenant(),
                            group: G::NAME.to_owned(),
                            module: module.clone(),
                            position: to,
                            streams,
                        })
                        .await;
                }
                Ok(Activity::Worked)
            }
```

(`match progress` now moves `streams` out; `progress` is no longer used after, which is fine.) `lib.rs:37` becomes `pub use jobs::{OutboxJob, PlatformOutboxJob, ProjectionJob, Signals};`.

- [x] **Step 4: Wire the binary.** In `bin/worker.rs`, after the `let control = Arc::new(control);` near line 50 add:

```rust
    // **Every projection advance is announced**, so a screen learns of it
    // without polling. Without Redis there is nobody to tell.
    let signals: Option<Arc<dyn erp_worker::Signals>> = control
        .shared()
        .map(|shared| Arc::new(shared.clone()) as Arc<dyn erp_worker::Signals>);
```

Change `fn module_jobs() -> Vec<…>` to `fn module_jobs(signals: Option<Arc<dyn erp_worker::Signals>>) -> Vec<…>` and, only inside that function's `vec![…]`, append `.signalling(signals.clone())` after every `.for_module(…::module_id())`:

```bash
python3 - <<'EOF'
p='crates/erp-worker/src/bin/worker.rs'; s=open(p).read()
start=s.index('fn module_jobs(signals: Option<Arc<dyn erp_worker::Signals>>) -> Vec<Arc<dyn erp_worker::Job>> {')
end=s.index('\n}\n', start)
body=s[start:end]
import re
n=len(re.findall(r'\.for_module\(([a-z_]+)::module_id\(\)\)', body))
body=re.sub(r'\.for_module\(([a-z_]+)::module_id\(\)\)', r'.for_module(\1::module_id())\n            .signalling(signals.clone())', body)
s=s[:start]+body+s[end:]
open(p,'w').write(s); print("signalling added to", n, "jobs")
EOF
```
Expected: `signalling added to 14 jobs`. Then the call sites: line ~133 `for job in module_jobs()` → `for job in module_jobs(signals.clone())`; the two in `mod tests` (~1747, ~1775) → `module_jobs(None)`.

- [x] **Step 5: Run**

```bash
cargo clippy -p erp-worker --all-targets -- -D warnings
cargo nextest run -p erp-worker
```
Expected: clean; the new test and the existing worker tests pass (`every_module_has_a_projection_job` still counts fourteen).

- [x] **Step 6: Falsify.** Move the `signals.advanced(…)` block **above** `tx.commit().await?` → the test still passes (it cannot see ordering) — so falsify what it can see: delete the whole `if let (Some(signals), …)` block → `signals.len() == 1` fails. Restore. Change `position: to` to `position: LogPosition::ZERO` (import it) → "the committed position" fails. Restore; pass. The ordering is held by review: the comment above the block says why.

---

### Task 4: The hub on every API node

**Files:**
- Create: `crates/erp-web/src/realtime.rs`
- Modify: `crates/erp-web/src/lib.rs` (module list ~line 40; re-exports ~line 48), `crates/erp-web/src/state.rs` (field after `storage`; `on` initialiser; builder after `sealing_with`), `crates/erp-web/src/messages.rs` (consts ~line 35, `CODES` ~line 106, `ENTRIES` before the second `];`), `crates/erp-web/Cargo.toml` (add `futures-util = { workspace = true }` under `[dependencies]`)

**Interfaces:**
- Consumes: `erp_control::shared::{Advanced, Shared}`, `ControlPlane::shared()`.
- Produces: `erp_web::realtime::{Hub, Caps, Full, listen_in_background, CAPACITY}`; `Hub::new(Caps)`, `Hub::living(Duration)`, `Hub::lifetime()`, `Hub::keep_alive()`, `Hub::watch_tenant(TenantId) -> Result<broadcast::Receiver<Advanced>, Full>`, `Hub::watch_subject(TenantId, StreamId) -> Result<…, Full>`, `Hub::publish(&Advanced)`, `Hub::open(TenantId) -> (usize, usize)`; `AppState.realtime: Option<Arc<Hub>>`, `AppState::streaming_through(Arc<Hub>)`; message codes `erp_web::messages::{NO_REALTIME, TOO_MANY_STREAMS}`.

- [x] **Step 1: Write the file with its failing unit tests.** `crates/erp-web/src/realtime.rs`:

```rust
//! Where an open stream waits, and how a projection advance reaches it.
//!
//! # Two registries, on purpose
//!
//! A counter screen watches a tenant; a customer's phone watches one
//! reservation. They are different populations — dozens against thousands —
//! and the spec's one hard requirement is that neither can starve the other.
//! So they share nothing but the subscriber task: separate senders, separate
//! caps, and a public signal is never routed through a staff sender.
//!
//! # No database connection per stream
//!
//! Fan-out is one Redis message per advance and one `broadcast` send per
//! sender. A stream holds a receiver and a deadline, nothing else. The budget
//! in `pools.rs` is sized for tenants, not for browser tabs.
//!
//! # A lagged watcher is told to reconnect
//!
//! `broadcast` keeps [`CAPACITY`] messages per sender; a receiver further
//! behind than that gets `Lagged`. The right answer is a fresh snapshot, and the
//! cheapest way to one that touches no database from inside a stream is to end
//! the stream: the browser reconnects, and the first event of every stream is
//! the snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use erp_control::ControlPlane;
use erp_control::shared::Advanced;
use erp_types::{StreamId, TenantId};
use tokio::sync::broadcast;

/// Signals buffered per sender before a slow receiver is told it lagged.
pub const CAPACITY: usize = 64;

/// How many streams one tenant may hold open on one node, per surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub staff_per_tenant: usize,
    pub public_per_tenant: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            staff_per_tenant: 256,
            public_per_tenant: 4096,
        }
    }
}

/// The cap is reached. A 429, with a moment to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("too many live streams are open for this tenant on this node")]
pub struct Full;

#[derive(Debug)]
pub struct Hub {
    staff: Mutex<HashMap<TenantId, broadcast::Sender<Advanced>>>,
    subjects: Mutex<HashMap<(TenantId, StreamId), broadcast::Sender<Advanced>>>,
    caps: Caps,
    lifetime: Duration,
    keep_alive: Duration,
}

impl Hub {
    #[must_use]
    pub fn new(caps: Caps) -> Self {
        Self {
            staff: Mutex::new(HashMap::new()),
            subjects: Mutex::new(HashMap::new()),
            caps,
            lifetime: Duration::from_secs(10 * 60),
            keep_alive: Duration::from_secs(15),
        }
    }

    /// The same hub with streams that end sooner. A test's.
    #[must_use]
    pub const fn living(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// How long a stream lives before it asks the client to reconnect. The
    /// reconnect re-runs authorization, which a stream held for hours would
    /// otherwise outlive.
    #[must_use]
    pub const fn lifetime(&self) -> Duration {
        self.lifetime
    }

    #[must_use]
    pub const fn keep_alive(&self) -> Duration {
        self.keep_alive
    }

    /// A receiver for everything this tenant's staff may see.
    pub fn watch_tenant(&self, tenant: TenantId) -> Result<broadcast::Receiver<Advanced>, Full> {
        let mut staff = self.staff.lock().unwrap_or_else(PoisonError::into_inner);
        watch(&mut staff, tenant, self.caps.staff_per_tenant)
    }

    /// A receiver for one subject — a reservation — and nothing else.
    pub fn watch_subject(
        &self,
        tenant: TenantId,
        stream: StreamId,
    ) -> Result<broadcast::Receiver<Advanced>, Full> {
        let mut subjects = self.subjects.lock().unwrap_or_else(PoisonError::into_inner);
        watch(&mut subjects, (tenant, stream), self.caps.public_per_tenant)
    }

    /// Hands an advance to every stream it concerns, and forgets senders
    /// nobody is holding any more.
    pub fn publish(&self, signal: &Advanced) {
        {
            let mut staff = self.staff.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(sender) = staff.get(&signal.tenant) {
                if sender.receiver_count() == 0 {
                    staff.remove(&signal.tenant);
                } else {
                    // A send fails only when every receiver is gone, which the
                    // count above just said is not so.
                    let _ = sender.send(signal.clone());
                }
            }
        }

        let mut subjects = self.subjects.lock().unwrap_or_else(PoisonError::into_inner);
        match &signal.streams {
            Some(streams) => {
                for stream in streams {
                    let key = (signal.tenant, stream.clone());
                    if let Some(sender) = subjects.get(&key) {
                        if sender.receiver_count() == 0 {
                            subjects.remove(&key);
                        } else {
                            let _ = sender.send(signal.clone());
                        }
                    }
                }
            }
            // "Many": every subject of this tenant re-checks once.
            None => {
                subjects.retain(|(tenant, _), sender| {
                    if *tenant != signal.tenant {
                        return true;
                    }
                    if sender.receiver_count() == 0 {
                        return false;
                    }
                    let _ = sender.send(signal.clone());
                    true
                });
            }
        }
    }

    /// How many streams this tenant holds open here: `(staff, public)`.
    #[must_use]
    pub fn open(&self, tenant: TenantId) -> (usize, usize) {
        let staff = self
            .staff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tenant)
            .map_or(0, broadcast::Sender::receiver_count);
        let public = self
            .subjects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((t, _), _)| *t == tenant)
            .map(|(_, sender)| sender.receiver_count())
            .sum();
        (staff, public)
    }
}

fn watch<K: std::hash::Hash + Eq>(
    senders: &mut HashMap<K, broadcast::Sender<Advanced>>,
    key: K,
    cap: usize,
) -> Result<broadcast::Receiver<Advanced>, Full> {
    match senders.get(&key) {
        Some(sender) if sender.receiver_count() >= cap => Err(Full),
        Some(sender) => Ok(sender.subscribe()),
        None => {
            if cap == 0 {
                return Err(Full);
            }
            let (sender, receiver) = broadcast::channel(CAPACITY);
            senders.insert(key, sender);
            Ok(receiver)
        }
    }
}

/// Forwards every advance announced in the fleet to this node's hub, for as
/// long as the control plane lives. Resubscribes when Redis goes away and
/// comes back, as the invalidation listener does.
pub fn listen_in_background(
    control: &Arc<ControlPlane>,
    hub: Arc<Hub>,
) -> Option<tokio::task::JoinHandle<()>> {
    let shared = control.shared()?.clone();
    let weak = Arc::downgrade(control);

    Some(tokio::spawn(async move {
        use futures_util::StreamExt as _;

        loop {
            if weak.upgrade().is_none() {
                return;
            }
            match shared.subscribe_advanced().await {
                Ok(mut pubsub) => {
                    tracing::info!("listening for projection advances");
                    let mut stream = pubsub.on_message();
                    while let Some(message) = stream.next().await {
                        if weak.upgrade().is_none() {
                            return;
                        }
                        match message.get_payload::<String>() {
                            Ok(raw) => match serde_json::from_str::<Advanced>(&raw) {
                                Ok(signal) => hub.publish(&signal),
                                Err(e) => tracing::error!(
                                    error = %e, %raw,
                                    "unreadable advance; a newer build may be announcing what this one cannot read"
                                ),
                            },
                            Err(e) => tracing::warn!(error = %e, "unreadable advance payload"),
                        }
                    }
                    tracing::warn!("advance subscription ended; resubscribing");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not subscribe to advances; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::{AggregateId, DomainName, LogPosition, ModuleId};

    fn reservation(id: &str) -> StreamId {
        StreamId::new(
            DomainName::new("booking_reservation").expect("a domain"),
            AggregateId::new(id).expect("an id"),
        )
    }

    fn advance(tenant: TenantId, streams: Option<Vec<StreamId>>) -> Advanced {
        Advanced {
            tenant,
            group: "booking".to_owned(),
            module: ModuleId::new("booking").expect("a module"),
            position: LogPosition::new(7).expect("a position"),
            streams,
        }
    }

    /// **A tenant's advance reaches every staff watcher and no subject**, and
    /// another tenant's watchers hear nothing.
    #[tokio::test]
    async fn a_tenant_signal_reaches_every_staff_watcher_and_no_subject() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let other = TenantId::new();
        let mut first = hub.watch_tenant(acme).expect("opens");
        let mut second = hub.watch_tenant(acme).expect("opens");
        let mut elsewhere = hub.watch_tenant(other).expect("opens");
        let mut phone = hub.watch_subject(acme, reservation("r-1")).expect("opens");

        hub.publish(&advance(acme, Some(vec![reservation("r-9")])));

        assert_eq!(first.try_recv().expect("heard").position.get(), 7);
        assert_eq!(second.try_recv().expect("heard").position.get(), 7);
        assert!(elsewhere.try_recv().is_err(), "another tenant heard it");
        assert!(phone.try_recv().is_err(), "a subject heard a stream not its own");
    }

    /// **A subject hears its own stream, and "many" wakes every subject.**
    #[tokio::test]
    async fn a_subject_signal_reaches_only_its_stream_and_many_reaches_all() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let mut one = hub.watch_subject(acme, reservation("r-1")).expect("opens");
        let mut two = hub.watch_subject(acme, reservation("r-2")).expect("opens");

        hub.publish(&advance(acme, Some(vec![reservation("r-1")])));
        assert!(one.try_recv().is_ok());
        assert!(two.try_recv().is_err(), "the wrong phone woke");

        hub.publish(&advance(acme, None));
        assert!(one.try_recv().is_ok());
        assert!(two.try_recv().is_ok(), "many did not wake every subject");
    }

    /// **The caps are counted apart**, which is the whole reason there are two
    /// registries: a tenant at its public cap still opens a staff stream.
    #[tokio::test]
    async fn the_staff_and_public_caps_are_counted_apart() {
        let hub = Hub::new(Caps {
            staff_per_tenant: 1,
            public_per_tenant: 1,
        });
        let acme = TenantId::new();
        let _phone = hub.watch_subject(acme, reservation("r-1")).expect("opens");
        assert_eq!(hub.watch_subject(acme, reservation("r-1")).err(), Some(Full));
        let _screen = hub.watch_tenant(acme).expect("the public cap is not the staff cap");
        assert_eq!(hub.watch_tenant(acme).err(), Some(Full));
        assert_eq!(hub.open(acme), (1, 1));
    }

    /// **A watcher further behind than the buffer is told so**, which the
    /// stream turns into a reconnect.
    #[tokio::test]
    async fn a_lagged_watcher_is_told_so() {
        let hub = Hub::new(Caps::default());
        let acme = TenantId::new();
        let mut slow = hub.watch_tenant(acme).expect("opens");
        for _ in 0..=CAPACITY {
            hub.publish(&advance(acme, None));
        }
        assert!(matches!(
            slow.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }
}
```

- [x] **Step 2: Wire the crate.** `lib.rs`: add `pub mod realtime;` after `pub mod rate;`. `state.rs`: add the field after `storage`:

```rust
    /// Where open streams wait for a projection advance.
    ///
    /// `None` when the deployment has no Redis, and then the stream routes
    /// **refuse** rather than open a stream nothing would ever write to (L6).
    pub realtime: Option<Arc<crate::realtime::Hub>>,
```
initialise `realtime: None,` in `on`, and add after `sealing_with`:

```rust
    /// The same state, able to keep streams open.
    #[must_use]
    pub fn streaming_through(mut self, hub: Arc<crate::realtime::Hub>) -> Self {
        self.realtime = Some(hub);
        self
    }
```

`messages.rs`: after `NO_SEALING_KEY`:

```rust
pub const NO_REALTIME: MessageCode = MessageCode::new("request.no_realtime");
pub const TOO_MANY_STREAMS: MessageCode = MessageCode::new("request.too_many_streams");
```
add both to `CODES` after `NO_SEALING_KEY,`, and to `ENTRIES` after the `NO_SEALING_KEY` Arabic entry:

```rust
    (
        NO_REALTIME,
        Locale::English,
        Template::Simple(
            "This deployment has no Redis, so nothing can be watched live. Set REDIS_URL and try again.",
        ),
    ),
    (
        NO_REALTIME,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد Redis مُهيّأ في هذا النظام، فلا يمكن متابعة أي شيء مباشرةً. اضبط REDIS_URL ثم أعد المحاولة.",
        ),
    ),
    (
        TOO_MANY_STREAMS,
        Locale::English,
        Template::Simple(
            "Too many live streams are open for this business on this server. Try again in a moment.",
        ),
    ),
    (
        TOO_MANY_STREAMS,
        Locale::Arabic,
        Template::Simple(
            "عدد البثوث المباشرة المفتوحة لهذه المنشأة على هذا الخادم كبير جدًا. أعد المحاولة بعد لحظات.",
        ),
    ),
```
`Cargo.toml`: `futures-util = { workspace = true }` under `[dependencies]`.

- [x] **Step 3: Run**

```bash
cargo clippy -p erp-web --all-targets -- -D warnings
cargo nextest run -p erp-web --lib realtime
```
Expected: clean; four tests pass.

- [x] **Step 4: Falsify.** In `publish`, delete the `None => { subjects.retain(…) }` body (make it `None => {}`) → `a_subject_signal_reaches_only_its_stream_and_many_reaches_all` fails. Restore. In `watch`, change `sender.receiver_count() >= cap` to `> cap` → `the_staff_and_public_caps_are_counted_apart` fails. Restore; pass.

---

### Task 5: The staff stream, `GET /v1/events`

**Files:**
- Create: `crates/erp-api/src/realtime.rs`
- Modify: `crates/erp-api/src/lib.rs` (add `mod realtime;`), `crates/erp-api/src/routes.rs` (`.merge(crate::realtime::routes())` after `.merge(crate::calendar::routes())`), `crates/erp-api/Cargo.toml` (`[dependencies]`: add `erp-projection = { workspace = true }` and `futures-util = { workspace = true }`; `[dev-dependencies]`: add `futures-util = { workspace = true }`), `crates/erp-api/src/bin/api.rs` (hub, caps, listener)
- Test: `crates/erp-api/tests/http.rs` (fixture, helpers, role matrix, six tests)

**Interfaces:**
- Consumes: `Hub`, `Caps`, `Full`, `listen_in_background`, `AppState::streaming_through`, `messages::{NO_REALTIME, TOO_MANY_STREAMS}` (Task 4); `erp_projection::checkpoint_of`; `crate::modules::available()`.
- Produces: handler `event_stream` at `GET /v1/events`; `pub(crate) fn live(hub: &Hub, ready: Event, receiver, watch: Watch) -> Live` and `pub(crate) enum Watch { Staff { modules: Vec<ModuleId> }, Subject }` reused by Task 6; `pub(crate) fn full(locale) -> Problem`, `pub(crate) fn no_realtime(locale) -> Problem`.

- [x] **Step 1: Fixture and helpers** in `crates/erp-api/tests/http.rs`. Add `use std::time::Duration;` and `use futures_util::StreamExt as _;` to the imports. Give `Fixture` a field `hub: Arc<erp_web::realtime::Hub>,` and split `new`:

```rust
    async fn new() -> Self {
        Self::with_hub(Arc::new(erp_web::realtime::Hub::new(
            erp_web::realtime::Caps::default(),
        )))
        .await
    }

    async fn with_hub(hub: Arc<erp_web::realtime::Hub>) -> Self {
```
(the existing body follows; where `AppState::new(Arc::clone(&control))` is built, chain `.streaming_through(Arc::clone(&hub))` after `.storing_in(…)`, and put `hub,` in the `Self { … }` literal). Add helpers to `impl Fixture`:

```rust
    /// Opens a stream and returns its status and body, unread.
    async fn open_stream(&self, request: Request<Body>) -> (StatusCode, axum::body::BodyDataStream) {
        let response = self.raw(request).await;
        (response.status(), response.into_body().into_data_stream())
    }
```
and two free functions:

```rust
/// The next SSE event on a body: `(event, data)`. Comments (keep-alives) are
/// skipped. `None` when nothing arrives in time or the body ends.
async fn next_event(
    body: &mut axum::body::BodyDataStream,
    buffer: &mut String,
    wait: Duration,
) -> Option<(String, String)> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = rest.trim().to_owned();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data = rest.trim().to_owned();
                }
            }
            if event.is_empty() && data.is_empty() {
                continue; // a comment
            }
            return Some((event, data));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, body.next()).await {
            Ok(Some(Ok(bytes))) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
            _ => return None,
        }
    }
}

fn advanced(tenant: TenantId, group: &str, module: &str, position: i64, streams: Option<Vec<erp_types::StreamId>>) -> erp_control::shared::Advanced {
    erp_control::shared::Advanced {
        tenant,
        group: group.to_owned(),
        module: erp_types::ModuleId::new(module).expect("a module"),
        position: erp_types::LogPosition::new(position).expect("a position"),
        streams,
    }
}
```

- [x] **Step 2: Write the failing tests** (after the ZATCA tests is fine):

```rust
/// **`ready` names every group the tenant may see, at its checkpoint, and
/// nothing else.** The snapshot a screen reconciles against.
#[tokio::test]
async fn ready_names_every_group_the_tenant_may_see_and_nothing_else() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (status, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut buffer = String::new();
    let (event, data) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("a first event");
    assert_eq!(event, "ready");
    let ready: serde_json::Value = serde_json::from_str(&data).expect("json");
    let groups = ready["groups"].as_object().expect("groups");
    assert!(groups.contains_key("sales"), "{ready}");
    assert!(groups.contains_key("ledger"), "{ready}");
    assert!(groups.contains_key("tax_sa"), "{ready}");
    assert!(!groups.contains_key("booking"), "a module the tenant lacks: {ready}");
    assert!(groups["sales"].is_i64());

    fixture.cleanup().await;
}

/// **A signal for a module the tenant lacks is not delivered**, and one for a
/// module it has is — with the group and the position.
#[tokio::test]
async fn a_signal_for_a_module_the_tenant_lacks_is_not_delivered() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_selling_only(tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (_, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let mut buffer = String::new();
    next_event(&mut body, &mut buffer, Duration::from_secs(2)).await.expect("ready");

    fixture.hub.publish(&advanced(tenant, "booking", "booking", 5, None));
    fixture.hub.publish(&advanced(tenant, "sales", "sales", 6, None));
    let (event, data) = next_event(&mut body, &mut buffer, Duration::from_secs(2))
        .await
        .expect("the sales signal");
    assert_eq!(event, "advanced");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&data).expect("json"),
        serde_json::json!({ "group": "sales", "position": 6 }),
        "the booking signal was delivered to a tenant without booking"
    );

    fixture.cleanup().await;
}

/// **Opening a stream asks for a visit.** A dormant tenant's worker has
/// backed off for hours; a screen that just opened should not wait for it.
#[tokio::test]
async fn opening_a_stream_asks_for_a_visit() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    sqlx::query("UPDATE tenant SET next_visit_at = now() + interval '1 hour' WHERE id = $1")
        .bind(tenant.as_uuid())
        .execute(fixture.db.pool())
        .await
        .expect("dormant");

    let (status, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut buffer = String::new();
    next_event(&mut body, &mut buffer, Duration::from_secs(2)).await.expect("ready");

    let due: bool =
        sqlx::query_scalar("SELECT next_visit_at <= now() FROM tenant WHERE id = $1")
            .bind(tenant.as_uuid())
            .fetch_one(fixture.db.pool())
            .await
            .expect("reads");
    assert!(due, "the stream did not ask for a visit");

    fixture.cleanup().await;
}

/// **Without Redis nothing can be watched**, and the route says so rather
/// than opening a stream nothing would ever write to (L6).
#[tokio::test]
async fn without_redis_nothing_can_be_watched() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    // The same control plane, a state with no hub: the deployment without Redis.
    let bare = router(AppState::new(Arc::clone(&fixture.control)).trusting_forwarded_for(true));
    let response = bare
        .oneshot(
            Request::get("/v1/events")
                .header(header::HOST, "acme.localhost")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("responds");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 16).await.expect("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "request.no_realtime");

    fixture.cleanup().await;
}

/// **The caps refuse the stream past them**, staff and public counted apart.
#[tokio::test]
async fn the_caps_refuse_the_stream_past_them() {
    let hub = Arc::new(erp_web::realtime::Hub::new(erp_web::realtime::Caps {
        staff_per_tenant: 1,
        public_per_tenant: 1,
    }));
    let mut fixture = Fixture::with_hub(Arc::clone(&hub)).await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let open = || {
        Request::get("/v1/events")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };

    let (status, _first) = fixture.open_stream(open()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = fixture.send(open()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "request.too_many_streams");
    assert_eq!(hub.open(tenant), (1, 0));

    fixture.cleanup().await;
}

/// **A stream ends after its lifetime with `reconnect`**, so authorization is
/// re-run by the reconnect rather than outlived by the stream.
#[tokio::test]
async fn a_stream_ends_after_its_lifetime_with_reconnect() {
    let hub = Arc::new(
        erp_web::realtime::Hub::new(erp_web::realtime::Caps::default())
            .living(Duration::from_millis(300)),
    );
    let mut fixture = Fixture::with_hub(hub).await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;

    let (_, mut body) = fixture
        .open_stream(
            Request::get("/v1/events")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let mut buffer = String::new();
    let (event, _) = next_event(&mut body, &mut buffer, Duration::from_secs(2)).await.expect("ready");
    assert_eq!(event, "ready");
    let (event, _) = next_event(&mut body, &mut buffer, Duration::from_secs(2)).await.expect("reconnect");
    assert_eq!(event, "reconnect");
    assert!(
        next_event(&mut body, &mut buffer, Duration::from_millis(500)).await.is_none(),
        "the stream did not end"
    );

    fixture.cleanup().await;
}
```
Also add `("event_stream", ALL_ROLES),` next to `("onboarding_status", ALL_ROLES),` in the role matrix and change the count to `217` / `"expected two hundred and seventeen role-scoped operations"`.

- [x] **Step 3: Run, expect compile errors** (`open_stream`, the route):

```bash
cargo nextest run -p erp-api --test http ready_names_every_group
```

- [x] **Step 4: Write `crates/erp-api/src/realtime.rs`:**

```rust
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
use axum::http::{HeaderValue, StatusCode};
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

use crate::catalog::CATALOG;

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

/// The `ready` event first, then every signal the watch wants, then
/// `reconnect` at the hub's lifetime or when the watcher lagged.
pub(crate) fn live(
    hub: &Hub,
    ready: Event,
    receiver: broadcast::Receiver<Advanced>,
    watch: Watch,
) -> Live {
    struct Open {
        receiver: broadcast::Receiver<Advanced>,
        deadline: tokio::time::Instant,
        watch: Watch,
        done: bool,
    }

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
                received = open.receiver.recv() => match received {
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
    Sse::new(stream).keep_alive(KeepAlive::new().interval(hub.keep_alive()).text("keep-alive"))
}

pub(crate) fn no_realtime(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        &erp_i18n::Message::new(erp_web::messages::NO_REALTIME),
        locale,
        &CATALOG,
    )
}

pub(crate) fn full(locale: Locale) -> Problem {
    Problem::new(
        StatusCode::TOO_MANY_REQUESTS,
        &erp_i18n::Message::new(erp_web::messages::TOO_MANY_STREAMS),
        locale,
        &CATALOG,
    )
    .with_header(axum::http::header::RETRY_AFTER, HeaderValue::from_static("30"))
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
    tag = "platform",
    params(("Host" = String, Header, description = "The tenant's subdomain — `bassat.erp.com`. Every path below is about that tenant."),),
    responses(
        (status = OK, description = "An event stream: `ready`, then `advanced`, then `reconnect`.", content_type = "text/event-stream", body = String),
        (status = UNAUTHORIZED, body = Problem),
        (status = FORBIDDEN, body = Problem),
        (status = NOT_FOUND, body = Problem),
        (status = TOO_MANY_REQUESTS, description = "Too many streams open for this business on this server. `Retry-After` says when.", body = Problem),
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
        let mut conn = tenant.db.read().await.map_err(|e| {
            erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG)
        })?;
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
```
plus the public handler from Task 6 (add a placeholder-free stub now so `routes!` compiles: write the real handler in Task 6; until then, register only `event_stream` in `routes()` and add the second `.routes(…)` line in Task 6).

Notes for the implementer:
- `Problem::with_header` may not exist. Check `crates/erp-web/src/problem.rs`; if there is no way to attach a header, add `pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self` that stores extra headers and emits them in `IntoResponse` — follow whatever the rate limiter's `too_many_requests` does for `Retry-After` (`grep -n "RETRY_AFTER\|Retry-After" crates/erp-web/src/*.rs`) and reuse that.
- `erp_web::ApiError::Access` takes the error the other handlers give it (`e.into()`); mirror `onboarding_row` in `modules/tax_sa/src/http.rs`. If `checkpoint_of`'s `sqlx::Error` does not convert, map it to the 500 the tax module's read-model failure uses (`erp_tenant::messages::INTERNAL`).
- `lib.rs`: `mod realtime;`. `routes.rs`: `.merge(crate::realtime::routes())`. `Cargo.toml` as listed.

- [x] **Step 5: Wire the binary.** In `bin/api.rs`, after the `_invalidations` line:

```rust
    // **Where open streams wait.** Only with Redis, because a projection
    // advance on a worker reaches an API node through it; without one the
    // stream routes refuse rather than sit silent.
    let realtime = match control.shared() {
        Some(_) => {
            let caps = erp_web::realtime::Caps {
                staff_per_tenant: env_usize("REALTIME_STAFF_STREAMS_PER_TENANT", 256)?,
                public_per_tenant: env_usize("REALTIME_PUBLIC_STREAMS_PER_TENANT", 4096)?,
            };
            tracing::info!(?caps, "live streams enabled");
            Some(Arc::new(erp_web::realtime::Hub::new(caps)))
        }
        None => {
            tracing::warn!("REDIS_URL is not set; nothing can be watched live");
            None
        }
    };
    let _advances = realtime
        .as_ref()
        .and_then(|hub| erp_web::realtime::listen_in_background(&control, Arc::clone(hub)));
```
and where the state is built, after `.storing_in`/before `let app = router(state)`:

```rust
    if let Some(hub) = realtime {
        state = state.streaming_through(hub);
    }
```
and the helper at the bottom of the file:

```rust
/// A number from the environment, or its default; a value that is set and
/// does not parse stops the process rather than silently becoming the default.
fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| format!("{name} is not a number: {e}").into()),
        Err(_) => Ok(default),
    }
}
```
(check `main`'s error type; it already uses `?` on `Shared::from_env`, so a `Box<dyn Error>` result fits — adapt the `.into()` to whatever it is.)

- [x] **Step 6: Run**

```bash
cargo clippy -p erp-api --all-targets -- -D warnings
cargo nextest run -p erp-api --test http ready_names_every_group a_signal_for_a_module opening_a_stream_asks without_redis_nothing the_caps_refuse a_stream_ends_after every_role_against_every_endpoint
```
Expected: all pass, matrix 217.

- [x] **Step 7: Falsify.** (a) In `live`, delete `if !modules.contains(&signal.module) { continue; }` → `a_signal_for_a_module_the_tenant_lacks_is_not_delivered` fails (booking arrives first). Restore. (b) Delete `nudge(&state, tenant_id).await;` → `opening_a_stream_asks_for_a_visit` fails. Restore. (c) In `event_stream`, replace `.filter(|(_, setup)| tenant.db.has_module(&setup.module))` with `.filter(|_| true)` → `ready_names_every_group…` fails on `booking`. Restore. (d) In `live`, change `sleep_until(open.deadline)` to `sleep_until(open.deadline + std::time::Duration::from_secs(3600))` → the lifetime test fails. Restore; all pass.

---

### Task 6: The phone's stream, and the deposit status that waits

**Files:**
- Modify: `crates/erp-api/src/realtime.rs` (add `public_reservation_events`, register it), `crates/erp-api/src/deposits.rs` (`public_settings` and `nothing_here` become `pub(crate)`; `public_deposit_status` gains `Consistency`), `crates/erp-api/tests/openapi.rs` (PUBLIC list), `crates/erp-api/tests/http.rs` (two tests)

**Interfaces:**
- Consumes: `live`, `Watch::Subject`, `hub_of`, `full` (Task 5); `deposits::{public_settings, nothing_here}`; `<booking::Reservation as erp_eventlog::Aggregate>::domain()`; `<booking::Booking as erp_projection::ProjectionGroup>::NAME`.
- Produces: handler `public_reservation_events` at `GET /v1/booking/public/reservations/{reservation}/events`; `?consistent_after=` on `public_deposit_status`.

- [x] **Step 1: Write the failing tests** in `http.rs`:

```rust
/// **A phone hears only its own reservation**, and "many" wakes it too.
#[tokio::test]
async fn a_phone_hears_only_its_own_reservation() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    open_the_diary(&fixture, tenant).await;

    let stream_for = |id: &str| {
        Request::get(format!("/v1/booking/public/reservations/{id}/events"))
            .body(Body::empty())
            .unwrap()
    };
    let mine = idem("PUBLIC-BOOKING-1");
    let theirs = idem("PUBLIC-BOOKING-2");
    let (status, mut my_body) = fixture.open_stream(stream_for(&mine)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, mut their_body) = fixture.open_stream(stream_for(&theirs)).await;
    let (mut my_buffer, mut their_buffer) = (String::new(), String::new());
    let (event, data) = next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2)).await.expect("ready");
    assert_eq!(event, "ready");
    let ready: serde_json::Value = serde_json::from_str(&data).expect("json");
    assert_eq!(ready["reservation"], mine);
    assert!(ready["position"].is_i64());
    next_event(&mut their_body, &mut their_buffer, Duration::from_secs(2)).await.expect("ready");

    let reservation = |id: &str| {
        erp_types::StreamId::new(
            <booking::Reservation as erp_eventlog::Aggregate>::domain(),
            erp_types::AggregateId::new(id).expect("an id"),
        )
    };
    fixture.hub.publish(&advanced(tenant, "booking", "booking", 9, Some(vec![reservation(&mine)])));
    let (event, data) = next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2)).await.expect("mine");
    assert_eq!(event, "advanced");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&data).expect("json"), serde_json::json!({ "position": 9 }));
    assert!(
        next_event(&mut their_body, &mut their_buffer, Duration::from_millis(300)).await.is_none(),
        "the other phone woke"
    );

    fixture.hub.publish(&advanced(tenant, "booking", "booking", 10, None));
    assert!(next_event(&mut my_body, &mut my_buffer, Duration::from_secs(2)).await.is_some());
    assert!(next_event(&mut their_body, &mut their_buffer, Duration::from_secs(2)).await.is_some(), "many did not wake every phone");

    fixture.cleanup().await;
}

/// **The exit criterion.** Two screens and a phone agree about a schedule
/// within a second of a booking, and nobody polled: the booking is made through
/// the public route, the worker projects and announces it, all three streams
/// hear it at the committed position, and a read at that position shows it.
#[expect(clippy::too_many_lines, reason = "the phase's exit criterion, told once")]
#[tokio::test]
async fn two_screens_and_a_phone_agree_within_a_second_and_nobody_polled() {
    let mut fixture = Fixture::new().await;
    let user = fixture.user("owner@acme.test", "hunter2hunter2").await;
    let tenant = fixture.provision("acme").await;
    fixture.join(user, tenant).await;
    fixture.enable_module(tenant, crm::setup()).await;
    fixture.enable_module(tenant, booking::setup()).await;
    let token = fixture.token("owner@acme.test", "hunter2hunter2").await;
    let bearer = |request: axum::http::request::Builder| {
        request
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
    };
    let (status, body, _) = fixture
        .send(
            bearer(Request::post("/v1/booking/resources"))
                .header("Idempotency-Key", idem("CHAIR-1"))
                .body(Body::from(
                    serde_json::json!({ "id": "CHAIR-1", "name": "كرسي", "kind": "person", "capacity": 1 }).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    open_the_diary(&fixture, tenant).await;
    fixture.project_booking(tenant).await;

    // Two counter screens, and the phone that is about to book.
    let staff = || bearer(Request::get("/v1/events")).body(Body::empty()).unwrap();
    let reservation = idem("PUBLIC-BOOKING-1");
    let (_, mut screen_a) = fixture.open_stream(staff()).await;
    let (_, mut screen_b) = fixture.open_stream(staff()).await;
    let (_, mut phone) = fixture
        .open_stream(
            Request::get(format!("/v1/booking/public/reservations/{reservation}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let (mut buf_a, mut buf_b, mut buf_p) = (String::new(), String::new(), String::new());
    for (body, buffer) in [(&mut screen_a, &mut buf_a), (&mut screen_b, &mut buf_b), (&mut phone, &mut buf_p)] {
        let (event, _) = next_event(body, buffer, Duration::from_secs(2)).await.expect("ready");
        assert_eq!(event, "ready");
    }

    // The booking, from the phone.
    let (status, body, _) = fixture
        .send(
            Request::post("/v1/booking/public/reservations")
                .header(header::CONTENT_TYPE, "application/json")
                .header("Idempotency-Key", &reservation)
                .body(Body::from(
                    serde_json::json!({
                        "customer_name": "سارة",
                        "customer_phone": "+966500000000",
                        "lines": [{ "resource": "CHAIR-1", "from": "2026-05-01T09:00:00Z", "until": "2026-05-01T10:00:00Z" }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The worker, played by this test exactly as `ProjectionJob::tick` does:
    // project in a transaction, commit, announce what was committed.
    let announced = {
        let db = fixture.control.enter_for_maintenance(tenant).await.expect("maintenance entry");
        let projections = booking::projections();
        let refs: Vec<&dyn erp_projection::Projection<Group = booking::Booking>> =
            projections.iter().map(AsRef::as_ref).collect();
        let mut tx = db.begin().await.expect("transaction");
        let progress = erp_projection::run_once_in::<booking::Booking>(&mut tx, &refs, booking::upcasters(), 200)
            .await
            .expect("projects");
        let erp_projection::Progress::Advanced { to, streams, .. } = progress else {
            panic!("nothing to project: {progress:?}");
        };
        tx.commit().await.expect("commits");
        let signal = erp_control::shared::Advanced {
            tenant,
            group: "booking".to_owned(),
            module: booking::module_id(),
            position: to,
            streams,
        };
        fixture.hub.publish(&signal);
        to
    };

    // Within a second, all three heard it, at the committed position.
    for (body, buffer, expected) in [
        (&mut screen_a, &mut buf_a, serde_json::json!({ "group": "booking", "position": announced.get() })),
        (&mut screen_b, &mut buf_b, serde_json::json!({ "group": "booking", "position": announced.get() })),
        (&mut phone, &mut buf_p, serde_json::json!({ "position": announced.get() })),
    ] {
        let (event, data) = next_event(body, buffer, Duration::from_secs(1)).await.expect("advanced within a second");
        assert_eq!(event, "advanced");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&data).expect("json"), expected);
    }

    // And a read at that position shows the booking — the screen's re-fetch,
    // and the phone's.
    let (status, body, _) = fixture
        .send(
            bearer(Request::get(format!(
                "/v1/booking/reservations/{reservation}?consistent_after={}",
                announced.get()
            )))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stage"], "reserved");

    fixture.cleanup().await;
}
```
with the shared helper (near `idem`):

```rust
/// Turns online booking on for a tenant, the way the settings route would.
async fn open_the_diary(fixture: &Fixture, tenant: TenantId) {
    let db = fixture.control.enter_for_maintenance(tenant).await.expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        booking::PublicBooking::KEY,
        &booking::PublicBooking { verify_phone: false, hold_minutes: 0, open: true, deposit_bp: 0 },
        None,
        None,
    )
    .await
    .expect("stores the setting");
}
```
(Confirm the staff read's path with `grep -n 'path = "/v1/booking/reservations/{' modules/booking/src/http.rs`; it is the one whose handler is `get_reservation`.) Add the public route to `tests/openapi.rs`'s `PUBLIC` list next to the deposit entries:

```rust
        // The phone's stream: signals about one reservation, keyed by an id
        // only the phone that booked it holds, behind the public limiter.
        ("get", "/v1/booking/public/reservations/{reservation}/events"),
```

- [x] **Step 2: Run, expect failures** (404 on the public route; the openapi public test fails):

```bash
cargo nextest run -p erp-api --test http a_phone_hears_only two_screens_and_a_phone
```

- [x] **Step 3: The public handler**, in `crates/erp-api/src/realtime.rs`, and `.routes(routes!(public_reservation_events))` in `routes()`:

```rust
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
    let reservation = erp_types::AggregateId::new(reservation.trim()).map_err(|_| {
        erp_web::bad_request(erp_web::messages::MALFORMED_BODY, "reservation", "", locale)
    })?;
    if !crate::deposits::public_settings(&caller, locale).await?.open {
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
        let mut conn = caller.db.read().await.map_err(|e| {
            erp_web::ApiError::Access(e.into()).into_problem(locale, &CATALOG)
        })?;
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
```
(`bad_request`'s message code: reuse whatever `parse_id` in `deposits.rs` uses — read it and use the same code.) In `deposits.rs`, make `async fn public_settings` and `fn nothing_here` `pub(crate)`. Add `consistent_after` to `public_deposit_status`: parameter `consistency: erp_web::Consistency,`; the utoipa `params` gain `("consistent_after" = Option<i64>, Query, description = "Wait for the booking read model to reach this log position — the one a stream's `advanced` named."),`; first line of the body after the module checks: `consistency.wait_for(&caller.db, <booking::Booking as erp_projection::ProjectionGroup>::NAME, locale).await?;`.

- [x] **Step 4: Run**

```bash
just openapi
cargo nextest run --no-fail-fast -p erp-api --test http a_phone_hears_only two_screens_and_a_phone every_role_against_every_endpoint --test openapi --test compatibility
cargo clippy -p erp-api --all-targets -- -D warnings
```
Expected: pass; compatibility additive (new operation, new optional query param); public list satisfied.

- [x] **Step 5: Falsify.** (a) In `Hub::publish`'s `Some(streams)` arm, send to every subject of the tenant regardless of key (replace the lookup with `subjects.retain(...)` as in the `None` arm) → `a_phone_hears_only_its_own_reservation` fails ("the other phone woke"). Restore. (b) In the test-side worker, skip `fixture.hub.publish(&signal)` → the exit test fails ("advanced within a second"); restore. (c) In `public_reservation_events`, build the stream with `AggregateId::new("nobody")` → the exit test's phone assertion fails. Restore; all pass.

---

### Task 7: Documents, the spec deviation, and the gates

**Files:**
- Modify: `docs/IMPLEMENTATION.md` (Phase 13a/13b boxes; §46 above `### 45 ·`), `docs/RUNNING.md` (after "ZATCA, end to end" or at the end), `docs/ARCHITECTURE.md` §1.3 (~line 192), the spec's "lagged" sentences (§2 and the error table)

- [x] **Step 1: Spec deviation.** In the spec, replace the §2 sentence "a receiver that falls behind gets `Lagged`, and the stream answers that by re-reading the checkpoints and sending a fresh `ready`…" and the error-table row "Client falls 64 signals behind | API | fresh `ready` instead of the missed signals" with: *the stream sends `reconnect` and ends; the reconnect's `ready` is the snapshot. `TenantDb` is not `Clone`, and no stream touches a database.* (Edit the exact sentences by `grep -n "Lagged" docs/superpowers/specs/2026-09-08-realtime-signal-stream-design.md`.)

- [x] **Step 2: `ARCHITECTURE.md` §1.3**, after the `pg_notify` paragraph:

```markdown
Real time (Phase 13) does not change this. The worker announces each projection
commit on a Redis channel, API nodes fan it out to open server-sent-event
streams, and a stream carries a signal — *group G is queryable through N* —
never data and never a database connection. `pg_notify` stays refused.
```

- [x] **Step 3: `RUNNING.md`**, a section "Watching a tenant live":

````markdown
## Watching a tenant live

Needs Redis (`REDIS_URL`); without it the routes answer 503. A staff screen:

```bash
curl -N $API/v1/events -H "$H" -H "$A"
```

```text
event: ready
data: {"groups":{"booking":1234,"sales":980}}

event: advanced
data: {"group":"booking","position":1235}

: keep-alive

event: reconnect
```

Re-fetch through the ordinary API with `?consistent_after=1235`; never apply a
delta, the stream carries none. The phone that booked watches its reservation:

```bash
curl -N $API/v1/booking/public/reservations/$RESERVATION/events -H "$H"
```

and re-fetches `…/reservations/$RESERVATION/deposit?consistent_after=<position>`.
Streams end after ten minutes with `reconnect`; `EventSource` reconnects by
itself and the fresh `ready` says what moved. Caps per business per server:
`REALTIME_STAFF_STREAMS_PER_TENANT` (256) and
`REALTIME_PUBLIC_STREAMS_PER_TENANT` (4096).
````

- [x] **Step 4: `IMPLEMENTATION.md`.** Tick the six 13a boxes and the three 13b boxes (`- [ ]` → `- [x]` for every box between `### 13a` and `### 13c`), appending to the 13b "reaches every screen" box: ` — and the phone that booked, on its own stream (§46)`. Insert above `### 45 ·`:

```markdown
### 46 · The signal stream: two screens and a phone agree, and nobody polled

**Every read was a poll, and the phase said so.** A booking from a phone had to
reach every counter screen without a refresh, and `pg_notify` was refused by D4.
What replaces polling is the shape 13a wrote down before it was built: a
server-sent-event stream that carries *group `booking` is queryable through N*
and nothing else, published by the worker **after** the projection commit —
the `Advanced` arm in `jobs.rs`, the one moment the guarantee is true — and
fanned out over the Redis channel `shared.rs` already had. The client
re-fetches through the ordinary API with `consistent_after`, which already does
authorization, localization and paging. A stream holds a receiver, a deadline
and a list of module ids; never a connection.

**Two surfaces, apart on purpose.** Staff watch a tenant on `GET /v1/events`;
a customer's phone watches one reservation on
`GET /v1/booking/public/reservations/{id}/events`, keyed by the id only the
phone holds, behind the public limiter. They are different populations —
dozens against thousands — so the hub keeps them in separate registries with
separate caps (`REALTIME_STAFF_STREAMS_PER_TENANT`,
`REALTIME_PUBLIC_STREAMS_PER_TENANT`), and a public signal is never routed
through a staff sender. For the phone to be woken only for its own booking
without a database read per open stream, the signal names the streams a batch
touched — `Progress::Advanced.streams`, bounded at 256, `None` for "many".

**Reconnect is the reconcile.** Every stream's first event is `ready`, the
checkpoint of every visible group; every stream ends at ten minutes with
`reconnect`, so authorization is re-run by the reconnect rather than outlived;
and a watcher behind the 64-signal buffer is sent `reconnect` too, because
`TenantDb` is deliberately not `Clone` and the cheapest fresh snapshot that
touches no database from inside a stream is the next `ready`.

**The exit criterion is a test:**
`two_screens_and_a_phone_agree_within_a_second_and_nobody_polled` books through
the public route, plays the worker the way `ProjectionJob::tick` does, and reads
`advanced` at the committed position on two staff streams and the phone's, then
reads the reservation at that position. Beside it:
`ready_names_every_group_the_tenant_may_see_and_nothing_else`,
`a_signal_for_a_module_the_tenant_lacks_is_not_delivered`,
`opening_a_stream_asks_for_a_visit`, `without_redis_nothing_can_be_watched`,
`the_caps_refuse_the_stream_past_them`,
`a_stream_ends_after_its_lifetime_with_reconnect`,
`a_phone_hears_only_its_own_reservation`; the worker's
`a_projection_that_advances_signals_once_with_the_committed_position`; the
runner's `an_advance_names_the_streams_it_touched`; the hub's four unit tests;
and `an_advance_published_is_received_by_a_subscriber` over a real Redis.

**Left by decision:** 13c notifications and 13d conversations, next in that
order; `Last-Event-ID` replay; per-branch filtering; public streams for anything
but a reservation, which the subject registry is ready for.
```

- [x] **Step 5: Gates**

```bash
just openapi
cargo nextest run --no-fail-fast -p erp-api --test openapi --test compatibility
cargo nextest run -p erp-api --test http every_role_against_every_endpoint
cargo nextest run --no-fail-fast -p erp-api --test idempotence -p erp-projection --test purity -p erp-eventlog --test write_side an_instant_becomes
just prepare
SQLX_OFFLINE=true cargo check --workspace --all-targets
cargo fmt --all -- --check
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --no-fail-fast -p erp-projection -p erp-control -p erp-worker -p erp-web -p erp-api
```
Expected: all green; `.sqlx` unchanged (no new `query!`).

- [x] **Step 6: Stop and summarize.** What was built, every falsification with the line reverted, the spec deviation, and `just check` for the user. Do not commit.

---

## Self-review against the spec

- §1 signal: Tasks 1 (streams), 2 (`Advanced`, channel), 3 (`Signals`, publish after commit, wiring). `StreamId` already serializes; that spec line is a no-op.
- §2 hub: Task 4 (registries, caps, capacity 64, `listen_in_background`, `AppState.realtime`, `streaming_through`, env caps in Task 5's bin wiring). Lagged → reconnect (deviation, recorded in Task 7).
- §3 routes: Task 5 (staff, order of checks, `ready`, filter, keep-alive, lifetime, 503, 429), Task 6 (public, `Public`, subject key, `consistent_after` on the deposit status). Messages: Task 4. OpenAPI and role matrix: Tasks 5 and 6.
- §4 client contract: `RUNNING.md` in Task 7.
- Error table: covered by Tasks 2 (publish warn), 4 (resubscribe), 5/6 (503/429), 5 (lifetime).
- Tests: every name in the spec has a task, except the spec's `a_lagged_watcher…` which is the hub unit test of the same name (the API-level lag is covered by the deviation and the lifetime test's mechanism).
- Type names used consistently: `Advanced`, `Signals`, `Hub`, `Caps`, `Full`, `Watch`, `live`, `hub_of`, `full`, `no_realtime`, `open_stream`, `next_event`, `advanced(...)`, `open_the_diary`.
- No placeholders remain; two implementer checks are explicit (`Problem::with_header`, `parse_id`'s message code, `get_reservation`'s path) with the command that answers each.
