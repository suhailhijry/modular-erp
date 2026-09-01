# erp-eventlog

The tenant event log, and everything built directly on it: appending, loading an
aggregate, reading events written by older builds, gapless document numbers,
sealed secrets, tenant configuration, and the outbox.

One log per tenant database, so nothing here carries a tenant id. The database
is the tenant.

**Depends on:** `erp-types`, `erp-i18n`.
**Used by:** `erp-projection`, `erp-tenant`, and through them every module.

## The invariant everything rests on

**Positions are gapless and their order is commit order.** That is L1, and it is
stronger than it sounds.

`BIGINT GENERATED ALWAYS AS IDENTITY` assigns at insert time and not at commit
time. Two concurrent transactions take 100 and 101, 101 commits first, and a
tailer reading `WHERE position > checkpoint ORDER BY position` sees 101, advances
its checkpoint past 100, and never comes back for it. That is rare under light
load and certain under contention, and it silently breaks replay because live and
replay observe different sets.

The mechanism is a single-row counter, `event_log_position`, read and advanced by
`UPDATE … RETURNING` inside the appending transaction. The row lock serialises
position assignment against commit order. The counter is ordinary transactional
data, so a rollback returns its number. A sequence would have burned it.

What the property buys: a tailer always sees an unbroken prefix of the log. Every
projection, every replay, and the whole reproducibility argument depend on this
and on nothing else. `tests/append.rs` proves it under concurrency.

## The files

| File | What is in it |
|---|---|
| [`aggregate.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/aggregate.rs) | `Aggregate`, `DomainEvent`, `Decision`, `load`, `execute`, `try_execute` |
| [`append.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/append.rs) | `NewEvent` and the untyped `append` |
| [`read.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/read.rs) | `read_since`, `read_stream`, `integrity` |
| [`envelope.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/envelope.rs) | `Envelope` and `Metadata` |
| [`upcast.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/upcast.rs) | `Upcasters`, reading old events |
| [`numbering.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/numbering.rs) | Gapless document numbers |
| [`config.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/config.rs) | Tenant configuration storage |
| [`secrets.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/secrets.rs) | `SealingKey`, sealed per-module secrets |
| [`outbox/effect.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/outbox/effect.rs) | `Effect`, `enqueue`, `PendingEffect` |
| [`outbox/dispatch.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-eventlog/src/outbox/dispatch.rs) | `Dispatcher`, `EffectHandler`, `RetryPolicy` |

Plus two statics:

```rust
pub static MIGRATIONS: sqlx::migrate::Migrator;   // migrations/tenant
pub static CATALOG: StaticCatalog;
```

## Writing an aggregate

Two traits, and nothing domain-specific in either. D11 keeps business domain out
of the framework.

```rust
pub trait DomainEvent: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    fn event_name(&self) -> EventName;
    fn schema_version(&self) -> SchemaVersion;
}

pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: DomainEvent;
    fn domain() -> DomainName;
    fn apply(&mut self, event: &Self::Event);
}
```

`event_name` is part of the API contract and of the stored data. Renaming one
orphans every event already written under the old name, so it is a migration and
not a refactor.

`apply` must be total and must not fail. By the time an event reaches it the
event is already history, and refusing it would mean the aggregate can no longer
be loaded at all. Validation belongs in the decision function, before the event
exists.

`Default` is how "does it exist?" gets answered. The log has no separate notion
of creation, so a version of zero means nothing has been written.

## Deciding, and the two halves of a decision

```rust
pub struct Decision<E> {
    pub events:  Vec<E>,
    pub effects: Vec<Effect>,
}

impl<E> Decision<E> {
    pub fn nothing() -> Self;
    pub fn record(events: Vec<E>) -> Self;
    pub fn one(event: E) -> Self;
    pub fn with_effect(self, effect: Effect) -> Self;
    pub fn with_effects(self, effects: impl IntoIterator<Item = Effect>) -> Self;
    pub fn is_empty(&self) -> bool;
}
```

Facts to record, and promises to keep. A decision function returns this instead
of performing I/O, which is what makes a command handler testable without a
network and replayable without side effects.

