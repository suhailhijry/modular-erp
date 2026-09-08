# Conversations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A thread against a subject that holds internal notes, outward messages
and inbound replies — with a customer's SMS reply landing on the thing it
answers.

**Architecture:** `messaging` starts recording what it sent, as write-side state
beside the meter, so a reply can be correlated. A `conversations` module owns one
`Thread` aggregate (`noted`, `said`, `heard`, `assigned`) whose id is derived
from its subject. A worker job lands inbound webhooks — correlating against the
reply's own timestamp, so the answer is stable and the job needs no cursor.

**Tech Stack:** Rust, axum 0.8, sqlx offline, Postgres, `utoipa`,
`erp-eventlog` (`try_execute` in the caller's transaction), `erp-projection`.

**The exemplar is `modules/notifications`**, built in the same session: same
module shape, same derived-id idiom, same layering rule. Read it before writing
anything here.

## Global Constraints

- **Do not commit.** Leave everything in the working tree.
- Every guard is **falsified**: revert the fix, watch the test fail, restore.
- Clippy `-D warnings`; `too_many_lines` 100 (`#[expect(…, reason = "…")]`); no
  `expect_used` in non-test `erp-api`/`erp-web`.
- Event names are globally unique across modules — the projection dispatch key.
- Derived ids only (`Uuid::new_v5`); never `new_v4`/`now_v7`/`rand` in a write
  path.
- A projection never reads while applying without a `// projection-read:` marker.
- An instant becomes a day only through the calendar.
- Both languages for every refusal, English and Arabic.
- Env: `export SQLX_OFFLINE=false DATABASE_URL="postgres://postgres:postgres@localhost:55432/erp_typecheck" REDIS_URL=redis://127.0.0.1:56379/`
- Gates: `just prepare`, `just openapi`, fmt, workspace clippy, the three
  source-scan meta-tests, the role matrix.

---

## File Structure

**New crate — `modules/conversations/`**

| File | Responsibility |
|---|---|
| `Cargo.toml` | like `modules/notifications/Cargo.toml`, plus `crm` |
| `schema/install.sql` | `conversation_message`, `conversation_thread` |
| `src/lib.rs` | module id, setup, groups, upcasters, catalog, `name`/`domain` |
| `src/thread.rs` | `Thread` aggregate, `ThreadEvent`, `Subject`→id derivation |
| `src/commands.rs` | `note`, `say`, `hear`, `assign` |
| `src/projections.rs` | group `Conversations`, `Messages` projection, reads |
| `src/http.rs` | five routes |
| `src/messages.rs` | six refusals, EN/AR |
| `tests/conversations.rs` | against a real tenant |

**Modified**

| File | Change |
|---|---|
| `migrations/tenant/0015_message_sent.sql` | new: what messaging sent |
| `modules/messaging/src/send.rs` | `deliver` takes `about`, records; `send` passes its subject |
| `modules/messaging/src/sent.rs` | new: `record`, `last_sent_to` |
| `modules/notifications/src/announce.rs` | pass the subject to `deliver` |
| `modules/crm/src/projections.rs` | `customer_by_phone` |
| `crates/erp-api/src/modules.rs` | register `conversations` |
| `crates/erp-worker/src/bin/worker.rs` | projection job, `LandInboundMessages` |
| `crates/erp-worker/src/bin/migrator.rs` | rebuild arm |
| `crates/erp-demo/tests/demo.rs` + `src/lib.rs` | shadow replay + a seeded thread |
| `crates/erp-api/tests/http.rs` | five endpoints, the live test |
| `docs/{ARCHITECTURE,IMPLEMENTATION,RUNNING}.md`, `docs/openapi.json` | |

---

## Task 1: Messaging remembers what it sent

**Files:**
- Create: `migrations/tenant/0015_message_sent.sql`, `modules/messaging/src/sent.rs`
- Modify: `modules/messaging/src/{send.rs,lib.rs}`, `modules/notifications/src/announce.rs`
- Test: `modules/messaging/tests/messaging.rs`

**Interfaces:**
- Produces:
  - `messaging::sent::record(&mut PgConnection, key: &str, &Outbound, about: Option<&Subject>, at: Timestamp) -> Result<(), sqlx::Error>`
  - `messaging::last_sent_to(&mut PgConnection, address: &str, before: Timestamp, within: chrono::TimeDelta) -> Result<Option<Subject>, sqlx::Error>`
  - `messaging::deliver(&mut PgConnection, &Outbound, key: String, about: Option<&Subject>, at: Timestamp) -> Result<bool, SendError>`

- [x] **Step 1: Write the failing test**

```rust
/// **What a reply answers.** A gateway hands back a number and a body and
/// nothing else, so the only way to know what a reply is about is to remember
/// what was said to that number — and `send` used to keep nothing.
#[tokio::test]
async fn what_was_sent_is_remembered_against_what_it_was_about() {
    let fixture = Fixture::new("sent-record").await;
    fixture.a_salon().await;
    fixture.template("booking.reminder", sms("Your appointment", "موعدك")).await;

    fixture.send(&reminder("booking.reminder.BK-1", at("2026-05-04", "08"))).await.expect("sends");

    let mut conn = fixture.db.acquire().await.expect("connection");
    let about = messaging::last_sent_to(
        &mut conn,
        "+966500000001",
        at("2026-05-04", "09"),
        chrono::TimeDelta::days(7),
    )
    .await
    .expect("reads")
    .expect("something was sent to that number");
    assert_eq!(about.topic, Topic::Reservation);
    assert_eq!(about.id.as_str(), "BK-1");

    // **Before, not now.** A reply is answered by what was said *to it*, so a
    // window that ends before the send finds nothing.
    assert!(
        messaging::last_sent_to(&mut conn, "+966500000001", at("2026-05-04", "07"), chrono::TimeDelta::days(7))
            .await
            .expect("reads")
            .is_none()
    );
    // And a stale one is not correlated either.
    assert!(
        messaging::last_sent_to(&mut conn, "+966500000001", at("2026-05-04", "09"), chrono::TimeDelta::minutes(1))
            .await
            .expect("reads")
            .is_none()
    );

    fixture.cleanup().await;
}
```

- [x] **Step 2: Run it, watch it fail** — `cargo nextest run -p messaging what_was_sent_is_remembered`

- [x] **Step 3: The migration** — `migrations/tenant/0015_message_sent.sql`

```sql
-- **What was said, to whom, about what.**
--
-- Write-side state, beside the meter and the device tokens, and for the same
-- reason both of those are here rather than in a projection: a send is an
-- **effect promise, not an event**, so nothing about it is derivable from the
-- log and a rebuild must not destroy it.
--
-- It exists because a reply arrives as a number and a body. Correlating one to
-- the booking it answers is impossible without a record of what went to that
-- number, and `messaging::send` kept none.
CREATE TABLE IF NOT EXISTS message_sent (
    -- The outbox key. Primary, so a promise deduplicated by the outbox is
    -- recorded once here too.
    key           TEXT PRIMARY KEY,
    channel       TEXT NOT NULL,
    -- The number or address it went to, exactly as it was resolved.
    addressed_to  TEXT NOT NULL,
    -- What it was about, when the sender knew. Null for anything sent without a
    -- subject — a one-time code, a signup email.
    topic         TEXT,
    subject_id    TEXT,
    sent_at       TIMESTAMPTZ NOT NULL
);

-- The correlation query: what was last said to this address, before an instant.
CREATE INDEX IF NOT EXISTS message_sent_by_address_idx
    ON message_sent (addressed_to, sent_at DESC);
```

- [x] **Step 4: `modules/messaging/src/sent.rs`**

```rust
/// Records one promised message, and answers what a reply is about.
pub async fn record(
    conn: &mut PgConnection,
    key: &str,
    message: &Outbound,
    about: Option<&Subject>,
    at: Timestamp,
) -> Result<(), sqlx::Error>
```

`INSERT … ON CONFLICT (key) DO NOTHING`, and:

```rust
/// **What was last said to this address, as of an instant.**
///
/// `before` is the reply's own timestamp and never the clock: correlating
/// against "now" would give a different answer every time the job ran, and the
/// same reply would land on a different thread each time.
pub async fn last_sent_to(
    conn: &mut PgConnection,
    address: &str,
    before: Timestamp,
    within: chrono::TimeDelta,
) -> Result<Option<Subject>, sqlx::Error>
```

`WHERE addressed_to = $1 AND sent_at <= $2 AND sent_at > $2 - $3 AND topic IS NOT NULL ORDER BY sent_at DESC LIMIT 1`.

- [x] **Step 5: `deliver` records what it promised**

Add `about: Option<&Subject>` between `key` and `at`; call `sent::record` after
the charge, in the same transaction. `send` passes `Some(&sending.subject)`;
`notifications::announce` passes `Some(&announcing.subject)`.

- [x] **Step 6: Run it** — and the whole `messaging` and `notifications` suites.

- [x] **Step 7: Falsify** — make `last_sent_to` ignore `before` (use `now()`),
  watch the "before, not now" assertion fail. Restore.

---

## Task 2: A customer by their number

**Files:**
- Modify: `modules/crm/src/projections.rs`, `modules/crm/src/lib.rs`
- Test: `modules/crm/tests/crm.rs`

**Interfaces:**
- Produces: `crm::customer_by_phone(&mut PgConnection, phone: &str) -> Result<Option<CustomerSummary>, sqlx::Error>`

- [x] **Step 1: Write the failing test** — a registered customer is found by
  their number; one whose number differs by formatting is **not**, and that is
  the documented behaviour rather than a bug.

```rust
/// **Exactly, or not at all.** A last-nine-digits heuristic would put one
/// person's reply in another person's conversation, which is worse than not
/// matching — `conversations` has an unmatched tray for exactly this.
#[tokio::test]
async fn a_customer_is_found_by_the_number_they_gave() { … }
```

- [x] **Step 2: Run it, watch it fail**

- [x] **Step 3: The read**

```rust
pub async fn customer_by_phone(
    conn: &mut PgConnection,
    phone: &str,
) -> Result<Option<CustomerSummary>, sqlx::Error>
```

`WHERE phone = $1 AND archived_at IS NULL ORDER BY registered_on LIMIT 1` —
oldest first, so two records sharing a number resolve to the same one every
time.

- [x] **Step 4: Run it. Falsify** — match on a suffix instead, watch the
  formatting assertion fail. Restore.

---

## Task 3: The `conversations` module — thread, projection, registration

**Files:**
- Create: the crate as listed in File Structure
- Modify: `Cargo.toml`, `crates/erp-api/{Cargo.toml,src/modules.rs}`,
  `crates/erp-worker/{Cargo.toml,src/bin/worker.rs,src/bin/migrator.rs}`,
  `crates/erp-demo/{Cargo.toml,tests/demo.rs}`
- Test: `modules/conversations/tests/conversations.rs`

**Interfaces:**
- Produces:
  - `conversations::{Thread, ThreadEvent, Conversations, thread_id, tray_id}`
  - `conversations::{messages, thread, unmatched}` reads
  - `thread_id(&Subject) -> AggregateId` = `v5("{topic}:{id}")`;
    `tray_id(address) -> AggregateId` = `v5("unmatched:{address}")`

- [x] **Step 1: Write the failing test** — an event appended to a thread becomes
  a row, in order, with its kind; and a thread of one subject is not a thread of
  another.

- [x] **Step 2: Run it, watch it fail** (no crate)

- [x] **Step 3: The aggregate** — `src/thread.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadEvent {
    /// Internal. **Never leaves.**
    Noted { text: String, at: Timestamp },
    /// Outward, promised in the same transaction.
    Said { text: String, channel: Channel, to: String, at: Timestamp },
    /// Inbound, carrying the gateway's own id.
    Heard { from: String, text: String, message_id: String, at: Timestamp },
    /// The tray only: these were about that after all.
    Assigned { topic: String, subject: AggregateId, at: Timestamp },
}
```

Names `conversations.thread.{noted,said,heard,assigned}`. The aggregate holds
`heard: VecDeque<String>` bounded at `HEARD_WINDOW = 200` — the idiom
`hr::Employee::recent_days` uses — plus `assigned_to: Option<(String, AggregateId)>`.

- [x] **Step 4: The schema** — `conversation_message(thread, position, kind,
  text, channel, address, who, at, topic, subject_id, recorded_at)`, primary key
  `(thread, position)`, indexed by `(topic, subject_id, position)` and by
  `(thread, position)`; plus `conversation_thread(thread, topic, subject_id,
  address, last_at)` for the tray listing.

- [x] **Step 5: The projection** — one arm per event; `Assigned` **moves** rows:

```sql
UPDATE conversation_message
   SET thread = $2, topic = $3, subject_id = $4
 WHERE thread = $1
```

with a comment that this is rebuild-safe because the assignment applies after
the messages it moves, in position order, live and during a replay alike.

- [x] **Step 6: Register it** in all four composition roots — `erp-api`'s
  `REGISTERED`, the worker's `module_jobs`, the migrator's `rebuild` arm, and the
  demo's shadow replay list. **Each of these has a test that fails if you
  forget**; run them.

- [x] **Step 7: Run, then falsify** — make the projection ignore `thread` in its
  `WHERE`, watch "a thread of one subject is not another's" fail.

---

## Task 4: Notes and outward messages

**Files:**
- Modify: `modules/conversations/src/{commands.rs,messages.rs,lib.rs}`
- Test: `modules/conversations/tests/conversations.rs`

**Interfaces:**
- Produces:
  - `conversations::note(&TenantDb, &Subject, text, at, &Metadata)`
  - `conversations::say(&TenantDb, &Subject, text, Channel, at, &Metadata)`
  - `conversations::ConversationError::{NothingToSay, NoClient, NotReachableThere, NotAChannelForThis, AlreadyAssigned}`

- [x] **Step 1: Write four failing tests** — a note promises nothing and charges
  nothing; a message promises exactly one effect on the chosen channel and
  charges the meter; a channel the customer has no address for is refused; an
  employee thread refuses `say`.

- [x] **Step 2: Run them, watch them fail**

- [x] **Step 3: `note`** — `try_execute` on the thread, refusing empty text.

- [x] **Step 4: `say`** — in one transaction:

```rust
// **SMS and email only.** WhatsApp outside a 24-hour service window takes
// pre-approved templates and nothing else (§26), so free text typed here would
// be accepted by us and refused by Meta; push addresses devices rather than
// people. Refusing says so rather than promising something that will not
// arrive.
if !matches!(channel, Channel::Sms | Channel::Email) {
    return Err(ConversationError::NotAChannelForThis(channel.as_str().to_owned()));
}
```

then `messaging::audience::people(conn, Audience::Client, subject, None)` →
first with an address on that channel → `try_execute` appending `Said` →
`messaging::deliver(conn, &outbound, key, Some(subject), at)` with
`key = format!("conversations.{thread}.{position}")`.

**Roll back on a refusal**, including a spent budget: the meter is written
before the budget is checked, so a caller that swallows the refusal has spent
budget on a message it did not send.

- [x] **Step 5: Run, then falsify twice** — allow WhatsApp, watch the refusal
  test fail; drop the roll-back on a spent budget, watch the meter assertion
  fail.

---

## Task 5: Reading and writing over HTTP

**Files:**
- Modify: `modules/conversations/src/http.rs`
- Test: `crates/erp-api/tests/http.rs`

- [x] **Step 1: Write the failing test** — a clerk posts a note and a message
  and reads them back in order; **a viewer is refused the read**.

- [x] **Step 2: Run it, watch it fail**

- [x] **Step 3: Three routes**, all `Allowed<PostEntries>`:
  `GET /v1/conversations/{topic}/{subject}`,
  `POST /v1/conversations/{topic}/{subject}/notes`,
  `POST /v1/conversations/{topic}/{subject}/messages`.

```rust
/// **`PostEntries` to read, which is the one place in this API where reading is
/// not the most permissive capability.** A thread holds staff's private notes
/// about a customer, and `Read` is the role for an external accountant at year
/// end — every reason to see the books, none to see what the front desk wrote.
```

- [x] **Step 4: Role matrix** — five operations (three here, two in Task 6),
  count 224 → 229. Run `every_role_against_every_endpoint`.

- [x] **Step 5: Run, then falsify** — make the read `Allowed<Read>`, watch the
  viewer assertion fail.

---

## Task 6: Inbound — landing, correlation, and the tray

**Files:**
- Modify: `modules/conversations/src/{commands.rs,http.rs,lib.rs}`,
  `crates/erp-worker/src/bin/worker.rs`
- Test: `modules/conversations/tests/conversations.rs`, `crates/erp-api/tests/http.rs`

**Interfaces:**
- Produces:
  - `conversations::hear(&TenantDb, &Inbound, at, &Metadata) -> Result<Landed, …>` where `Inbound { message_id, from, text, sent_at }` and `Landed { thread: AggregateId, subject: Option<Subject>, fresh: bool }`
  - `conversations::land(&TenantDb, within: chrono::TimeDelta, limit: i64) -> Result<Landing, …>` — the whole sweep, so the worker job is a call
  - `conversations::assign(&TenantDb, address, &Subject, at, &Metadata)`
  - `PROVIDER: &str = "messages"`

- [x] **Step 1: Write four failing tests**
  1. a reply lands on the booking a reminder was sent about;
  2. with nothing recently sent, on the known customer's thread;
  3. with an unknown number, in the tray — and assigning moves it;
  4. the same webhook landed twice writes one message, and **correlation does
     not drift**: a later reminder about another booking does not move it.

- [x] **Step 2: Run them, watch them fail**

- [x] **Step 3: `hear`** — correlate, then `try_execute` on the chosen thread,
  no-op when `message_id` is already in the window.

```rust
// **Against the reply's own instant, never the clock.** Correlating against
// `now` would give a different answer every time the sweep ran, and the same
// reply would land on a different thread each time — which is what makes the
// sweep safe to re-run without a cursor.
let about = messaging::last_sent_to(conn, &inbound.from, inbound.sent_at, within).await?;
```

- [x] **Step 4: `land`** — read `webhook_event` for `PROVIDER`, parse each
  payload into `Inbound` (a payload that is not one is logged and skipped, never
  fatal), call `hear`, count.

- [x] **Step 5: The job** — `LandInboundMessages` in `bin/worker.rs`, gated on
  `db.has_module(&conversations::module_id())`, window 6 hours, batch 100.

- [x] **Step 6: The tray routes** — `GET /v1/conversations/unmatched`,
  `POST /v1/conversations/unmatched/{address}/assign`.

- [x] **Step 7: Run, then falsify three times** — correlate against `now`, watch
  the drift test fail; drop the heard-window check, watch the twice test fail;
  make an unknown number land nowhere, watch the tray test fail.

---

## Task 7: Live, documents, gates

**Files:**
- Modify: `crates/erp-api/tests/http.rs`, `crates/erp-demo/src/lib.rs`,
  `docs/{ARCHITECTURE,IMPLEMENTATION,RUNNING}.md`, `docs/openapi.json`

- [x] **Step 1: The live test** — a landed reply advances the group and the 13a
  stream carries `advanced{group:"conversations"}`; the thread re-fetch shows it.

- [x] **Step 2: The demo** — seed a thread: a note on a booking, and one inbound
  reply landed through `hear`, so `proj_conversations` is not empty and shadow
  replay proves something.

- [x] **Step 3: The documents** — tick 13d's three boxes; §48 on the
  correlation rule, the tray, and why `PostEntries` reads; a paragraph in
  `RUNNING.md` with the inbound contract and a `curl`.

- [x] **Step 4: The gates**

```bash
just openapi && just prepare
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p erp-api no_write_path_mints_its_own_identity
cargo nextest run -p erp-projection a_projection_does_not_read_while_applying
cargo nextest run -p erp-eventlog an_instant_becomes_a_day_only_through_the_calendar
cargo nextest run -p conversations -p messaging -p crm -p notifications -p erp-worker -p erp-api -p erp-demo
```

- [x] **Step 5: Hand over `just check`. Do not commit.**

---

## Self-review

**Spec coverage.** §1 record → Task 1. §2 inbound contract, job, correlation,
exact matching → Tasks 2 and 6. §3 thread, events, derived id, channel refusals,
notes-only → Tasks 3 and 4. §4 tray → Tasks 3 (the move) and 6 (the routes). §5
routes and capability → Task 5. Testing items 1–12 map onto Tasks 1, 4, 5, 6
and 7; item 11 (survives a rebuild) is Task 3's projection test plus the demo's
shadow replay in Task 7.

**Types.** `Subject` is `messaging::Subject` throughout. `Channel` is
`messaging::Channel`. `thread_id`/`tray_id` are named the same in Tasks 3, 4
and 6. `deliver`'s new parameter is `about: Option<&Subject>` in Task 1 and is
called that way in Task 4.

**Placeholders.** Tasks 2–7 name code by signature and exemplar rather than
quoting every line; `modules/notifications` is the working model for all of it,
and each step names its file, its refusal, its test and its falsification.

---

## What was built differently, and why

**One `hear_from` per message, and `land` as the sweep.** The plan had `hear`
doing the correlating; it is `conversations::hear` (write only, refuses a
message id it has seen) and `inbound::hear_from` (correlate, then write), so
the part with three read models in it is testable without a webhook.

**A spent budget had to survive as itself.** `messaging::deliver` returns a
`SendError`, and mapping the whole of it into a database fault made "the month
is out of money" indistinguishable from "the database is unwell" — a client
would retry the one thing retrying never fixes. `ConversationError::OverBudget`
carries the channel and the limit, and the route answers `402`.

**The projection can tell a tray thread from a real one by recomputing the id.**
The first version marked every thread that heard something as unmatched, which
put answered bookings in the tray — caught by the landing test. Because ids are
derived rather than minted, `tray_id(from) == thread` is an exact answer, and it
needs no flag on the event and no column to keep in step.

**No subject columns on a message.** The plan denormalised `topic`/`subject_id`
onto every line. The thread id already is the subject — the route computes the
same one the writer did — so the columns were a second answer kept in step by
nothing, and dropping them removed a projection-read as well.

**Two schema names collided.** `LineView` with `sales` and `NewMessage` with
`messaging`; `utoipa` keeps one definition per name and silently describes one
shape for both. They are `ConversationLine` and `OutwardMessage`, caught by the
guard that exists because `crm`'s `AddressBody` once overwrote `tax_sa`'s.

**Five registries, not four.** The plan named the API list, the worker jobs, the
migrator's rebuild arms and the demo's shadow replay. The schema-name audit is a
fifth, and every one of them failed loudly rather than letting the module ship
half-registered.
