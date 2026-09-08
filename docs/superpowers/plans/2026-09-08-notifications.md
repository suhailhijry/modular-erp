# Notifications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A notification is a durable record in the tenant's log, addressed to a
person's login, readable per person, live through the Phase 13a stream, and
raised by five kinds the system already knows about.

**Architecture:** `hr` learns which login is which employee. `messaging` gains a
fifth channel (`in_system`) and a people-first resolver. A new `notifications`
module owns two aggregates — `Notification` (announced, read) and `Person`
(preferences, read-all) — one projection group, and five routes. Announcements
are raised from the composition roots only, because `notifications → messaging →
booking/crm/hr/sales` means a domain module cannot announce without a cycle.

**Tech Stack:** Rust, axum 0.8, sqlx (offline `.sqlx`), Postgres, `utoipa`,
`erp-eventlog` (`try_create`/`try_execute` inside the caller's transaction),
`erp-projection`, `erp-i18n`.

## Global Constraints

- **Do not commit.** Leave every change in the working tree.
- Every guard test is **falsified**: revert the fix, watch the test fail,
  restore it.
- Clippy is `-D warnings`; `too_many_lines` is 100 (use
  `#[expect(clippy::too_many_lines, reason = "…")]`); no `expect_used` in
  non-test `erp-api`/`erp-web`.
- **Event names are globally unique** across every module — they are the
  projection dispatch key.
- **No write path mints identity**: derived ids only (`Uuid::new_v5`), never
  `Uuid::new_v4`/`now_v7`/`rand` in `http.rs` or `commands.rs`.
- **A projection never reads while applying** unless the read is its own
  working table with a `// projection-read:` comment within 24 lines.
- **An instant becomes a day only through the calendar.**
- Both languages, always: English and Arabic, neither a translation of a
  compiled string.
- Iteration env:
  `export SQLX_OFFLINE=false DATABASE_URL="postgres://postgres:postgres@localhost:55432/erp_typecheck" REDIS_URL=redis://127.0.0.1:56379/`
- Gates before done: `just prepare` (no `.sqlx` churn), `just openapi`, fmt,
  workspace clippy, the three source-scan meta-tests, the role matrix.

---

## File Structure

**New crate — `modules/notifications/`**

| File | Responsibility |
|---|---|
| `Cargo.toml` | deps: `erp-eventlog`, `erp-projection`, `erp-tenant`, `erp-types`, `erp-i18n`, `erp-web`, `messaging`, axum/utoipa/serde/sqlx/uuid |
| `schema/install.sql` | `inbox`, `preference` |
| `src/lib.rs` | module id, setup, groups, upcasters, catalog, `name`/`domain` helpers |
| `src/kind.rs` | the five kinds: topic, ordered audiences, `as_str`/`FromStr` |
| `src/copy.rs` | compiled bilingual wording per kind, and its vocabulary test |
| `src/notification.rs` | `Notification` aggregate + `NotificationEvent` |
| `src/person.rs` | `Person` aggregate + `PersonEvent` (preferences, read-all) |
| `src/announce.rs` | `announce()` — resolve, word, prefer, write, fan out |
| `src/commands.rs` | `read`, `read_all`, `set_preferences` |
| `src/projections.rs` | group `Notifications`, `Inbox` projection, reads |
| `src/http.rs` | five routes |
| `src/messages.rs` | refusals, EN/AR |
| `tests/notifications.rs` | module-level tests against a real tenant |

**Modified**

| File | Change |
|---|---|
| `modules/hr/src/employee.rs` | `LoginLinked`/`LoginUnlinked` events, `identity` on the aggregate |
| `modules/hr/schema/install.sql` | `identity TEXT` + index on `employee` |
| `modules/hr/src/projections.rs` | apply the two events; `EmployeeSummary.identity` |
| `modules/hr/src/commands.rs` | `link_login`, `unlink_login`, `HrError::LoginTaken` |
| `modules/hr/src/http.rs` | `PUT`/`DELETE /v1/hr/employees/{employee}/login` |
| `modules/hr/src/messages.rs` | `LOGIN_TAKEN`, `NOT_A_MEMBER` |
| `modules/messaging/src/channel.rs` | `Channel::InSystem`, `kind() -> Option<EffectKind>` |
| `modules/messaging/src/audience.rs` | `Person` public with `identity`; `people()` |
| `modules/messaging/src/send.rs` | refuse in-system; extract `deliver()` |
| `modules/messaging/src/template.rs` | in-system needs a title |
| `modules/booking/src/projections.rs` | `reserved_since()` read |
| `modules/payments/src/projections.rs` | `finished_since()` read |
| `modules/tax_sa/src/documents.rs` | `refused_since()` read |
| `crates/erp-api/src/modules.rs` | register `notifications` |
| `crates/erp-worker/src/bin/worker.rs` | projection job, three producers, one new job |
| `crates/erp-api/tests/http.rs` | seven endpoints in the role matrix, the live test |
| `docs/{ARCHITECTURE,IMPLEMENTATION,RUNNING}.md`, `docs/openapi.json` | |

---

## Task 1: An employee may name the login that is that person

**Files:**
- Modify: `modules/hr/src/employee.rs`, `modules/hr/schema/install.sql`,
  `modules/hr/src/projections.rs`, `modules/hr/src/commands.rs`,
  `modules/hr/src/http.rs`, `modules/hr/src/messages.rs`, `modules/hr/src/lib.rs`
- Test: `modules/hr/tests/hr.rs`, `crates/erp-api/tests/http.rs`

**Interfaces:**
- Produces: `hr::link_login(&TenantDb, &AggregateId, &str, Timestamp, &Metadata)`,
  `hr::unlink_login(&TenantDb, &AggregateId, Timestamp, &Metadata)`,
  `EmployeeSummary.identity: Option<String>`,
  `hr::employee_by_login(&mut PgConnection, &str) -> Result<Option<EmployeeSummary>, sqlx::Error>`

- [x] **Step 1: Write the failing test** in `modules/hr/tests/hr.rs`

```rust
/// **Which bell rings.** An audience resolves to an employee; a bell belongs to
/// a login. Without this field the two never meet.
#[tokio::test]
async fn a_login_linked_to_an_employee_is_on_their_record() {
    let f = Fixture::new("hr_login").await;
    let sara = f.hire("E-1", "Sara").await;
    let login = uuid::Uuid::new_v4().to_string();

    hr::link_login(&f.db, &sara, &login, at("2026-09-08"), &Metadata::default())
        .await
        .expect("links");
    f.project().await;

    let mut conn = f.db.acquire().await.expect("connection");
    assert_eq!(
        hr::employee(&mut conn, "E-1").await.expect("read").expect("a row").identity,
        Some(login.clone())
    );

    // And a second employee may not hold it.
    let omar = f.hire("E-2", "Omar").await;
    let refused = hr::link_login(&f.db, &omar, &login, at("2026-09-08"), &Metadata::default())
        .await
        .expect_err("one login is one person");
    assert!(matches!(
        refused,
        CommandError::Execute(ExecuteError::Rejected(hr::HrError::LoginTaken(_)))
    ));

    // Unlinking frees it.
    hr::unlink_login(&f.db, &sara, at("2026-09-08"), &Metadata::default())
        .await
        .expect("unlinks");
    f.project().await;
    hr::link_login(&f.db, &omar, &login, at("2026-09-08"), &Metadata::default())
        .await
        .expect("now free");
}
```

- [x] **Step 2: Run it and watch it fail**

```bash
cargo nextest run -p hr a_login_linked_to_an_employee_is_on_their_record
```
Expected: FAIL — `link_login` does not exist.

- [x] **Step 3: The events**

In `modules/hr/src/employee.rs`, add to `EmployeeEvent`:

```rust
    /// **Which login is this person.** Not authorization: no capability check
    /// reads this, and 9c's refusal to let an org chart feed the platform's
    /// role stands. It answers which bell rings.
    LoginLinked { identity: String, at: Timestamp },
    /// They no longer log in as themselves — they left, or the account moved.
    LoginUnlinked { at: Timestamp },
```

Extend `NAMES` to include `"hr.employee.login_linked"` and
`"hr.employee.login_unlinked"` (append — the array length changes, and the
`event_name` match arms index into it). Add `pub identity: Option<String>` to
the `Employee` aggregate and apply both arms.

- [x] **Step 4: The read model**

`modules/hr/schema/install.sql`, inside `CREATE TABLE employee`:

```sql
    -- **Which login is this person**, when somebody has said. Nothing about
    -- authorization reads it; `notifications` reads it to know whose bell to
    -- ring. Not unique here: a projection that can fail to apply is a
    -- projection that stops, so the refusal lives in the command.
    identity      TEXT,
```

and after the table:

```sql
CREATE INDEX IF NOT EXISTS employee_by_identity_idx
    ON employee (identity) WHERE identity IS NOT NULL;
```

In `modules/hr/src/projections.rs`: apply both events with
`UPDATE employee SET identity = $2, position = $3 WHERE id = $1` (NULL on
unlink), add `identity` to `EmployeeSummary`, to the `summarise!` macro, and to
the `SELECT` lists in `employees` and `employee`. Add:

```rust
/// Whose record a login belongs to, if anybody's.
pub async fn employee_by_login(
    conn: &mut PgConnection,
    identity: &str,
) -> Result<Option<EmployeeSummary>, sqlx::Error> {
    let row = sqlx::query!(
        r#"SELECT id as "id!", name as "name!", name_latin, email, phone,
                  reports_to, branch, identity, hired_on as "hired_on!", left_at
             FROM proj_hr.employee WHERE identity = $1"#,
        identity
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| summarise!(r)))
}
```

- [x] **Step 5: The commands**

`modules/hr/src/commands.rs`, following `transfer`'s shape (load, refuse if the
employee does not exist, `try_execute`, `Decision::nothing()` when unchanged):