```rust
|loaded| Ok(Decision::one(Opened { .. }))
|loaded| Ok(Decision::one(Posted { .. })
            .with_effect(Effect::new(kind("email.send"), payload)))
|loaded| Ok(Decision::nothing())
```

`Decision::nothing()` is a success and not a failure. Re-issuing a command that
has already taken effect should be quiet.

`execute` takes this type directly and not anything convertible into it. An
earlier version accepted `impl Into<Decision<_>>`, which let a command return a
bare `Vec` of events and silently drop the effects it meant to promise.

## Loading and executing

```rust
pub struct Loaded<A> {
    pub aggregate: A,
    pub version: Sequence,
}
impl<A> Loaded<A> { pub fn is_new(&self) -> bool; }

pub async fn load<A: Aggregate>(
    conn: &mut PgConnection, id: &AggregateId, upcasters: &Upcasters,
) -> Result<Loaded<A>, LoadError>;

pub async fn load_since<A: Aggregate>(
    conn: &mut PgConnection, id: &AggregateId, upcasters: &Upcasters,
    aggregate: A, from: Sequence,
) -> Result<Loaded<A>, LoadError>;
```

The aggregate and its version travel together because the version is what the
next append is checked against. Passing an aggregate around without its version
is how an optimistic-concurrency check gets made against the wrong number.

`load_since` is the shape a snapshot would use. Snapshots are deliberately
absent: they optimize aggregates with long histories and nobody yet knows which
of those exist. Adding them later is additive, and this signature is where they
attach.

**Loading is only legal inside command handling.** That is L7, and
`crates/erp-eventlog/tests/write_side.rs` fails the build if `load` or
`load_since` is named outside `modules/*/src/commands.rs`. A read path that
rebuilds an aggregate is a read path that will be slow at 10,000 events and will
diverge from the projection nobody updated.

```rust
pub const MAX_ATTEMPTS: u32 = 5;

pub async fn execute<A, F, E>(
    pool: &PgPool, id: &AggregateId, upcasters: &Upcasters,
    metadata: &Metadata, decide: F,
) -> Result<Committed<A::Event>, ExecuteError<E>>;

pub async fn try_execute<A, F, E>(
    conn: &mut PgConnection, id: &AggregateId, upcasters: &Upcasters,
    metadata: &Metadata, decide: F,
) -> Result<Committed<A::Event>, ExecuteError<E>>;
```

`execute` loads, decides, appends, records the decision's effects, and retries if
somebody got there first. `try_execute` is one turn of that loop inside a
transaction you own.

**`decide` must be a pure function of the aggregate's state.** It runs again on
every retry, so a decision that reads a clock, generates an id, or writes
anywhere produces different results on the second attempt, and the one that got
committed is not the one that was checked. Retrying is safe precisely because of
that purity: a conflict means the aggregate moved on, so the decision is remade
against the state that actually won.

`MAX_ATTEMPTS` is 5. Enough to clear ordinary contention, few enough that a
genuinely hot aggregate surfaces as a retryable error instead of a hung request.

### Which one to call

Almost never either, directly. `TenantDb::execute` wraps `execute` and draws its
connection from the tenant's budget, and that is the normal path.

Reach for `try_execute` when the command writes something else in the same
transaction. The retry loop then has to be yours, because the transaction has to
come from wherever the connection budget is. From
[`modules/sales/src/commands.rs:180`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/commands.rs):

```rust
for _ in 1..=MAX_ATTEMPTS {
    let mut tx = db.begin().await?;
    match issue_in(&mut tx, id, &entry_id, draft, &memo, metadata).await {
        Ok(numbered) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            return Ok(numbered);
        }
        Err(e) if e.is_conflict() => { tx.rollback().await?; }
        Err(e) => { tx.rollback().await?; return Err(e.into()); }
    }
}
Err(contended(id))
```

Your obligation with `try_execute` is to commit on `Ok` and roll back on `Err`.
Forgetting to commit loses the command, and there is no ordering in which part of
it survives.

### What comes back

