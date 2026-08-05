# Architecture

Specification for the SPA multi-tenant ERP backend.

This document states **decisions**, **laws**, and **contracts**. It is the thing to
read before writing code and the thing to update when a decision changes. The
rationale — why these choices and not others, plus the review of the prototype
that motivated them — lives in the review artifact and is deliberately not
repeated here.

Status: **Rev 1**, superseding the `prototype` line at `f2e8acd`.

---

## 1. Decisions

Each decision is numbered, has a status, and states what it forecloses. Changing
one means changing this document first.

| # | Decision | Status |
|---|---|---|
| D1 | One database per tenant, orchestrated by a control-plane core database | Accepted |
| D2 | Control plane: normalized tables + audit for identity/membership/entitlement; event-sourced for workflows | Accepted |
| D3 | Tenant plane: event-sourced write model, projected read models | Accepted |
| D4 | Postgres is the log *and* the internal transport. No Kafka, no `pg_notify` internally | Accepted |
| D5 | Projections are pure functions of the event stream; replay is bit-reproducible | Accepted |
| D6 | Configuration is resolved at command time and frozen into events | Accepted |
| D7 | Modules are compiled in, enabled per tenant at runtime | Accepted |
| D8 | Blueprints (versioned command scripts) are the single mechanism for charts of accounts, demo seeds, and presets | Accepted |
| D9 | Effects are values written to an outbox, never inline I/O | Accepted |
| D10 | Money carries its currency at runtime; no `Add` impl | Accepted — revises an earlier proposal, see §1.10 |

### 1.1 One database per tenant (D1)

A tenant's data lives in its own Postgres database. The control plane holds the
`tenant → (cluster, database)` map.