```rust
/// Says which login is this person.
///
/// **The uniqueness check is here and not an index.** Two employees holding one
/// login would be one person with two inboxes; a unique index on a projection
/// column would instead stop the projection, which is worse than the thing it
/// prevents. A simultaneous pair can still slip through and costs a duplicate
/// notification, not a wrong one.
pub async fn link_login(
    db: &TenantDb,
    id: &AggregateId,
    identity: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome
```

with `HrError::LoginTaken(String)` (the employee id already holding it) and its
`Localize` arm on `messages::LOGIN_TAKEN`. `unlink_login` is the same shape,
`Decision::nothing()` when there is no link.

- [x] **Step 6: Run the test**

```bash
cargo nextest run -p hr a_login_linked_to_an_employee_is_on_their_record
```
Expected: PASS.

- [x] **Step 7: Falsify it** — delete the `LoginTaken` check, watch the test
  fail on the second link, restore it.

- [x] **Step 8: The routes**

`modules/hr/src/http.rs`, mounted with `.routes(routes!(link_employee_login, unlink_employee_login))`:

```rust
/// Say which login is this person.
///
/// **`ManageTenant`**, because it decides who receives what — the same
/// authority as adding a member.
#[utoipa::path(
    put,
    path = "/v1/hr/employees/{employee}/login",
    tag = "hr",
    …
)]
async fn link_employee_login(
    tenant: Allowed<ManageTenant>,
    State(state): State<AppState>,
    Language(locale): Language,
    Path(id): Path<String>,
    Json(body): Json<LinkLogin>,
) -> Result<Json<HrAccepted>, Problem>
```

The handler checks membership before the command — `erp-api` is not reachable
from here, but `state.control` is, and `hr` already depends on `erp-control`:

```rust
    let members = state
        .control
        .members(tenant.db.tenant())
        .await
        .map_err(|e| /* 503, messages::DATABASE */)?;
    if !members.iter().any(|m| m.identity.to_string() == body.identity) {
        return Err(bad_request(Message::new(crate::messages::NOT_A_MEMBER)
            .with("identity", MessageArg::text(body.identity.clone())), locale, &crate::CATALOG));
    }
```