```rust
pub struct Committed<E> {
    pub events: Vec<E>,
    pub at: Option<LogPosition>,   // position of the first event; None if none were written
    pub version: Sequence,         // the aggregate's version afterwards
    pub effects_enqueued: usize,
}
impl<E> Committed<E> { pub fn did_nothing(&self) -> bool; }
```

`version` is returned even when nothing happened, because "nothing happened, and
here is where you are" is a useful answer. It is what an `ETag` would carry and
what the next `If-Match` would be checked against.

`effects_enqueued` can be lower than the number promised, which is a pinned
idempotency key deduplicating. That is the mechanism working, not a failure.

```rust
pub enum ExecuteError<E> { /* Rejected(E), Contended { .. }, Load, Append, … */ }
impl<E> ExecuteError<E> { pub const fn is_conflict(&self) -> bool; }
```

`is_conflict` is true only for a lost optimistic-concurrency race. A rejection is
the aggregate's rules saying no, and re-asking gets the same answer.

## Appending directly

```rust
pub struct NewEvent { … }
impl NewEvent {
    pub fn new(event_name: EventName, schema_version: SchemaVersion,
               payload: serde_json::Value) -> Self;
}

pub async fn append(
    conn: &mut PgConnection, stream: &StreamId, expected: Sequence,
    events: &[NewEvent], metadata: &Metadata,
) -> Result<Vec<Envelope>, AppendError>;

pub async fn append_events<A: Aggregate>(
    conn: &mut PgConnection, id: &AggregateId, expected: Sequence,
    events: &[A::Event], metadata: &Metadata,
) -> Result<Vec<Envelope>, AppendError>;
```

`expected` is the version the caller believes the aggregate is at. Events are
written at `expected + 1 ..= expected + n`, and the unique constraint on
`(stream, sequence)` rejects the batch if anybody got there first. That is the
optimistic-concurrency check, enforced by the database and not by a read-then-write
race.

**Append as late in the transaction as possible.** Positions come from the
counter row, whose lock is taken here and released when your transaction ends.
Everything you do after the append is time every other writer in this tenant
spends waiting.

`append_events` is the typed version, for callers who own their transaction
because they are writing something else alongside. It returns the stored
envelopes, which is how the caller learns the positions its effects are keyed on.

## Reading

```rust
pub async fn read_since(conn: &mut PgConnection, after: LogPosition, limit: i64)
    -> Result<Vec<Envelope>, ReadError>;

pub async fn read_stream(conn: &mut PgConnection, stream: &StreamId)
    -> Result<Vec<Envelope>, ReadError>;

pub async fn read_stream_since(conn: &mut PgConnection, stream: &StreamId, after: Sequence)
    -> Result<Vec<Envelope>, ReadError>;
```

`read_since` is the tailer's read, and L1 is what makes it safe: the result is
always an unbroken prefix of everything committed, so a caller can advance its
checkpoint to the last position it sees.

```rust
pub struct Envelope {
    pub position: LogPosition,
    pub stream: StreamId,
    pub sequence: Sequence,
    pub event_name: EventName,
    pub schema_version: SchemaVersion,
    pub payload: serde_json::Value,
    pub metadata: Metadata,
    pub occurred_at: Timestamp,
}
```

`occurred_at` is when the append committed, and it is **the only clock a
projection may read**. Calling `now()` inside a projector makes replay
non-reproducible, which is L2.

```rust
pub struct Metadata {
    pub actor: Option<String>,          // None for workers, reapers, provisioning
    pub on_behalf_of: Option<String>,   // platform staff acting for a tenant
    pub correlation_id: Option<String>, // ties one request's events together
    pub config_version: Option<i64>,    // L5
    pub rule_version: Option<i64>,      // L5
}
```

Both parties are recorded on an impersonated action, so it is never
indistinguishable from a tenant's own.

`config_version` and `rule_version` are what make L5 checkable. An event records
the outcome a command decided and names the configuration that decided it, so
configuration stays freely editable while replay stays reproducible, and "why was
this 10% and not 15%" is still answerable a year later.

### Integrity