**What it forecloses:** cross-tenant SQL. Platform-wide questions ("how many
tenants use invoicing") are answered from rollups tenants push to the control
plane via their outbox, not from joins.

**What it buys:** cross-tenant access becomes unrepresentable rather than
prevented. The only route to tenant data is a `TenantDb` handle (§5.1), which
can only be obtained by resolving a tenant through the control plane. There is
no ambient pool. A query that reads another tenant's ledger cannot be written,
because no connection is in scope that could serve it.

**The cost to manage:** connections. Postgres spends a process per connection,
so pools must be `min = 0`, held in an LRU with idle eviction, and workers must
lease *tenants with pending work* rather than running a loop per tenant.
Connection count tracks concurrency, not tenant count. This is load-bearing and
is soak-tested before anything is built on it (§7).

### 1.2 Control plane persistence (D2)

Identity, membership, tenant registry and entitlements are **normalized tables
with an audit trail**. They are read on every request, are highly relational,
and must be queryable for cross-tenant reporting.

Provisioning, module enablement, and blueprint installation are
**event-sourced** — they are multi-step, resumable, and genuinely benefit from a
log.

Two persistence styles is a real cost, accepted deliberately: the alternative is
either a projection on the hot path of every request, or provisioning state that
cannot resume after a crash.

### 1.3 Postgres as log and transport (D4)

Projections tail the tenant's own `events` table by position. No message broker
participates in internal delivery.

`pg_notify` is **not used** — it does not survive PgBouncer transaction pooling,
and the fallback poll it would optimize is cheap enough that the dependency
isn't worth it. Wake-ups are: poll on a short interval when a tenant has recent
activity, back off when idle.

Kafka, if it appears, is an *outbound export* for consumers outside the system,
fed from the outbox. Nothing internal ever consumes it.

### 1.4 Reproducible replay (D5)

See §3 for the laws. The short form: replaying the log from zero into an empty
database produces state identical to live at the same position, including across
related tables.

### 1.5 Configuration frozen into events (D6)

See Law L5. Commands read configuration; events record the resolution;
projections never read configuration.

### 1.6 Modules compiled in (D7)

Every module is a crate implementing `Module` (§5.4), compiled into the binary.
Entitlements decide which are active per tenant. Rust's dynamic-plugin story
(`dlopen`, unstable ABI, `abi_stable` at every boundary) would trade away the
type-level guarantees the rest of this design is built on.

Designed for roughly 15 modules: thin trait, generated registry, minimal
per-module boilerplate. Degrades gracefully if the number is 5 or 30.

Third-party extension, if needed later, is a WASM sandbox or webhooks. Both
additive. Neither before this system has carried our own modules for a year.

### 1.7 Blueprints (D8)

A blueprint is a versioned, parameterized list of **commands** — never rows.
Charts of accounts, demo seeds, rule packs and module presets are all blueprints.
Steps being commands means everything a blueprint produces is provably reachable
through the real domain, and a broken blueprint fails at build time rather than
in front of a customer.

### 1.8 Effects as values (D9)

No domain code performs I/O. A command returns events *and* effects; both are
written in one transaction; a dispatcher delivers afterwards with retries and
idempotency keys.

### 1.9 Identity, membership, profile

Three separate concepts:

- **Identity** — authenticates. Credentials, sessions, MFA. Control plane.
- **Membership** — `(identity, scope, role)`. The right to *enter* a scope.
  Control plane.
- **Profile** — the party: employee, client, supplier. Tenant DB (control plane
  for platform staff).

An employee is a profile that *usually* has an identity. Offboarding revokes the
identity and keeps the employee record, because the ledger references it forever.
A client is a profile that usually has none. One identity may have many profiles.

The identity → profile link crosses a database boundary, so no foreign key. The
tenant DB stores `IdentityId` plus a denormalized display name refreshed by a
control-plane outbox event.

**There is no `is_system` boolean.** Platform capability is explicit and scoped.
Support access to a tenant is **impersonation**: first-class, logged, time-boxed,
visible to the tenant, and attributed to both parties.

### 1.10 Money (D10) — revising an earlier proposal

An earlier draft proposed `Money<C: Currency>` with a phantom currency type. That
is wrong for this system: currencies are **tenant configuration**, selected at
runtime, so they cannot be type parameters.

The design that actually holds:

```rust
pub struct Money { minor: i64, currency: CurrencyCode }
```

with **no `Add`/`Sub`/`Sum` implementations**. Arithmetic is only available as
`checked_add(self, rhs) -> Result<Money, MoneyError>`, which fails on currency
mismatch and on overflow. The type system still forces the mismatch to be
handled — you cannot write `a + b` — while currency stays runtime data.

`CurrencyCode` carries its ISO-4217 minor-unit exponent (JPY = 0, SAR/USD = 2,
KWD/BHD = 3), so formatting and rounding are not caller guesswork.

---

## 2. Topology

```
                 CONTROL PLANE                                TENANT PLANE
        ┌──────────────────────────────┐                ┌────────────────────┐
        │ identities · authenticators  │                │ tenant_a           │
        │ memberships                  │   TenantDb     │  events (gapless)  │
        │ tenant registry              │  ──────────▶   │  proj_<group>.*    │
        │ entitlements · config        │                │  outbox            │
        │ blueprints · billing         │                │  config_versions   │
        │ workflow event log + outbox  │                └────────────────────┘
        └──────────────────────────────┘                ┌────────────────────┐
                                                        │ tenant_b …         │
   WORKERS: connections scale with concurrent work,     └────────────────────┘
   not tenant count.
     projection scheduler · outbox dispatcher
     fleet migrator · provisioner
```

Many databases per Postgres cluster; few clusters. The registry carries cluster
identity from day one so promoting a tenant to dedicated hardware is a row change
plus a migration, not an architecture change.

### Which plane answers what

| Question | Plane | Hot-path cost |
|---|---|---|
| Who is this? | Control — session → identity | 1 cache read |
| May they enter this tenant? | Control — membership | cached, in-process |
| Which modules and settings apply? | Control — entitlements + config | cached, in `TenantDb` |
| What may they do *here*? | Tenant — grants + conditions | cached projection read |

No request joins across planes.

---

## 3. Laws

These are invariants, not guidelines. Each has a corresponding test or continuous
assertion (§7). A change that violates one is a bug regardless of what it enables.

### L1 — The event log is gapless and commit-ordered

Positions within a tenant are contiguous, and position order equals commit order.

`BIGINT GENERATED ALWAYS AS IDENTITY` assigns at *insert* time, not commit time:
two concurrent transactions can take 100 and 101 with 101 committing first,
permanently stranding 100 from a tailer reading `WHERE position > checkpoint`.
This is rare under light load, certain under contention, and silently breaks
replay because live and replay observe different sets.

**Mechanism:** a transaction-scoped advisory lock per tenant around the append.
Writes within a tenant are already largely serialized by aggregate contention, so
the cost is negligible. The lock must be transaction-scoped (`pg_advisory_xact_lock`)
to survive PgBouncer.

**Asserted by:** a concurrent-append test, plus a continuous contiguity check.

### L2 — Projections are pure functions of the event stream

A projector's only inputs are the event, the position, the event timestamp, and
its own group's tables.

**Mechanism:** `ProjectionCtx` (§5.3) exposes `position()`, `event_time()`,
`tenant()`, and `derive_id(salt)`. It holds no pool, no clock, no RNG, no HTTP
client. There is nothing in scope to be impure with.

Forbidden inside a projector: `Utc::now()`, random ids, reading configuration,
reading another group's tables, any network call, `HashMap` iteration where order
reaches a write.

### L3 — Projection groups are the unit of consistency

Tables that must agree belong to one group: one checkpoint, one transaction, a
declared order within the group. Across groups: **no reads, ever.**

**Mechanism:** each group owns a Postgres schema; the projection transaction sets
`search_path` to that schema alone. A cross-group query fails with "relation does
not exist" on first execution. Reaching across requires a schema-qualified name,
which is greppable and banned in review.

Group by *what must agree*, not by *what is related*.

### L4 — Checkpoints advance in the same transaction as their effects

```sql
BEGIN;
  SELECT position FROM _checkpoint WHERE grp = $1 FOR UPDATE;  -- also the lease
  -- apply the batch
  UPDATE _checkpoint SET position = $2 WHERE grp = $1;
COMMIT;
```

Crash recovery replays exactly the events whose effects were lost. There is no
separate dedup table, and no second position space to confuse with the first.

### L5 — Events carry outcomes, never references to mutable configuration

A command reads configuration and rules, decides, and emits an event carrying the
**resolved outcome** plus the `config_version` and `rule@version` that produced
it. A projection consumes only the event.

This is what makes D5 and D6 compatible. Without it the system is configurable or
reproducible, not both.

Corollary: configuration keys that influence command outcomes
(`affects_events: true`) are append-only versioned. Keys that only affect display
may be mutated in place, because display config is applied at *query* time and
never touches stored state.

### L6 — Failures stop; they never degrade

No swallowed errors, no "log a warning and continue with the feature disabled",
no advancing a checkpoint past an event that did not apply.

In a system of record, a loud failure costs an incident; a quiet one costs an
audit.

### L7 — Aggregates are loadable only inside command handling

Reads are served by projections. Event sourcing is a write model and a rebuild
mechanism, not a query engine.

### L8 — Every mutation is idempotent under retry

Enforced at the edge by a required `Idempotency-Key`, and in projections by L4.

---

## 4. Type-system posture

Ordered by cost-to-benefit. The first three are close to free and should be
pervasive from the first commit.

**Newtypes for every identifier and quantity.** The prototype had a live defect
from `sequence` (per-aggregate) and `id` (global position) both being `u64`.
Distinct types make that a compile error.

**`Money` with no `Add`.** §1.10.

**Proof-carrying constructors.** A type whose only constructor validates, with
`#[serde(try_from = ...)]` so the proof survives the log boundary:

```rust
#[derive(Serialize, Deserialize)]
#[serde(try_from = "Vec<JournalLine>")]
pub struct BalancedLines(NonEmpty<JournalLine>);
```

An event replayed from storage is re-validated on the way in, so a bad migration
surfaces as a decode error rather than a quietly unbalanced trial balance.

**Capability tokens.** `Permit<C: Capability>` is minted only by the authorizer
and demanded by domain functions, so "did we authorize this?" is a signature
requirement rather than a review finding. Composes with `ModuleEnabled<M>`.

**Typestate where states have consequences** — journal entries, fiscal periods,
documents. Rehydration takes one runtime check at the boundary; everything
downstream is compile-time. Not applied to aggregates without meaningful states.

**Where to stop:** session types, const-generic dimensional analysis, and
type-level rule evaluation are past the point of paying for themselves.

---

## 5. Contracts

### 5.1 `TenantDb`

```rust
pub struct TenantDb {
    tenant:  TenantId,
    pool:    PgPool,           // this tenant's database, nobody else's
    modules: EnabledModules,
    config:  ConfigSnapshot,   // resolved once per request
    schema:  SchemaVersion,
}
```

No public constructor. Obtained only via `ControlPlane::enter(auth, tenant)`,
which checks membership. Every domain function takes `&TenantDb` or a transaction
derived from it.

### 5.2 Event log

```rust
pub struct StreamId { pub domain: DomainName, pub id: AggregateId }

pub struct Envelope {
    pub position:       LogPosition,   // global within tenant, gapless (L1)
    pub stream:         StreamId,
    pub sequence:       Sequence,      // per-aggregate — a DIFFERENT type
    pub event_name:     EventName,
    pub schema_version: SchemaVersion,
    pub payload:        serde_json::Value,
    pub metadata:       Metadata,
    pub recorded_at:    Timestamp,
}
```

### 5.3 Projections

```rust
pub trait ProjectionGroup {
    const NAME: &'static str;
    const SCHEMA: &'static str;   // its own Postgres schema (L3)
}

pub struct ProjectionCtx { /* position, event_time, tenant — nothing else */ }
impl ProjectionCtx {
    pub fn position(&self) -> LogPosition;
    pub fn event_time(&self) -> Timestamp;
    pub fn tenant(&self) -> TenantId;
    pub fn derive_id(&self, salt: &str) -> Uuid;   // v5 over (position, salt)
}

pub trait Projection: Send + Sync {
    type Group: ProjectionGroup;
    fn apply(&self, ctx: &ProjectionCtx, e: &Envelope, tx: &mut GroupTx<Self::Group>)
        -> Result<(), ProjectionError>;
}
```

### 5.4 Modules

```rust
pub trait Module: Send + Sync + 'static {
    const ID: ModuleId;
    fn depends_on(&self)   -> &[ModuleId];
    fn migrations(&self)   -> &[Migration];
    fn config_keys(&self)  -> Vec<ConfigDecl>;
    fn facts(&self)        -> Vec<FactDecl>;
    fn capabilities(&self) -> Vec<CapabilityDecl>;
    fn projections(&self)  -> Vec<Box<dyn AnyProjection>>;
    fn routes(&self)       -> Router<TenantDb>;
    fn blueprints(&self)   -> &[Blueprint];
}
```

One manifest per module. There is no way to ship a module that forgot to register
its fact vocabulary, because registration and routing come from the same object.

**Enable** — check dependencies, migrate that tenant's database, register
projections, mount routes, optionally install blueprints. A durable workflow.
**Disable** — unmount, stop projections, mint no more tokens. **Never drop
tables.** Storage is reclaimed only on explicit deletion, after an export.

### 5.5 Configuration

```rust
pub struct ConfigDecl {
    pub key:             ConfigKey,
    pub kind:            FactKind,      // reuses the rule vocabulary
    pub default:         ConfigValue,
    pub scopes:          &'static [Layer],
    pub tenant_editable: bool,          // false = ours to set, not theirs
    pub affects_events:  bool,          // true ⇒ append-only versioned (L5)
    pub description:     &'static str,
}

pub enum Layer { Platform, Plan, Tenant, Branch }   // most specific wins
```

Resolution returns the value *and* its layer, so a tenant sees "your plan sets
this" rather than an unexplained failure.

### 5.6 Rules

```rust
pub struct Rule<E> {
    pub id: RuleId,
    pub version: Version,      // events cite this (L5)
    pub when: DynCondition,    // validated against FactRegistry at authoring time
    pub then: E,
    pub priority: i32,
    pub effective: DateRange,
    pub origin: RuleOrigin,    // Preset | Form(TemplateId) | Builder | Raw
}
```

`Rule<Scale>` is a permission. `Rule<DiscountValue>` is pricing.
`Rule<Vec<LineTemplate>>` is a posting rule. `Rule<ApprovalChain>` is routing.
One evaluator, one validator, one `explain`.

Four authoring levels — presets, forms, builder, raw JSON — all producing the
same artifact. `origin` lets a form-authored rule render back as its form.

### 5.7 API

| Concern | Contract |
|---|---|
| Errors | RFC 9457 `application/problem+json`, stable machine-readable `type` |
| Spec | OpenAPI generated from handlers; drift fails CI |
| Pagination | Opaque cursors, always |
| Concurrency | `ETag` = aggregate version, `If-Match`, `412` on mismatch |
| Idempotency | `Idempotency-Key` required on every mutation |
| Versioning | `/v1/`, additive-only within a version |
| Consistency | Writes return `version` + `log_position`; reads accept `?consistent_after=` |

---

## 6. Workspace

```
crates/
  spa-types       (no deps)    newtypes, Money, NonEmpty — WASM-safe, frontend-shareable
  spa-rules       types        Facts, DynCondition, registries, Rule<E>, StateMachine
  spa-eventlog    types        gapless append, load, snapshot, upcasters, outbox
  spa-projection  eventlog     groups, ProjectionCtx, scheduler, leases, shadow replay
  spa-config      types,rules  declarations, layers, versioned resolution
  spa-control     eventlog,config  identities, tenants, entitlements, TenantDb, migrator
  spa-kernel      eventlog,rules,projection,config  ledger, periods, permits, Module
  spa-api         control,kernel,modules  routing, problem+json, OpenAPI, composition root
  spa-testkit     all          template-DB fixtures, generators, fault injection, differ
modules/
  <one crate each>             depend on spa-kernel only
```

Direction is the enforcement: `modules/*` depend on `spa-kernel` and never on each
other or on `spa-control`. A module physically cannot reach the control plane or
another module's tables. Cross-module interaction goes through events, which is
also what keeps L3 enforceable.

---

## 7. Verification posture

Coverage percentage is the wrong metric. What follows is organized by the property
each test protects.

**Real Postgres everywhere, never a mock.** A fresh database per test, cloned via
`CREATE DATABASE … TEMPLATE` from a migrated-and-seeded template. Measured at
**≈280 ms to acquire** on local Postgres 18 (`spa-testkit`'s own timing test
reprints this per machine). Three of the four defects found in the prototype were
invisible to any test that mocks the database and unmissable to any test that
doesn't.

| Property | Test |
|---|---|
| Ledger correctness | Property test: any sequence of valid commands leaves debits = credits per currency |
| Replay reproducibility (L2, L3, L5) | `replay --shadow` then diff every table against live |
| Log integrity (L1) | Concurrent-append test asserting contiguous, commit-ordered positions |
| Idempotency (L8) | Command applied twice ≡ once; batch replayed ≡ applied once |
| Crash safety | Fault injection at every transaction boundary; assert no partial state and that resume completes |
| Shutdown safety | SIGTERM mid-batch; assert drain completes and nothing is lost |
| Old data still reads | Golden files of real event JSON per schema version, decoded every build |
| Migration equivalence | Schema migrated from v(N−1) must equal one built fresh at vN |
| Tenant isolation (D1) | Two provisioned tenants; assert no code path reaches across |
| Authorization | Matrix over (role × capability × facts); deny-by-default for unlisted pairs |
| Module composition | Demo build with every module enabled, as a required CI check |
| Blueprint validity | Every shipped blueprint previewed against a fresh tenant in CI |
| API contract | OpenAPI drift; generated-client round-trip; problem+json shape |
| Connection strategy | Soak at target tenant count and concurrency |

**Validation happens in four places, each with one job:**

1. **Edge** — shape and type, by parsing into typed requests.
2. **Domain** — invariants, in aggregates and smart constructors.
3. **Authoring** — rules and config validated against registries when written,
   returning every problem at once.
4. **Continuous** — the invariants below asserted per tenant, in production.

**Continuously asserted, per tenant:**

- schema version = target, per module
- projection lag < threshold, per group
- event positions contiguous (L1)
- unresolved dead letters = 0
- outbox backlog age < threshold
- **trial balance balances, per currency**

The last one holds only if commands, events, projections and replays are all
correct. It is one number that catches an entire class of pipeline bug, and it is
the single most valuable alert in the system.

---

## 8. Open questions

**The generic `Document` aggregate.** Phase 6 replaces per-document Rust
aggregates with a data-driven type. Clearly right for expense claims; clearly
wrong for journal entries. If tenants mostly need the same handful of documents
with different *fields*, a typed aggregate with a dynamic field bag keeps the
state machine compiled and gets most of the benefit. If they need genuinely
different *workflows*, the generic aggregate earns its cost. **Decide from
customer conversations before Phase 6, not from this document.**

**Module count.** Designed for ~15. If the real number turns out to be 30+,
revisit whether the registry should be macro-generated and whether third-party
extension needs to arrive earlier.