- [x] **Step 9: Role matrix**

In `crates/erp-api/tests/http.rs`, add `("link_employee_login", MANAGERS)` and
`("unlink_employee_login", MANAGERS)` to the operations table (whatever the
existing `ManageTenant` row set is called), and raise the asserted count from
217 to 219.

```bash
cargo nextest run -p erp-api every_role_against_every_endpoint
```

- [x] **Step 10: Regenerate and check**

```bash
just prepare && just openapi && cargo clippy --workspace --all-targets -- -D warnings
```

---

## Task 2: The in-system channel, and a people-first resolver

**Files:**
- Modify: `modules/messaging/src/channel.rs`, `audience.rs`, `send.rs`,
  `template.rs`, `messages.rs`, `lib.rs`, `transport.rs` (call sites of `kind()`)
- Test: `modules/messaging/tests/messaging.rs` and the unit tests in those files

**Interfaces:**
- Consumes: `hr::EmployeeSummary.identity` (Task 1)
- Produces:
  - `messaging::Channel::InSystem`, `Channel::kind(self) -> Option<EffectKind>`
  - `pub struct messaging::audience::Person { pub identity: Option<String>, pub email: Option<String>, pub phone: Option<String> }`
    with `pub fn reachable_on(&self, Channel) -> Option<Address>`
  - `messaging::audience::people(&mut PgConnection, Audience, &Subject, Option<&str>) -> Result<Vec<Person>, sqlx::Error>`
  - `messaging::deliver(&mut PgConnection, &Outbound, key: String, at: Timestamp) -> Result<bool, SendError>`

- [x] **Step 1: Write the failing tests** — three, in `channel.rs` and
  `audience.rs` unit modules:

```rust
// channel.rs
/// **In-system leaves nothing.** Every other channel is an effect kind a
/// handler claims; a bell is a record this system writes itself, and giving it
/// a kind would leave rows in the outbox that nothing will ever deliver.
#[test]
fn only_the_channels_that_leave_the_system_are_effect_kinds() {
    assert_eq!(Channel::InSystem.kind(), None);
    for channel in Channel::ALL.into_iter().filter(|c| *c != Channel::InSystem) {
        assert!(channel.kind().is_some(), "{channel} promises nothing");
    }
}

// audience.rs
/// A person is reachable in-system only once somebody has said which login is
/// theirs. A customer never is: customers do not log in.
#[test]
fn in_system_reaches_a_linked_login_and_nobody_else() {
    let linked = Person {
        identity: Some("11111111-1111-1111-1111-111111111111".to_owned()),
        email: Some("a@b.test".to_owned()),
        phone: None,
    };
    assert_eq!(
        linked.reachable_on(Channel::InSystem).map(|a| a.value),
        Some("11111111-1111-1111-1111-111111111111".to_owned())
    );
    let unlinked = Person { identity: None, email: Some("a@b.test".to_owned()), phone: None };
    assert!(unlinked.reachable_on(Channel::InSystem).is_none());
    assert!(unlinked.reachable_on(Channel::Email).is_some());
}
```

- [x] **Step 2: Run them and watch them fail**

```bash
cargo nextest run -p messaging only_the_channels_that_leave_the_system_are_effect_kinds in_system_reaches_a_linked_login_and_nobody_else
```

- [x] **Step 3: The channel**

`channel.rs`: add `InSystem` to the enum and to `ALL` (now 5), `as_str` →
`"in_system"`, `has_a_subject` → true for `Email | InSystem` (a bell has a
title), `units` → 1. Change `kind` to return `Option<EffectKind>`:

```rust
    /// The effect this channel's messages are enqueued under, or `None` for a
    /// channel that never leaves this system.
    #[must_use]
    pub fn kind(self) -> Option<EffectKind> {
        let name = match self {
            Self::Email => "email.send",
            Self::Sms => "sms.send",
            Self::Push => "push.send",
            Self::WhatsApp => "whatsapp.send",
            // A bell is a record, not a promise. See `notifications`.
            Self::InSystem => return None,
        };
        Some(EffectKind::new(name).unwrap_or_else(|_| unreachable!("a literal that satisfies EffectKind")))
    }
```

Fix every call site the compiler names (`send::Outbound::promised`,
`transport::handlers`, the existing `every_channel_has_its_own_effect_kind`
test — rename it to reflect the four that leave).

- [x] **Step 4: The resolver returns people**

`audience.rs`: make `Person` public with `pub identity`, `pub email`,
`pub phone`; make `reachable_on` take `&self`; add `Channel::InSystem =>
self.identity.clone()` to it. Fill `identity` from `hr` in `employee()` and
`managers_of()`; `client_of` leaves it `None` with a comment. Then:

```rust
/// Who this reaches, as people rather than addresses.
///
/// [`resolve`] is this filtered to one channel. Announcing needs the person —
/// one recipient reached in-system *and* by SMS is one person with two
/// preferences, and a list of addresses cannot say that.
pub async fn people(
    conn: &mut PgConnection,
    audience: Audience,
    subject: &Subject,
    operator: Option<&str>,
) -> Result<Vec<Person>, sqlx::Error>
```

and re-express `resolve` as `people(...).filter_map(|p| p.reachable_on(channel))`.

- [x] **Step 5: Split `deliver` out of `send`, and refuse in-system there**

`send.rs`:

```rust
/// Charges the meter and promises one message. **The tail of [`send`]**, for a
/// caller that has already resolved a person — which is what announcing does.
///
/// Returns whether anything was written: `false` means this key was already
/// promised, and then nothing is charged.
pub async fn deliver(
    conn: &mut PgConnection,
    message: &Outbound,
    key: String,
    at: Timestamp,
) -> Result<bool, SendError> {
    let Some(_) = message.channel.kind() else {
        return Err(SendError::NotSendable { channel: message.channel.as_str().to_owned() });
    };
    if erp_eventlog::enqueue(conn, None, std::slice::from_ref(&message.promised(key))).await? == 0 {
        return Ok(false);
    }
    budget::charge(conn, message.channel, message.channel.units(&message.body), at).await?;
    Ok(true)
}
```