```rust
pub struct Integrity { … }
impl Integrity { pub const fn is_contiguous(&self) -> bool; }

pub async fn integrity(conn: &mut PgConnection) -> Result<Integrity, ReadError>;
```

Checks L1 against the stored data: `event_count == highest_position`, and the
counter agrees. Any deviation means a position was burned, a row was deleted
despite the append-only trigger, or the counter was tampered with. Cheap enough
to run continuously per tenant, and it is the alarm that says replay can no
longer be trusted.

## Upcasters

A stored event can never be migrated. The log is append-only, so the bytes
written in 2026 are the bytes read in 2030. What moves forward is the
interpretation.

```rust
pub type UpcastStep = fn(serde_json::Value) -> Result<serde_json::Value, String>;

pub struct Upcasters { … }
impl Upcasters {
    pub fn new() -> Self;
    pub fn declare(self, event_name: &EventName, current: SchemaVersion) -> Self;
    pub fn step(self, event_name: &EventName, from: SchemaVersion, step: UpcastStep) -> Self;
    pub fn also(self, other: &Self) -> Self;
    pub fn gaps(&self) -> Vec<String>;
    pub fn current_version(&self, event_name: &EventName) -> Option<SchemaVersion>;
    pub fn upcast(&self, event_name: &EventName, stored: SchemaVersion,
                  payload: serde_json::Value) -> Result<serde_json::Value, UpcastError>;
    pub fn decode<E: DeserializeOwned>(&self, event_name: &EventName,
                  stored: SchemaVersion, payload: serde_json::Value) -> Result<E, UpcastError>;
}
```

`UpcastStep` is a plain function pointer and not a closure, so a registry is
buildable in a `const` or `static` context and an upcaster cannot accidentally
capture mutable state. Captured state would make it non-deterministic and break
replay.

### Why a chain and not one function per version

Adding a fourth version otherwise means writing `v1→v4`, `v2→v4` and `v3→v4` and
getting all three right. With a chain it means writing `v3→v4`, and the older
paths keep working because they compose through it. The number of things that can
be wrong grows linearly instead of quadratically.

### Events from the future are refused

An event whose version is newer than this build understands is refused and not
guessed at (L6). That happens during a rolling deploy if a new pod writes v3
while an old pod is still serving, which is why the deploy order is fixed:

1. Deploy the build that can **read** v3 but still writes v2.
2. Deploy the build that **writes** v3.

Two deploys, the same as every other expand-and-contract change in the system.
Doing it in one is how a rollback becomes unreadable data.

### also, and why an extending module needs it

`tax_sa` builds a ZATCA document from `sales.invoice.issued`, so its projection
group must decode an event it did not define. Declaring those names again would
be a second copy of sales' history, wrong the day sales adds a version, and wrong
*silently*, because two copies disagreeing looks exactly like an event from the
future.

```rust
// modules/tax_sa/src/lib.rs
Upcasters::new()
    .declare(&name("tax_sa.return.filed"), VERSION_1)
    .also(sales::upcasters())
    .also(purchases::upcasters())
```

### gaps

```rust
pub fn gaps(&self) -> Vec<String>;
```

Every missing step in every chain, from version 1 to each declared current. Run
it at startup. A missing step means some event already in some tenant's log
cannot be read, and finding that out during a replay months later is the worst
possible time.

## Gapless document numbers

An invoice number with a hole in it is a compliance problem, so this cannot be a
sequence.

```rust
pub async fn reserve(conn: &mut PgConnection, series: &str) -> Result<i64, NumberingError>;
pub async fn consume(conn: &mut PgConnection, series: &str) -> Result<(), NumberingError>;
pub async fn peek(conn: &mut PgConnection, series: &str) -> Result<i64, NumberingError>;
pub async fn start_at(conn: &mut PgConnection, series: &str, next: i64)
    -> Result<i64, NumberingError>;
```

Two steps, in the transaction that writes the document:

```rust
let number = numbering::reserve(&mut tx, "sales.invoice").await?;    // locks the series
let committed = try_execute::<Invoice, _, _>(&mut tx, id, …).await?; // decides
if committed.at.is_some() {
    numbering::consume(&mut tx, "sales.invoice").await?;             // it was used
}
```

