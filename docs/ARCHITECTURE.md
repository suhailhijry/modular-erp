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
| D11 | The kernel contains **no business domain**. Accounting is a module. | Accepted — revises an earlier proposal, see §1.11 |
| D12 | Errors are message codes plus typed arguments, never sentences. Arabic is a first-class target. | Accepted — see §1.12 |
| D13 | Clusters are control-plane data; placement is by concurrently-active tenants | Accepted — see §1.13 |
| D14 | Background work claims tenants by **per-visit lease**; idle tenants are throttled by `next_visit_at` | Accepted — see §1.14 |

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

**The cost to manage:** connections. Two ceilings, measured in
`spa-control/tests/soak.rs`, not assumed:

| quantity | bounded by |
|---|---|
| connections *executing* | the lane budget (§1.1.1) |
| connections *open* | concurrently-active tenants × `max_connections_per_tenant` |

```text
connections_per_cluster ≈ concurrently_active_tenants × max_connections_per_tenant
```

**Cluster count is driven by concurrently-active tenants — not total tenants, and
not request rate.** 5,000 tenants at 25% concurrency and 2 connections each is
~2,500 connections; at ~400 per instance that is 7–8 instances, which is also
roughly what storage dictates. The two constraints agree at this scale; when they
diverge, this is the one that decides.

Pools are therefore `min_connections(0)` with a short idle timeout — that is what
drains connections from tenants that have gone quiet — and workers lease *tenants
with pending work* rather than running a loop per tenant.

#### 1.1.0 Review: is database-per-tenant still the right call?

Revisited deliberately, because it is the decision everything else rests on and
because "connections scale with tenants" is the first objection anyone raises.
Measured on the compose stack rather than argued:

| | measured |
|---|---|
| 300 concurrent requests, **1** tenant | **8** connections |
| 9 active tenants under load | 35 tenant + 48 control = **84** of 200 |
| one connection, simple `SELECT` | **12,441** queries/s |

Request rate does not enter it, because a permit is taken per database
*operation*. A connection is not a throughput bottleneck at any scale here; it is
a forked backend process, and therefore a memory cost.

**A connection cannot be reused across tenants.** Not a design choice — the
database name travels in the protocol's `StartupMessage` and there is no message
to change it afterwards. `\c` in psql opens a new backend (observable: the
`pg_backend_pid()` changes). Every pooler inherits this, PgBouncer and Supavisor
included; their pools are per `(database, user)` too.

The alternatives, costed against this codebase — 71 catalog objects per tenant,
58 module queries written with no tenant predicate:

| | database/tenant | schema/tenant | shared schema |
|---|---|---|---|
| catalog objects @ 5k | 355k over ~8 instances | 355k in **one** `pg_class` | 71 |
| connections | active tenants × pool | one pool per instance | one pool |
| fleet migration | 5,000 databases | 5,000 schemas | **one** |
| restore one tenant | `pg_dump -d` | `pg_dump -n` | realistically no |
| a cross-tenant leak needs | *unrepresentable* | a `search_path` bug | a forgotten `WHERE` |
| module queries to rewrite | 0 | 0 | **58** |

Schema-per-tenant does not reduce the catalog, it **concentrates** it; what it
buys is one pool per instance. Shared schema drops `TenantDb`'s guarantee from
"cannot be written" to "we remembered the predicate", which is a change in kind.

**Kept.** The evidence that settles it is external: Supabase gives each project
its **own database** and reaches millions of them — not by abandoning isolation
but by putting a pooler fleet outside the application, pausing dormant tenants,
and orchestrating many modest instances. That is the same model as D1 plus three
things around it, and it is a better map for this system than any of the rows
above.

What follows from that, in cost order, none of which is a data-model change:

1. **Size the lane budgets against `max_connections`.** Four processes each hold
   their own 400-permit budget with no knowledge of the others; the per-tenant
   pool cap is currently what hides it. This is the gap a pooler would close.