`send`'s loop becomes a call to `deliver`. At the top of `send`, refuse an
in-system template:

```rust
    if template.channel == Channel::InSystem {
        // An announcement is announced, not sent: it is a record in the log,
        // and `send` promises effects. See `notifications::announce`.
        return Err(SendError::NotSendable { channel: template.channel.as_str().to_owned() });
    }
```

Add `SendError::NotSendable { channel }`, `messages::NOT_SENDABLE` in both
languages, and the `Localize` arm.

- [x] **Step 6: Templates**

`template.rs`: `check` requires a subject line for `InSystem` as it does for
`Email` (a bell with no title is a bell nobody reads), and the vocabulary is
unchanged. `Topic::audiences` is unchanged.

- [x] **Step 7: Run the messaging suite**

```bash
cargo nextest run -p messaging
```

- [x] **Step 8: Falsify twice** — (a) make `InSystem.kind()` return
  `Some("in_system.send")` and watch the channel test fail; (b) make
  `reachable_on(InSystem)` fall back to `self.email` and watch the audience test
  fail. Restore both.

---

## Task 3: The `notifications` module — schema, aggregates, projection

**Files:**
- Create: `modules/notifications/Cargo.toml`, `schema/install.sql`,
  `src/{lib,kind,notification,person,projections,messages}.rs`,
  `tests/notifications.rs`
- Modify: `Cargo.toml` (workspace members), `crates/erp-api/src/modules.rs`,
  `crates/erp-api/Cargo.toml`, `crates/erp-worker/src/bin/worker.rs`,
  `crates/erp-worker/Cargo.toml`

**Interfaces:**
- Produces:
  - `notifications::Kind` (`BookingReserved`, `PaymentsSettled`, `PaymentsFailed`, `TaxRefused`, `DocumentExpiring`) with `as_str`, `topic() -> messaging::Topic`, `audiences() -> &'static [messaging::Audience]`, `ALL`, `FromStr`
  - `notifications::{Notification, NotificationEvent, Person, PersonEvent}`
  - `notifications::{module_id, setup, install, upcasters, projections, GROUP_NAME, CATALOG}`
  - `notifications::Notifications` (the projection group)

- [x] **Step 1: Write the failing test** in `modules/notifications/tests/notifications.rs`
  (fixture copied from `modules/files/tests/files.rs`, with `messaging`, `crm`,
  `hr` and `booking` installed):

```rust
/// **A record before it is a signal.** The announcement is in the log, and the
/// inbox is what a rebuild makes of it.
#[tokio::test]
async fn an_announcement_becomes_a_row_in_one_persons_inbox() {
    let f = Fixture::new("notif_inbox").await;
    let login = "11111111-1111-1111-1111-111111111111";

    let mut tx = f.db.begin().await.expect("transaction");
    erp_eventlog::try_create::<Notification, _, std::convert::Infallible>(
        &mut tx,
        &code("N-1"),
        notifications::upcasters(),
        &Metadata::default(),
        |_| Ok(Decision::one(NotificationEvent::Announced {
            kind: "booking.reserved".to_owned(),
            topic: "reservation".to_owned(),
            subject: code("R-1"),
            recipients: vec![login.to_owned()],
            wording: wording("New booking", "حجز جديد"),
            at: at("2026-09-08"),
        })),
    )
    .await
    .expect("announces");
    tx.commit().await.expect("commits");

    f.project().await;

    let mut conn = f.db.acquire().await.expect("connection");
    let page = notifications::inbox(&mut conn, login, false, 20, None).await.expect("read");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].kind, "booking.reserved");
    assert!(page.items[0].read_at.is_none());
    assert_eq!(notifications::unread(&mut conn, login).await.expect("count"), 1);

    // Somebody else's inbox is empty, and that is a `WHERE`, not a filter.
    let other = notifications::inbox(&mut conn, "22222222-2222-2222-2222-222222222222", false, 20, None)
        .await
        .expect("read");
    assert!(other.items.is_empty());
}
```

- [x] **Step 2: Run it and watch it fail** (the crate does not exist)

```bash
cargo nextest run -p notifications
```

- [x] **Step 3: The crate**

`modules/notifications/Cargo.toml` copied from `modules/files/Cargo.toml`, plus
`messaging = { path = "../messaging" }` and `uuid`. Add
`"modules/notifications"` to the workspace `members`.

- [x] **Step 4: The schema** — `modules/notifications/schema/install.sql`

```sql
-- One person's copy of one notification.
--
-- **Per recipient, not per notification.** "Sara has seen this and Ahmed has
-- not" is the question an inbox exists to answer, and a single row with a set
-- of readers cannot answer it without a join nobody can index.
CREATE TABLE IF NOT EXISTS inbox (
    notification TEXT NOT NULL,
    -- The login this belongs to. Every read is `WHERE recipient = $1`.
    recipient    TEXT NOT NULL,
    kind         TEXT NOT NULL,
    topic        TEXT NOT NULL,
    subject_id   TEXT NOT NULL,
    -- `{"en": {"title": …, "body": …}, "ar": {…}}`. Rendered when it was
    -- announced, in both languages, so the bell answers `Accept-Language` and
    -- a booking that moved is described as it stood.
    wording      JSONB NOT NULL,
    announced_at TIMESTAMPTZ NOT NULL,
    read_at      TIMESTAMPTZ,
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL,
    PRIMARY KEY (notification, recipient)
);

CREATE INDEX IF NOT EXISTS inbox_by_recipient_idx
    ON inbox (recipient, announced_at DESC, notification);
CREATE INDEX IF NOT EXISTS inbox_unread_idx
    ON inbox (recipient) WHERE read_at IS NULL;
-- What an announcer asks: "have I already said this?"
CREATE INDEX IF NOT EXISTS inbox_by_subject_idx ON inbox (kind, subject_id);

-- What one person wants, for one kind.
--
-- A row is a **statement**, and its absence is the default — which is why the
-- channels are an array rather than a row per channel: "nothing for this kind"
-- must be distinguishable from "never said".
CREATE TABLE IF NOT EXISTS preference (
    identity    TEXT NOT NULL,
    kind        TEXT NOT NULL,
    channels    TEXT[] NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL,
    position    BIGINT NOT NULL,
    PRIMARY KEY (identity, kind)
);
```