### Why two calls and not one

Because the document might not be written. A create is idempotent on the key the
client sent in `Idempotency-Key`, so re-sending the same request is a no-op, and
a single `nextval`-shaped call would burn a number on every retry. A retried request is
the normal case for a client that timed out, so burning one there would put gaps
in the sequence of a business that did nothing wrong.

`reserve` takes the row lock without moving the counter. `consume` moves it.
Between them nobody else can be issuing in this series, so the number `reserve`
returned is still the one `consume` hands out. The series is created at 1 the
first time it is asked for, so a tenant needs no setup step and a module added
later needs no migration.

**Reserving and then not consuming is safe. Writing a document and then not
consuming is not**, because the next document gets the same number. That pairing
is the one thing this module cannot enforce from here, which is why
`re_issuing_does_not_move_the_series` in `modules/sales/tests/sales.rs` tests it
directly.

`peek` is for a settings screen and for tests. Never for issuing: reading a
counter you do not hold the lock on tells you where it was, not where it is.

`start_at` is for a business arriving from another system that reached invoice
4,107 and must not start again at one. It refuses to move a series backwards,
which would reissue numbers that are already on documents somebody holds.

## Configuration

```rust
pub struct Configured<T> { /* value, generation */ }

pub async fn get<T: DeserializeOwned>(conn: &mut PgConnection, key: &str)
    -> Result<Option<Configured<T>>, ConfigError>;

pub async fn set<T: Serialize>(conn: &mut PgConnection, key: &str,
    value: &T, set_by: Option<&str>) -> Result<i64, ConfigError>;

pub async fn version(conn: &mut PgConnection) -> Result<i64, ConfigError>;
```

Reachable as `erp_eventlog::configuration::{get, set, version}`.

`get` returns `None` when the tenant has never set the key, which is the normal
case. Every caller pairs it with a shipped default. "Most tenants never open the
settings" is the requirement, not a shortcut.

`set` takes `&T` and not raw JSON, so the only way into this table is through the
type that gives the value meaning, and a reader's decode cannot be the first
thing to notice a mistake.

This is not a settings bag anything may write to. The mechanism is key-value; the
surface is typed, one endpoint per thing a tenant can configure. A generic "set
any key to any JSON" endpoint would make every reader's validation the only thing
standing between a typo and a broken module.

`version` is the generation of the tenant's configuration as a whole, and it is
what goes into `Metadata::config_version`. It answers "what was configured when
this command decided?" so the question can be asked later without the
configuration ever being *read* later. Zero when nothing is configured, which is
a real answer.

## Sealed secrets

For the things a module must keep and must not reveal. One user so far: ZATCA
onboarding hands a tenant an ECDSA private key and a CSID secret, neither derived
from anything, both surviving a projection rebuild, both rotatable, and neither
readable by everything that can read the tenant.

```rust
pub struct SealingKey { … }   // Debug shows the id and the length, never the bytes

impl SealingKey {
    pub fn new(id: impl Into<String>, bytes: &[u8]) -> Result<Self, SecretError>;
    pub fn parse(configured: &str) -> Result<Self, SecretError>;   // "<id>:<64 hex>"
    pub fn generate(id: impl Into<String>) -> Result<Self, SecretError>;
    pub fn id(&self) -> &str;
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecretError>;
    pub fn unseal(&self, key: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError>;
}

pub async fn put(conn: &mut PgConnection, sealing: &SealingKey,
                 key: &str, plaintext: &[u8]) -> Result<(), SecretError>;
pub async fn get(conn: &mut PgConnection, sealing: &SealingKey, key: &str)
    -> Result<Option<Vec<u8>>, SecretError>;
pub async fn exists(conn: &mut PgConnection, key: &str) -> Result<bool, SecretError>;
pub async fn forget(conn: &mut PgConnection, key: &str) -> Result<(), SecretError>;
```

AES-256-GCM through OpenSSL, which the workspace already links for Postgres TLS.
The nonce is 12 random bytes and lives in the ciphertext, so a row is
self-describing and there is no second column to fall out of step with the first.