2. **Parallelise `survey_fleet` / `migrate_fleet`.** Sequential today, and its own
   comment concedes "a few thousand tenants is a few minutes".
3. **Pause dormant tenants.** `min_connections(0)` and `next_visit_at` throttle a
   quiet tenant; nothing stops or archives one. This is the answer to cost per
   tenant at the low end, and it is far smaller than a rewrite.

#### 1.1.0.1 Standards set now, so a pooler is configuration

Adopting Supavisor or PgBouncer later should be variables, not a rewrite. Four
things make that true, and they are in the build already:

1. **Two routes per cluster, `Write` and `Direct`.** A transaction pooler hands
   out a different backend per transaction, and `CREATE DATABASE` cannot run in
   one at all. So provisioning, fleet migration and schema rebuilds ask for
   `Role::Direct` — `PRIMARY_DIRECT_URL`, which **falls back to the primary**
   when unset. Supabase ships exactly this shape: a pooler connection string and
   a direct one.
2. **`SET LOCAL`, never `SET`, outside a DDL path.** The projection hot path was
   already transaction-scoped. `crates/spa-control/tests/pooler.rs` walks every
   `.rs` and `.sql` in the workspace and fails the build on a session-scoped
   `SET`, a session advisory lock, or a `LISTEN` outside the paths that are
   allowed one.
3. **The statement cache is a knob.** sqlx prepares by default and caches per
   connection; a transaction pooler invalidates that. `POOL_STATEMENT_CACHE=0`
   turns it off without a rebuild.
4. **Lane budgets are deployment config.** They were compiled constants, which
   is how four processes came to hold 400 permits each against a
   200-connection server. `POOL_INTERACTIVE`/`POOL_CLIENT`/`POOL_BACKGROUND`,
   and `report_budget` states the arithmetic at start-up against what the server
   actually allows. Behind a pooler the number to compare against becomes the
   pooler's client limit — which is the point of having one.

Nothing here assumes a pooler will be adopted. Every one of them is a
correctness or clarity improvement on its own, and the day one goes in front it
is a handful of environment variables.

#### 1.1.1 Lanes