- [x] **Step 5: The aggregates**

`src/notification.rs` — domain `notifications_notification`, names
`notifications.notification.announced` and `notifications.notification.read`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationEvent {
    Announced {
        kind: String,
        topic: String,
        subject: AggregateId,
        /// The logins this reached, resolved at the moment it was announced.
        recipients: Vec<String>,
        /// Locale code → `{title, body}`.
        wording: BTreeMap<String, Wording>,
        at: Timestamp,
    },
    /// One person has seen it.
    Read { by: String, at: Timestamp },
}
```

The aggregate holds `recipients: Vec<String>` and `read: BTreeSet<String>` so
`read` can refuse a stranger and no-op a repeat.

`src/person.rs` — domain `notifications_person`, names
`notifications.person.preferences_set` and `notifications.person.read_all`:

```rust
pub enum PersonEvent {
    PreferencesSet { identity: String, entries: BTreeMap<String, Vec<Channel>>, at: Timestamp },
    /// Everything in this person's inbox at this point in the log is read.
    ///
    /// **No watermark.** The projection applies events in position order, so
    /// "every row that exists now" is exactly "every notification announced
    /// before this" — during a rebuild as much as live.
    ReadAll { identity: String, at: Timestamp },
}
```

- [x] **Step 6: The projection** — `src/projections.rs`, group
  `Notifications { NAME = "notifications", SCHEMA = "proj_notifications" }`,
  one projection `Inbox`:
  - `Announced` → one `INSERT … ON CONFLICT (notification, recipient) DO NOTHING`
    per recipient.
  - `Read` → `UPDATE inbox SET read_at = $3 WHERE notification = $1 AND recipient = $2 AND read_at IS NULL`.
  - `ReadAll` → `UPDATE inbox SET read_at = $2 WHERE recipient = $1 AND read_at IS NULL`.
  - `PreferencesSet` → delete this identity's rows and insert the given ones
    (a whole-grid replacement, per the spec).

  Plus the reads `inbox(conn, recipient, unread_only, limit, after) -> Page<InboxRow>`,
  `unread(conn, recipient) -> i64`,
  `announced_subjects(conn, kind, &[String]) -> HashSet<String>`,
  `preferences_for(conn, &[String], kind) -> HashMap<String, Vec<Channel>>`.

- [x] **Step 7: `lib.rs`** — `module_id()` = `notifications`, `setup()` with
  `.requiring(&["messaging"])`, `install()` creating `proj_notifications`,
  `upcasters()` over both event name lists, `GROUP_NAME`, `CATALOG`.

- [x] **Step 8: Register it in the three composition roots**
  - `crates/erp-api/src/modules.rs`: a `Registered` entry after `messaging`.
  - `crates/erp-worker/src/bin/worker.rs`: a `ProjectionJob::<notifications::Notifications>`
    in `module_jobs`, `.for_module(notifications::module_id()).signalling(signals.cloned())`.
  - Both crates' `Cargo.toml`.

- [x] **Step 9: Run the test**

```bash
just prepare && cargo nextest run -p notifications an_announcement_becomes_a_row_in_one_persons_inbox
```

- [x] **Step 10: Falsify it** — change the inbox read's `WHERE recipient = $1`
  to `WHERE TRUE`, watch the "somebody else's inbox is empty" assertion fail,
  restore it.

---

## Task 4: `announce` — resolve, word, prefer, write, fan out

**Files:**
- Create: `modules/notifications/src/{announce,copy}.rs`
- Modify: `modules/notifications/src/{lib,kind,messages}.rs`
- Test: `modules/notifications/tests/notifications.rs`

**Interfaces:**
- Consumes: `messaging::audience::people`, `messaging::deliver`,
  `messaging::bindings::of`, `messaging::template::{Templates, KEY, render}`,
  `notifications::preferences_for`
- Produces:
  - `notifications::Announcing { kind: Kind, subject: messaging::Subject, at: Timestamp }`
  - `notifications::Announced { recipients: usize, announced: bool, promised: usize }`
  - `notifications::announce(&mut PgConnection, &Announcing, &Metadata) -> Result<Announced, AnnounceError>`
  - `notifications::AnnounceError` with `Unreachable { kind, audience, topic, id }`
  - `notifications::DEFAULT_CHANNELS: [Channel; 1] = [Channel::InSystem]`

- [x] **Step 1: Write four failing tests**

```rust
/// **The audience decides the inbox.** The stylist on the booking gets it; when
/// nobody is assigned, whoever runs the branch does.
#[tokio::test]
async fn a_booking_reaches_the_stylist_and_otherwise_the_manager() { … }

/// **A default does not spend money.** With no preferences set, announcing
/// writes the bell and promises nothing; turning SMS on for that kind promises
/// one and charges the meter.
#[tokio::test]
async fn nothing_is_sent_until_somebody_asks_for_it() { … }

/// Announcing twice writes one notification and says so.
#[tokio::test]
async fn announcing_the_same_thing_twice_writes_one() {
    let first = announce(&mut tx, &announcing, &Metadata::default()).await.expect("announces");
    assert!(first.announced);
    let second = announce(&mut tx, &announcing, &Metadata::default()).await.expect("again");
    assert!(!second.announced, "the second call wrote a second notification");
}