`parse` takes the id and the key in one string on purpose. A rotation means two
keys existing at once, and a deployment that carries them separately has two
things to keep in step.

`unseal` never guesses. GCM authenticates, so a tampered value fails instead of
decrypting to something.

`get` returns `Result<Option<_>>` because "there is none" and "there is one that
will not unseal" are different answers and the caller has to tell them apart.

`exists` is for a status endpoint: "is this tenant onboarded?" must be answerable
by something that is not allowed to read the key.

**What this does not protect against** is somebody who has the sealing key *and*
the database. That is the trade. It turns "a leaked backup exposes every tenant's
signing key" into "a leaked backup is useless without the deployment's
environment", which is the difference worth having. Moving the key into an HSM or
KMS is the next step up and changes only `SealingKey`.

## The outbox

A command decides what should happen; it never makes it happen. The decision is
written to the outbox in the same transaction as the events that justify it, and
a dispatcher delivers it afterwards.

```text
  command transaction                    later, separately
  ┌──────────────────────────┐           ┌────────────────────────┐
  │ append events            │           │ claim (lease)          │
  │ enqueue effects          │  commit   │ deliver  ← the only    │
  └──────────────────────────┘  ───────► │ settle     I/O anywhere│
        both or neither                  └────────────────────────┘
```

**No effect without its cause.** A rolled-back command sends nothing, because the
promise rolled back with it.

**No cause without its effect.** A crash after commit loses nothing. The promise
is durable and the dispatcher finds it on restart.

**Rebuilds are safe.** Effects are written by commands and not derived by
projections, so rebuilding a read model re-sends nothing. That is what makes
`replay_shadow` something you can run in production.

**Testable domains.** A command handler returns values, so asserting "this would
have emailed the customer" needs no mock and no network.

### Effect

```rust
pub struct Effect { … }

impl Effect {
    pub fn new(kind: EffectKind, payload: serde_json::Value) -> Self;
    pub fn with_key(kind: EffectKind, key: impl Into<String>,
                    payload: serde_json::Value) -> Self;
    pub const fn kind(&self) -> &EffectKind;
    pub const fn payload(&self) -> &serde_json::Value;
    pub fn key(&self) -> Option<&str>;
}
```

`new` derives the idempotency key from the cause: `{position}:{index}`, unique
without any coordination because log positions are. That makes *delivery*
idempotent, so the dispatcher can retry freely. It does not make *execution*
idempotent: running the same command twice appends at two positions and so
promises twice.

`with_key` is for when the intention is what must not repeat. One welcome email
per account, whether or not signup was retried. Derive the key from the thing
being deduplicated, never from a clock or a random value.

```rust
pub async fn enqueue(conn: &mut PgConnection, cause: Option<LogPosition>,
                     effects: &[Effect]) -> Result<usize, EnqueueError>;
```

Call this inside the transaction that appends the events, which is the entire
point. `execute` does it for you. `cause` is the log position of the first event
the command wrote, and `None` is for callers that appended nothing, in which case
every effect must pin its own key. The return is how many rows were actually
inserted, lower than `effects.len()` when a pinned key was already there.

### Handling an effect

```rust
pub trait EffectHandler: Send + Sync {
    fn kind(&self) -> EffectKind;
    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError>;
}

pub enum DeliveryError {
    Retryable(String),   // a timeout, a 5xx, a refused connection
    Permanent(String),   // a 400, a malformed payload, an address that does not exist
}
```

Implementations live in modules and not in the kernel (D11). The kernel knows
that effects exist and how to deliver them reliably, never what any of them mean.

The distinction in `DeliveryError` is yours to make and it matters. Retrying a
permanent failure wastes attempts and delays the dead-letter signal that tells an
operator something needs looking at.

```rust
pub struct PendingEffect {
    pub id: i64,
    pub kind: EffectKind,
    pub payload: serde_json::Value,
    pub idempotency_key: String,   // stable across retries. Pass it downstream
    pub attempts: i32,             // 1 on the first delivery
    pub caused_by: Option<LogPosition>,
}
impl PendingEffect {
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error>;
}
```