A permit is taken **per database operation**, never per request. At 10,000 req/s
a request-scoped permit needs ~400 connections; an operation-scoped one needs
~120. Permits are drawn from a lane — `Interactive` (an employee at the counter),
`Client` (a tenant's customers), `Background` (projections, outbox, migrations) —
each with its own allowance, so a flood of consumer bookings cannot starve the
counter. Exhaustion returns 503 rather than queueing.

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

Three consequences worth stating, because each rules out a design that looks
equivalent:

- **Effects are written by commands, never derived by projections.** A projection
  would get exactly-once for free from L4, but projections are rebuildable — and
  a rebuild would re-derive every effect and re-send years of email. Command-time
  effects are what make `replay --shadow` safe to run against a live tenant.
- **Delivery is at-least-once, and that is not fixable at this layer.** The
  delivery and the record of it are separate commits, so a crash between them
  redelivers. Every effect carries a stable idempotency key; a handler that
  passes it downstream is what turns the second delivery into a no-op.
- **An effect whose kind has no registered handler is not claimed.** Claiming and
  failing would burn attempts and dead-letter a tenant's work during an ordinary
  staggered rollout. Unhandled effects age instead, and the backlog-age health
  check is where "nobody can handle this" belongs.

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

### 1.11 The kernel holds no business domain (D11) — revising an earlier proposal

An earlier draft placed the general ledger in the kernel, on the reasoning that
double-entry balance is a universal invariant rather than a tenant preference.
The premise is true. The conclusion does not follow.

**What was wrong with it:**

- **Not every tenant needs accounting.** A tenant running only inventory, or a
  booking system, would carry the ledger's schema and migrations for nothing.
- **It contradicts the modularity requirement.** Modules exist so a tenant enables
  what they need and pays for what they enable. Accounting is among the most
  valuable things to charge for; making it unremovable removes it from the
  catalogue.
- **It couples every module to a domain.** With the ledger in the kernel, an
  inventory module depends on accounting types whether or not that tenant has
  accounting.
- **The invariant is smaller than the domain.** What is genuinely universal is
  *debits equal credits per currency*, *posted entries are immutable*, and *a
  closed period refuses postings*. The chart of accounts, statement formats,
  posting rules, fiscal-calendar shape, and multi-GAAP/multi-book support are all
  variable — and some are regulated differently per jurisdiction. Bundling the
  variable parts with the invariant ones puts jurisdiction-specific accounting in
  the kernel.

**The rule, restated:** the kernel is a *framework*, not a domain. It holds the
event log, tenancy, identity, the projection runtime, money, rules, the module
system, and the outbox. Every business capability — including accounting — is a
module.

An invariant lives with the thing it constrains. `BalancedLines` (§4) belongs to
the ledger module, not the kernel; the module enforces its own law. Modules
contribute health checks to the platform's verification battery (§7), so the
trial-balance canary still runs — as a ledger-module invariant rather than a
platform one.

**Consequences to design around:**

- Modules that need accounting declare `depends_on: [ledger]`, so the ledger
  cannot be disabled while they are on. That is a module-system concern, handled
  explicitly, rather than a reason to make the ledger permanent.
- A module reacting to another module's events by *emitting* events is a process
  manager, not a projection. Its output is written to the log once, live, and is
  never regenerated by replay — replay rebuilds projections only. See L5.

**Amended when the second module was built** (`modules/sales`, Phase 4). This
section originally said modules must never call the ledger directly, and that
integration is *by event*: a module emits, and the ledger's posting rules turn
that into a journal entry asynchronously.

Building it separated two things that had been assumed to travel together:

- **Who decides the journal lines** — the coupling question. The answer stands,
  and Phase 6's configured posting rules are still where it lands. Until then
  `sales::PostingAccounts` holds the mapping explicitly, which is a value the
  configuration will supply rather than a call the module will stop making.
- **When the posting happens** — a separate question, and the original answer was
  wrong. An invoice and its journal entry now commit in **one transaction**,
  through `ledger::post_entry_in`.

The asynchronous version buys nothing here and costs a guarantee. Delivering an
effect at-least-once is unavoidable when the far side is outside this process —
an email, ZATCA clearance — and is a *choice* between two aggregates in the same
database. Taking it would mean an invoice can exist without its accounting entry,
which is a state that needs a dead-letter queue, a sweeper, and an operator to
explain it, in exchange for decoupling that the configured mapping provides
anyway.

So: the dependency arrow `sales → ledger` is real, declared, and enforced at
signup. What sales still cannot do is read `proj_ledger` — the groups never touch
each other (L3), and they share only the log.

### 1.12 Errors are data, not prose (D12)

Saudi Arabia is the first market, so Arabic is a target language, not a
translation layer added later. That forces one decision early: **an error carries
a stable [`MessageCode`] and typed arguments; the sentence is chosen at the API
boundary** from `Accept-Language`.

`#[error("no membership for this identity")]` bakes English into the type and
turns localization into a rewrite. It also duplicates work: the machine-readable
code is already required as the `type` field of the RFC 9457 problem response, so
the localization requirement and the API requirement are the same requirement.

Three consequences specific to Arabic, all mechanised in `spa-i18n`:

- **Six CLDR plural categories**, selected by `n % 100`. `if n == 1` is wrong for
  3 vs 103 vs 11 in a way no reviewer catches.
- **Bidi isolation.** A Latin identifier interpolated into Arabic reorders the
  text around it unless wrapped in `U+2068`/`U+2069`. Applied automatically for
  RTL locales.
- **Completeness is enforced.** A code without a translation in every locale
  fails the build, so a missing Arabic string cannot ship as English.

What a user is told is not what an operator is told. `NoSuchTenant` and
`NotAMember` render identically — distinguishing them is a tenant-enumeration
oracle — and internal failures never describe themselves. Both are tested.

### 1.13 Clusters are data; placement follows activity (D13)

A cluster is a row, not a config constant, so capacity can be brought online
without a deploy. **Credentials are never stored**: the row names an environment
variable holding the DSN, so a control-plane backup carries no passwords.

Two capacity limits, answering different questions:

| limit | bounds | binds when |
|---|---|---|
| `max_active_tenants` | connections | tenants are busy |
| `max_databases` | storage, migration time, catalog size | tenants are numerous |

The first is the one that takes a cluster down, because open connections scale
with concurrently-active tenants (§1.1). Utilization is the **maximum** of the two
ratios, never the average — a cluster 20% full on storage and 99% full on activity
is 99% full. Expressed in integer basis points, since `float_arithmetic` is denied
workspace-wide.

Default placement is `Balanced` (least-utilized first): activity is what binds, so
spreading it is right. `Packed` exists for deliberate consolidation while tenants
are mostly dormant. `Draining` keeps a cluster serving while taking no new
tenants, which is how hardware is retired.

### 1.14 Background work is scheduled by per-visit lease (D14)

Projections and the outbox need driving. With one worker that is trivial; with a
fleet it is a coordination problem whose obvious answers are both wrong.

**Every worker services every tenant** is *safe* — two workers on one projection
group is already refused by the checkpoint lock (L4) — but each worker opens a
connection to each tenant to discover there is nothing to do. Connections scale
as workers × tenants, and by D13's own sizing rule that makes every tenant
permanently active. It is the one thing that must not happen.

**Static assignment by hash** means a deploy either leaves a shard unowned or
leaves it doubly owned, for as long as the rollout takes.

So a worker claims tenants that are **due**, holds them for the length of one
visit, and lets the claim lapse. `FOR UPDATE SKIP LOCKED` makes simultaneous
claims disjoint; a worker that dies is recovered from by doing nothing. Claiming
and renewing are the same call, so long work needs no second code path.

The throttle is a separate column, and it is the one that matters for cost. A
visit that finds nothing pushes `next_visit_at` out by an interval plus jitter
derived from the tenant's own id — so idle tenants cost one short query per
interval and hold no connection between them. `request_visit` pulls a tenant
forward, which is where a push path attaches when the API can tell a worker
directly that a tenant just wrote something: polling becomes the floor rather
than the mechanism, and nothing downstream changes.

Background access is its own entry path (`enter_for_maintenance`). It takes no
identity — which is the safety argument, since a request handler always has one
and so cannot reach it by accident — and is fixed to the background lane.

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
crates/                                                        ← core: what a tenant cannot disable
  spa-types       (no deps)    newtypes, Money, NonEmpty — WASM-safe, frontend-shareable
  spa-i18n        types        message catalogs, locales, the Localize trait
  spa-eventlog    types        gapless append, load, upcasters, numbering, configuration, outbox
  spa-projection  eventlog     groups, ProjectionCtx, runner, leases, shadow replay and swap
  spa-control     eventlog     identities, tenants, entitlements, clusters, TenantDb, fleet
  spa-worker      control,projection  Job trait, tenant visit loop, cancellation and drain,
                               bin/worker, bin/migrator, bin/reaper
  spa-web         control      extractors, problem+json, paging, request messages —
                               what a module's routes are built from
  spa-api         web,modules  the core's own routes, the module list, composition root
  spa-demo        api          the seeded tenant, and bin/demo
  spa-testkit     all          template-DB fixtures, fault injection, differ
modules/                                                       ← what a tenant chooses
  ledger          core,web     accounts, journal entries, fiscal periods, VAT treatment,
                               charts, the trial-balance invariant, its own routes
  sales           ledger       invoices, credit notes, payments received
  purchases       ledger       bills, payments made, input tax
  tax_sa          sales,purchases  the Saudi rate, the VAT return, ZATCA
```

**A module ships its own routes.** `sales::http::routes()` is a router the sales
crate owns, and `spa-api` mounts it — which is why `spa-web` exists and sits
*below* the modules: an extractor a module cannot name is one it cannot use, and
a module reaching up into `spa-api` for one would close a cycle, because
`spa-api` names every module. What stays in the composition root is the decision
about what is mounted, not the writing of it.

**Core is what a tenant cannot disable**; a module is what they choose. Direction
is the enforcement, and it points one way: modules depend on core, and may depend
on modules *below* them. `tax_sa → {sales, purchases} → ledger` is deliberate — a
VAT return nets output tax against input tax, and the thing that nets them has to
be above both rather than a third sibling reaching sideways. What no module may do
is reach the control plane, or reach another module's tables (L3).

The *crate* dependency and the *entitlement* dependency are not the same thing.
`tax_sa` links against both sales and purchases, and a tenant needs **at least
one** of them: a business that only sells still files a return, and demanding
both would force a shop with no supplier bills to enable a module they do not
use. `ModuleSetup` says both — `requires` is an AND list, `requires_any` a group
satisfied by any member — and signup, enabling, and refusing to disable all read
the same declaration.

**Extension is by subscription, and the extended module does not know.** `tax_sa`
builds a ZATCA document out of `sales.invoice.issued` without a line of `sales`
changing: a projection reads the whole log rather than its own module's slice, its
group is the unit of consistency so it writes only into `proj_tax_sa`, and
`Upcasters::also` folds `sales`' event history into its own so a version added
later stays readable. No registry, no hook, no callback — which is why an
extension cannot break what it extends.

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
| Crash safety | `pg_terminate_backend` mid-transaction — a real severed connection, not a simulated error; assert no partial state and that resume completes |
| Shutdown safety | SIGTERM mid-batch; assert the drain completes, then **rebuild the projection from the log and diff** rather than trusting the numbers left behind |
| Effect delivery | At-least-once asserted, not exactly-once: a crash between the delivery and its record must redeliver, with the same idempotency key |
| Deploy safety | A worker without a module's handler leaves those effects unclaimed rather than dead-lettering them |
| Old data still reads | Golden files of real event JSON per schema version, decoded every build |
| Migration equivalence | Schema migrated from v(N−1) must equal one built fresh at vN |
| Tenant isolation (D1) | Two provisioned tenants; assert no code path reaches across |
| Authorization | Matrix over (role × capability × facts); deny-by-default for unlisted pairs |
| Module composition | Demo build with every module enabled, as a required CI check |
| Blueprint validity | Every shipped blueprint previewed against a fresh tenant in CI |
| API contract | OpenAPI drift; generated-client round-trip; problem+json shape |
| Connection strategy | `soak.rs` — asserts open connections track active tenants, busy connections track the lane budget, and neither tracks request count |
| Entry-path cost | A cold `enter` costs exactly 4 lookups; 200 warm ones cost 0 |

**Validation happens in four places, each with one job:**

1. **Edge** — shape and type, by parsing into typed requests.
2. **Domain** — invariants, in aggregates and smart constructors.
3. **Authoring** — rules and config validated against registries when written,
   returning every problem at once.
4. **Continuous** — the invariants below asserted per tenant, in production.

**Continuously asserted, per tenant.** Platform invariants come from the kernel;
modules contribute their own through the health-check registry, so a tenant is
checked for exactly what it has enabled.

*Kernel:*

- schema version = target, per module
- projection lag < threshold, per group
- event positions contiguous (L1)
- unresolved dead letters = 0
- outbox backlog age < threshold

*Contributed by the ledger module:*

- **trial balance balances, per currency**

That last one holds only if commands, events, projections and replays are all
correct, so it catches an entire class of pipeline bug in one number — which is
why it is worth naming even though it is now a module's invariant rather than the
platform's. The demo tenant has every module enabled (§4.10 of the review), so
CI gets the canary regardless of what any individual tenant runs.

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