/// A tenant's own wording wins over the compiled copy — and an inactive
/// template does not.
#[tokio::test]
async fn a_tenants_template_beats_the_compiled_copy() { … }
```

- [x] **Step 2: Run them and watch them fail**

- [x] **Step 3: The kinds** — `src/kind.rs`

```rust
/// What the system tells people about.
///
/// **A closed set.** A tenant does not write a template for "a booking
/// arrived"; they choose whether they want it and where. The wording is
/// [`crate::copy`]'s, and a tenant may override it with a template of the same
/// name on the `in_system` channel.
pub enum Kind { BookingReserved, PaymentsSettled, PaymentsFailed, TaxRefused, DocumentExpiring }
```

with `as_str` (`"booking.reserved"`, `"payments.settled"`, `"payments.failed"`,
`"tax_sa.refused"`, `"hr.document_expiring"`), `topic()`, and:

```rust
    /// Who to tell, in order. **The first that resolves to anybody wins** — so
    /// a booking reaches the stylist it was made with, and the manager only
    /// when nobody was assigned.
    pub const fn audiences(self) -> &'static [Audience] {
        match self {
            Self::BookingReserved => &[Audience::Worker, Audience::BranchManager],
            Self::PaymentsSettled | Self::PaymentsFailed | Self::TaxRefused => &[Audience::BranchManager],
            Self::DocumentExpiring => &[Audience::Worker, Audience::BranchManager],
        }
    }
```

- [x] **Step 4: The copy** — `src/copy.rs`, a compiled table of
  `(kind, locale, title, body)` for all five kinds in English and Arabic, using
  the same `{{ binding }}` names a template may use, e.g.

```rust
    ("booking.reserved", "en", "New booking",
     "{{ customer.name }} booked {{ reservation.starts_at }}."),
    ("booking.reserved", "ar", "حجز جديد",
     "حجز {{ customer.name }} بتاريخ {{ reservation.starts_at }}."),
```

and a source-level test:

```rust
/// **Every kind can be worded, in both languages, from bindings that resolve.**
/// A placeholder that is not in the topic's vocabulary renders as braces in
/// front of whoever the notification is for.
#[test]
fn every_kind_has_both_languages_and_says_only_what_can_be_resolved() { … }
```

- [x] **Step 5: `announce`** — `src/announce.rs`

```rust
pub async fn announce(
    conn: &mut PgConnection,
    announcing: &Announcing,
    metadata: &Metadata,
) -> Result<Announced, AnnounceError> {
    // 1 · Who. The first audience that resolves to anybody with a login.
    let mut people = Vec::new();
    for audience in announcing.kind.audiences() {
        people = messaging::audience::people(&mut *conn, *audience, &announcing.subject, None)
            .await?
            .into_iter()
            .filter(|p| p.identity.is_some())
            .collect();
        if !people.is_empty() { break; }
    }
    if people.is_empty() {
        return Err(AnnounceError::Unreachable { … });
    }

    // 2 · What it says, in both languages, now.
    let wording = wording(&mut *conn, announcing).await?;

    // 3 · The record, before anything leaves the building.
    let id = derived_id(announcing.kind, &announcing.subject);
    let recipients: Vec<String> = people.iter().filter_map(|p| p.identity.clone()).collect();
    let committed = erp_eventlog::try_create::<Notification, _, Infallible>(…).await?;
    if committed.events.is_empty() {
        // Already announced. Nothing was written and nothing is promised
        // again — the same shape `send` uses to report a repeat.
        return Ok(Announced { recipients: recipients.len(), announced: false, promised: 0 });
    }

    // 4 · The channels each person asked for, beyond the bell.
    let wanted = crate::preferences_for(&mut *conn, &recipients, announcing.kind).await?;
    let mut promised = 0;
    for person in &people { … messaging::deliver(conn, &outbound, key, announcing.at).await? … }

    Ok(Announced { recipients: recipients.len(), announced: true, promised })
}