### Dispatcher

```rust
pub struct RetryPolicy {
    pub max_attempts: i32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub lease: Duration,
}
impl RetryPolicy { pub fn backoff(&self, attempts: i32) -> Duration; }

pub struct Dispatcher { … }
impl Dispatcher {
    pub fn new(policy: RetryPolicy) -> Self;
    pub fn register(self, handler: Arc<dyn EffectHandler>) -> Self;
    pub fn kinds(&self) -> Vec<EffectKind>;
    pub const fn policy(&self) -> &RetryPolicy;

    pub async fn dispatch_once(&self, pool: &PgPool, limit: i64)
        -> Result<Dispatched, DispatchError>;
    pub async fn dispatch_until_idle(&self, pool: &PgPool, limit: i64)
        -> Result<Dispatched, DispatchError>;

    pub async fn claim(&self, conn: &mut PgConnection, limit: i64)
        -> Result<Vec<PendingEffect>, DispatchError>;
    pub async fn deliver(&self, effect: &PendingEffect) -> Settlement;
    pub async fn settle(&self, conn: &mut PgConnection, effect: &PendingEffect,
                        settlement: &Settlement) -> Result<(), DispatchError>;
}

pub enum Settlement {
    Delivered,
    Retrying { delay: Duration, error: String },
    Dead { error: String },
    Abandoned,     // no handler registered; nothing recorded, the lease lapses
}
```

`lease` must comfortably exceed the slowest handler's timeout. Too short and a
slow delivery is duplicated while the first is still in flight; too long and a
crashed dispatcher's work sits idle. Handler timeout times three is a reasonable
starting point.

**The one rule: no database transaction is ever open while a handler runs.**
Delivery is network I/O with a timeout measured in seconds, and holding a
transaction across it would pin a connection, hold row locks, and keep `xmin`
back so autovacuum cannot clean up behind it. So a delivery is three separate
steps: claim in a short transaction, deliver outside one, settle in another.

That is why `deliver` never fails. A delivery that went wrong is a `Settlement`
and not an error, because "it failed" is information the outbox has to record.
And it is why `Settlement` is a value and not a `Result`: the decision to retry
or give up is a pure function of the policy and the failure, testable without a
database.

`dispatch_once` and `dispatch_until_idle` are conveniences for a caller holding a
pool. A worker uses the three steps directly, so each database moment draws a
metered connection from `TenantDb` and none is held across the delivery.

`dispatch_until_idle` stops when nothing more is **due**, which is not the same
as the outbox being empty. An effect that failed and is backing off is not due,
so this returns while it is still owed. That is also why it terminates.

### At-least-once, and where it is fixed

Claiming and settling are separate commits, so a dispatcher that dies after
delivering but before settling leaves an effect that was performed and not
recorded as such. The lease lapses and it is delivered again.

Delivery is therefore at least once, and that is not fixable here. It is the
two-generals problem between this process and whatever it is calling. It is fixed
one level down: every `PendingEffect` carries a stable `idempotency_key`, and a
handler that passes it to the downstream API makes the second delivery a no-op on
the far side. A handler that ignores it is the thing that sends two emails, not
this loop.

### Concurrency, and unhandled kinds

One pass delivers its batch sequentially. Concurrency comes from running more
dispatchers, which is safe by construction: the claim uses `FOR UPDATE SKIP
LOCKED`, so two dispatchers never see the same row, and
`two_dispatchers_never_deliver_the_same_effect` proves it.

The claim filters on the kinds this dispatcher knows, so a worker deployed
without some module's handler leaves those rows for a worker that has it. The
alternative, claim and fail and back off, would burn attempts and dead-letter
effects for a deploy that was merely in progress.

### Health

```rust
pub struct OutboxHealth { … }
impl OutboxHealth { pub fn is_healthy(&self, max_backlog_age_seconds: i64) -> bool; }

pub async fn outbox_health(conn: &mut PgConnection) -> Result<OutboxHealth, sqlx::Error>;
```

Unresolved dead letters and backlog age. Both are continuously asserted per
tenant.