/// **Derived, never minted** (L8): the same kind about the same subject is the
/// same notification, which is what lets every producer be a scan that may run
/// twice.
fn derived_id(kind: Kind, subject: &Subject) -> AggregateId {
    let name = format!("{}:{}:{}", kind.as_str(), subject.topic.as_str(), subject.id.as_str());
    AggregateId::new(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()).to_string())
        .unwrap_or_else(|_| unreachable!("a uuid satisfies AggregateId"))
}
```

`wording` reads the tenant's templates
(`config::get::<Templates>(conn, messaging::template::KEY)`), takes the one
named `kind.as_str()` **if it is active and on `Channel::InSystem`**, else the
compiled copy; renders both locales through `messaging::template::render`
against `messaging::bindings::of(conn, subject)` plus the business name.

The effect key is `format!("{}.{}.{}", kind.as_str(), subject.id, identity)`
per channel suffix, so a retry promises one message (L8).

- [x] **Step 6: Run the four tests**

- [x] **Step 7: Falsify four times**
  - Reverse `audiences()` for `BookingReserved` → the stylist test fails.
  - Default the channels to `[InSystem, Sms]` → the money test fails.
  - Replace `try_create` with `try_execute` → the twice test fails.
  - Ignore `template.active` → the override test fails.

---

## Task 5: The inbox routes

**Files:**
- Create: `modules/notifications/src/{commands,http}.rs`
- Test: `crates/erp-api/tests/http.rs`

**Interfaces:**
- Produces: `notifications::{read, read_all}` commands;
  `GET /v1/notifications`, `POST /v1/notifications/{id}/read`,
  `POST /v1/notifications/read`

- [x] **Step 1: Write the failing test** in `crates/erp-api/tests/http.rs`

```rust
/// **An inbox is one person's.** Two logins, one announcement: the other sees
/// nothing and cannot mark it read — a 404, because a 403 would confirm it
/// exists.
#[tokio::test]
async fn a_notification_belongs_to_one_person() { … }
```

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The commands** — `read(db, notification, by, at, metadata)`
  refusing `NotYours` when `by` is not among the recipients and
  `Decision::nothing()` when already read; `read_all(db, identity, at, metadata)`
  on the `Person` aggregate.

- [x] **Step 4: The routes** — all three `Allowed<Read>`, all three taking the
  caller from `tenant.session.identity`:

```rust
/// Your own notifications, newest first.
///
/// **`Read`, though it writes.** A viewer must be able to clear their own bell:
/// it is not an administrative act on the tenant, and requiring `PostEntries`
/// would make the most junior person's inbox unusable.
```

`NotYours` maps to **404**, not 403.

- [x] **Step 5: Role matrix** — three more operations, count 219 → 222.

- [x] **Step 6: Run, then falsify** — make `read` skip the recipient check and
  watch the 404 assertion fail.

---

## Task 6: Preferences

**Files:**
- Modify: `modules/notifications/src/{commands,http,projections}.rs`
- Test: `crates/erp-api/tests/http.rs`

**Interfaces:**
- Produces: `notifications::set_preferences(&TenantDb, &str, BTreeMap<String, Vec<Channel>>, Timestamp, &Metadata)`;
  `GET`/`PUT /v1/notifications/preferences`

- [x] **Step 1: Write the failing test**

```rust
/// The grid comes back with the defaults filled in, and what a person sets
/// replaces it whole.
#[tokio::test]
async fn a_person_sets_their_own_grid_and_gets_it_back() { … }
```

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The command and the routes** — `PUT` is a whole-grid
  replacement; an unknown kind is `notifications.unknown_kind` and an unknown
  channel `notifications.unknown_channel`, both 400. `GET` merges stored rows
  over `DEFAULT_CHANNELS` for every `Kind::ALL`.

- [x] **Step 4: Role matrix** — two more, count 222 → 224.

- [x] **Step 5: Run, then falsify** — make `PUT` merge instead of replace and
  watch the "replaces it whole" assertion fail.

---

## Task 7: Producer — new bookings

**Files:**
- Modify: `modules/booking/src/projections.rs`, `modules/booking/src/lib.rs`,
  `crates/erp-worker/src/bin/worker.rs`
- Test: `crates/erp-worker/tests/modules.rs`

**Interfaces:**
- Produces: `booking::reserved_since(&mut PgConnection, Timestamp, i64) -> Result<Vec<ReservationSummary>, sqlx::Error>`;
  the `AnnounceNewBookings` job

- [x] **Step 1: Write the failing test** — a tenant with `booking`,
  `messaging`, `hr`, `crm` and `notifications`, a linked manager, a booking
  made inside the window, one tick of the job, one row in the manager's inbox,
  and a second tick that adds nothing.

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The read** — over the `reserved_on` column that already exists:

```rust
/// Bookings made since an instant, oldest first.
///
/// **When it was made, not when it starts.** A window on `starts_at` would
/// announce every future booking on the first tick after a tenant enables
/// notifications; this announces what is new.
pub async fn reserved_since(
    conn: &mut PgConnection,
    since: Timestamp,
    limit: i64,
) -> Result<Vec<ReservationSummary>, sqlx::Error>
```

- [x] **Step 4: The job** — in `bin/worker.rs`, requiring both modules:

```rust
/// How far back an announcer looks. Wide enough to survive a worker restart,
/// narrow enough that enabling the module does not announce a week of history.
const ANNOUNCE_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(6);

struct AnnounceNewBookings;
```

`tick`: skip unless `db.has_module(&notifications::module_id())`; read the
window; ask `notifications::announced_subjects` which are already done;
announce the rest one transaction at a time, logging and continuing on
`Unreachable` exactly as `BookingReminders` does.

- [x] **Step 5: Run, then falsify** — drop the `announced_subjects` filter and
  the derived id, and watch the "second tick adds nothing" assertion fail.

---

## Task 8: Producer — payments settled and failed

**Files:**
- Modify: `modules/payments/src/projections.rs`, `modules/payments/src/lib.rs`,
  `crates/erp-worker/src/bin/worker.rs`
- Test: `crates/erp-worker/tests/modules.rs`

**Interfaces:**
- Produces: `payments::finished_since(&mut PgConnection, Timestamp, i64) -> Result<Vec<Finished>, sqlx::Error>`
  where `Finished { id: String, stage: String, invoice: Option<String>, advance_for: Option<String> }`

- [x] **Step 1: Write the failing test** — a settled deposit announces
  `payments.settled` against its reservation; a failed one announces
  `payments.failed`.

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The read** — `WHERE stage IN ('settled','retained','failed')
  AND coalesce(settled_at, started_at) > $1`, with a comment that a failure is
  windowed on `started_at` because there is no `failed_at`, and that the derived
  id makes a repeat free.

- [x] **Step 4: The hook** — at the end of `SettleGatewayPayments::tick`,
  after the existing repair, under
  `db.has_module(&notifications::module_id())`.

- [x] **Step 5: Run, then falsify** — map both stages to
  `Kind::PaymentsSettled` and watch the failed-payment assertion fail.

---

## Task 9: Producer — a ZATCA refusal

**Files:**
- Modify: `modules/tax_sa/src/documents.rs`, `modules/tax_sa/src/lib.rs`,
  `crates/erp-worker/src/bin/worker.rs`
- Test: `modules/tax_sa/tests/tax_sa.rs` or `crates/erp-worker/tests/modules.rs`

**Interfaces:**
- Produces: `tax_sa::refused_since(&mut PgConnection, Timestamp, i64) -> Result<Vec<Refused>, sqlx::Error>`
  where `Refused { number: String, source: String }`

- [x] **Step 1: Write the failing test** — a document ZATCA refused announces
  `tax_sa.refused` against its source invoice, to the branch manager.

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The read** — `WHERE status = 'refused' AND settled_at > $1
  ORDER BY settled_at, id`.

- [x] **Step 4: The hook** — in the submission sweep inside `zatca_jobs`,
  after the sweep records its verdicts.

- [x] **Step 5: Run, then falsify** — announce against the document number
  instead of `source_id` and watch the audience resolve to nobody.

---

## Task 10: Producer — an expiring work document

**Files:**
- Modify: `crates/erp-worker/src/bin/worker.rs`
- Test: `crates/erp-worker/tests/modules.rs`

- [x] **Step 1: Write the failing test** — an iqama expiring inside
  `DOCUMENT_WARNING_DAYS` announces `hr.document_expiring` against the employee,
  and the existing health finding still fires.

- [x] **Step 2: Run it and watch it fail**

- [x] **Step 3: The job** — `AnnounceExpiringDocuments`, reading
  `hr::expiring(conn, DOCUMENT_WARNING_DAYS, 200)` and announcing one per
  employee. **The `WorkDocumentExpiry` invariant stays**, with a comment saying
  why: it is the operator's channel and the bell is the tenant's, and a tenant
  without the module or without a linked login would otherwise be told by
  nobody.

- [x] **Step 4: Run, then falsify** — remove the announcement and watch the
  inbox assertion fail while the finding assertion still passes.

---

## Task 11: The live half, the documents, and the gates

**Files:**
- Test: `crates/erp-api/tests/http.rs`
- Modify: `docs/ARCHITECTURE.md`, `docs/IMPLEMENTATION.md`, `docs/RUNNING.md`,
  `docs/openapi.json`, `crates/erp-api/tests/openapi.rs`

- [x] **Step 1: The exit test**

```rust
/// **The bell rings without anybody polling.** An announcement advances the
/// `notifications` group, the worker publishes it, and the recipient's stream
/// carries the position — while a second login watching the same tenant sees
/// its own unread count unchanged.
#[tokio::test]
async fn a_bell_rings_on_one_screen_and_not_the_other() { … }
```

Built on the Phase 13a fixture helpers already in this file: `open_stream`,
`next_event`, `advanced(tenant, group, module, position, streams)`.

- [x] **Step 2: Run it, then falsify** — stop the projection job from
  signalling for this group and watch the stream assertion time out.

- [x] **Step 3: The documents**
  - `docs/IMPLEMENTATION.md`: tick 13c's three boxes; add a §47 explaining the
    layering rule (only what sits above the modules may announce), the derived
    id as the dedup key, and the two audiences for an expiring document.
  - `docs/ARCHITECTURE.md`: one paragraph in the module list.
  - `docs/RUNNING.md`: how to link a login and what a bell needs to be enabled.

- [x] **Step 4: The gates**

```bash
just openapi
just prepare
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run -p erp-api --test openapi
cargo nextest run -p erp-api no_write_path_mints_its_own_identity
cargo nextest run -p erp-projection a_projection_does_not_read_while_applying
cargo nextest run -p erp-eventlog an_instant_becomes_a_day_only_through_the_calendar
cargo nextest run -p notifications -p messaging -p hr -p erp-worker -p erp-api
```

- [x] **Step 5: Hand over `just check`. Do not commit.**

---

## Self-review

**Spec coverage.** Login link → Task 1. In-system channel and people-first
resolution → Task 2. Module, aggregates, projection, inbox filtered by
recipient → Task 3. Announce with catalog copy, template override, preferences,
fan-out, derived id, `Unreachable` → Task 4. Inbox routes, read as an event,
404 for a stranger → Task 5. Preferences grid and defaults → Task 6. Five kinds
across four producers → Tasks 7–10. Live signal, role matrix, OpenAPI, docs,
meta-tests → Task 11. Read state surviving a rebuild is covered by Task 3's
fixture (`f.project()` replays from the log) and asserted again in Task 5.

**Types.** `Kind` (Task 3) is used by `announce` (4), the routes (5, 6) and
every producer (7–10) under the same name. `Person` is `messaging::audience::Person`
(Task 2) and `notifications::Person` (the aggregate, Task 3) — different
modules, no import collision, but never `use` both unqualified in one file.
`Announced` is both a `NotificationEvent` variant and `announce`'s return type;
they are in different namespaces and the return type is
`notifications::Announced`.

**Placeholders.** Tasks 5–10 name the code by shape and exemplar rather than
quoting it in full; each names the file, the signature, the refusal, the test
and the falsification, which is what the executor needs given the patterns are
already in the tree.

---

## What was built differently, and why

Recorded against the plan rather than rewritten into it, so the two can be read
against each other.

**`proj_booking.reservation` already had `reserved_on`.** The plan's Task 7 was
written expecting to add a column and a fleet refresh; the instant a booking was
made was already recorded and simply had no reader. Only
`booking::reserved_since` was needed, and **there is no deploy step at all**.

**The four producers share one loop.** Tasks 7–10 would each have written the
same "skip what has been said, announce the rest, one transaction each, carry on
past a refusal" cycle in a binary no test can reach. It is
`notifications::announce_all` instead — one function, one test, and each job is
a read plus a call.

**Two bugs the plan did not know about, both found by building on them:**

- **An invoice has no branch**, so `Audience::BranchManager` about one resolved
  to nobody — which would have made three of the five kinds dead on arrival, and
  which had *already* been true of any Phase 11 template addressed that way.
  `messaging` now reads a subject with no branch as the business itself
  (whoever reports to nobody at all), and a record that is not there still
  resolves to nobody.
- **The kind names read as message codes.** `booking.reserved` in a route
  description tripped `every_code_the_document_cites_exists`, and the ambiguity
  was real rather than the test being wrong: they are now `booking_reserved`,
  `payments_settled`, `payments_failed`, `tax_refused`, `document_expiring`.

**`payments.settled` and `payments.failed` announce invoices only.** A deposit
against a booking has no invoice, and its subject would have to be the
reservation — a different kind. The diary already shows a paid deposit live.

**Three registries had to learn the module**, and each was a failing test rather
than a discovery in production: the worker's projection jobs, the migrator's
rebuild arms, and the demo's shadow replay. The demo now also seeds a bell — it
links the signup identity to the employee at the top of the chart and runs the
same sweep a worker would.

**Read state needed no watermark.** `read_all` is one event on the person and
the projection clears everything of theirs that is unread *at that point in the
log* — exact during a rebuild too, because events apply in position order.
