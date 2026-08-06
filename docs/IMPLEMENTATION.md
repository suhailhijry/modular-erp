# Implementation plan

Phased build order for the architecture in [ARCHITECTURE.md](./ARCHITECTURE.md).

Ordering is by **dependency**, not by value. Each phase ends somewhere that
compiles, tests green, and is worth having on its own. Durations assume one
focused engineer and are the least reliable thing in this document.

Type-safety work (§4 of the architecture) is deliberately spread across phases
rather than batched — it is cheapest applied to code as it is written.

**Legend:** `[ ]` todo · `[~]` in progress · `[x]` done

---

## Phase 1 — Foundations · 3–4 weeks

Nothing above this is worth building until the handle types and the test harness
exist, because everything after inherits them.

### 1a · Workspace skeleton
- [x] Cargo workspace, crate layout per architecture §6
- [x] `rust-toolchain.toml` pinning the toolchain
- [x] Shared lint configuration (`workspace.lints`), warnings denied
- [x] Retire the prototype `src/` tree (preserved at `f2e8acd`)

### 1b · `spa-types`
- [x] Newtype macro: `Display`, `FromStr`, serde, sqlx, validation
- [x] Identifiers: `TenantId`, `IdentityId`, `AggregateId`, `StreamId`
- [x] Distinct position types: `LogPosition` (global) vs `Sequence` (per-aggregate)
- [x] `CurrencyCode` with ISO-4217 minor-unit exponent
- [x] `Money` — runtime currency, no `Add`, `checked_add -> Result`
- [x] `NonEmpty<T>`
- [x] Unit tests including the "these two types cannot be confused" cases

### 1c · `spa-testkit`
- [x] Template-database-per-test fixture (`CREATE DATABASE … TEMPLATE`)
- [x] Measured: ≈280 ms to acquire, ≈140 ms to drop (local Postgres 18)
- [x] Parallel-safe (unique names, automatic teardown)
- [ ] Fault-injection hooks at transaction boundaries *(deferred to Phase 2, where the transactions exist)*

### 1d · Control plane
- [x] Schema: identities, memberships, tenants, entitlements
- [x] Append-only audit trail, enforced by trigger (D2)
- [x] Connection manager: LRU pools, `min = 0`, global budget as a semaphore
- [x] `TenantDb` with no public constructor; `ControlPlane::enter`
- [x] Tenant registry carries `(cluster, database)` from day one
- [x] Support access as a separate audited path — no `is_system` bypass
- [ ] Authenticators and sessions *(deferred to Phase 3, with the rest of auth)*

### 1e · Build hygiene
- [x] `.sqlx/` offline query data; `SQLX_OFFLINE=true` in `.cargo/config.toml`
- [x] Verified: `cargo build` succeeds with no `DATABASE_URL` and no server
- [x] Verified: a query that drifts from the committed schema fails the build
- [x] `justfile` with `check`, `prepare`, `clean-databases`
- [x] `docs/DATABASE_SETUP.md`

### 1f · Exit criteria
- [x] Two-tenant isolation test: no code path reaches across
- [x] `cargo test --workspace` green (60 tests), `clippy -- -D warnings` clean,
      `cargo fmt --check` clean
- [x] **Soak test** (`spa-control/tests/soak.rs`, run with `--ignored`).
      Measured: open connections track *active tenants × per-tenant pool*, busy
      connections track the lane budget, neither tracks request count, entry
      cache hit rate 99.9%. 22,169 ops/s across 40 tenants with 256 workers.
- [x] Per-operation connection permits with per-lane bulkheads
- [x] Entry-path cache (four cold lookups, zero warm)
- [x] Read-replica seam (`TenantDb::read`, falls back to primary)

### 1g · Localization (D12)
- [x] `spa-i18n`: `Locale`, `MessageCode`, `MessageArg`, `Message`, `Localize`
- [x] CLDR plural rules — six categories for Arabic, two for English
- [x] Bidi isolation of Latin arguments inside RTL text
- [x] `Accept-Language` negotiation with quality values and regional subtags
- [x] English + Arabic for every control-plane message
- [x] Completeness test — a missing translation fails the build (verified by
      deleting one and watching three tests fail)
- [x] User-facing messages never leak tenant existence, internal detail, or
      cluster topology

### 1h · Multi-cluster (D13)
- [x] `cluster` table; credentials by env-var name, never stored
- [x] `cluster_load` view — counts from the tenant table, not a drifting counter
- [x] `ClusterStatus`: available / draining / full / offline
- [x] `PlacementPolicy`: balanced and packed, deterministic tie-breaking
- [x] Utilization in integer basis points, max of the two limits
- [x] `register_tenant` places automatically; `register_tenant_on` pins
- [x] Foreign key so a tenant cannot be placed on a nonexistent cluster, and a
      cluster holding tenants cannot be deleted
- [x] Typed `SlugTaken` — a normal signup outcome, not a database failure

---

## Phase 2 — Event core with reproducibility built in · 3–4 weeks

Replay guarantees are structural. They cannot be added later without rewriting
every projection, so they land before the first projection exists.

- [x] Gapless append — **a counter row, not an advisory lock** (see notes)
- [x] Concurrent-append test asserting contiguity and commit order
- [x] A test proving the naive implementation *does* lose events, so the one
      above cannot pass vacuously
- [x] Append-only enforced by trigger; `integrity()` for the continuous check
- [x] `Envelope`, `Metadata` with `config_version` (L5), optimistic concurrency
- [x] Reads: `read_since` (tailer), `read_stream`/`read_stream_since` (rebuild)
- [x] `Aggregate` / `DomainEvent` traits; `load`, `load_since`, `append_events`
- [x] `execute` — load, decide, append, retry on conflict, give up as `Contended`
- [x] Upcaster chain (`v1 → v2 → v3`), gap check, events-from-the-future refused
- [x] Golden files: every stored shape decodes on every build
- [ ] Snapshots *(deferred — an optimization, and which aggregates need one is
      not yet known; `load_since` is the seam)*
- [x] Crash tests: a rolled-back append returns its positions; a crash between
      the append and the promise leaves neither
- [x] Projection groups, one Postgres schema each, `search_path` isolation (L3)
- [x] `ProjectionCtx` — no clock, no RNG, no pool; `derive_id` instead (L2)
- [x] Checkpoint-in-transaction; the row lock doubles as the lease (L4)
- [x] `run_once` / `run_to_head`; `Progress::Busy` rather than blocking
- [x] `replay_shadow` + table differ (`EXCEPT ALL` both ways)
- [x] The differ is itself tested against a clock-reading projection, so a clean
      diff means something
- [x] Outbox schema; `Effect` as a value; `enqueue` in the command's transaction
- [x] `Decision` (events + effects) and `Committed` (position, version, effects)
- [x] Dispatcher: claim under `SKIP LOCKED`, deliver with no connection held,
      settle separately; exponential backoff, dead letters, health counters
- [x] Effects whose kind has no registered handler are **not claimed**, so a
      staggered deploy cannot dead-letter a tenant's work
- [x] Per-visit tenant leases (`claim_tenants`), `next_visit_at` scheduling with
      per-tenant jitter, `request_visit` as the seam for the push path
- [x] `enter_for_maintenance` — background access with no identity, own lane
- [x] `spa-worker`: `Job` trait, `ProjectionJob`, `OutboxJob`, `bin/worker`
- [x] `CancellationToken` + `TaskTracker` drain; leases released on the way out
- [x] SIGTERM-mid-batch test, verified by shadow differ rather than by assertion
- [x] Fault injection at transaction boundaries (carried from 1c) —
      `pg_terminate_backend`, not a simulated failure
- [ ] Snapshots *(deferred — see above)*
- [ ] Shadow replay wired into CI *(needs the demo tenant, Phase 4)*

**Exit:** a projection can be written, replayed into a shadow schema, and proven
identical by a differ rather than by assertion. **Met.** 215 tests.

---

## Phase 3 — Kernel services and the API contract · 3–4 weeks

No business domain here — that is D11. This phase builds what modules are built
*on*.

- [ ] `Module` trait, registry, and the health-check registry modules contribute to
- [ ] Capability permits; `Permit<C>` minted only by the authorizer
- [ ] Tenant-local authorization as a projection, never an aggregate replay (L7)
- [ ] `spa-config`: declarations, layers, versioned resolution, provenance
- [ ] Numbering (gapless per-tenant document sequences)
- [ ] API: problem+json, cursors, `ETag`/`If-Match`, `Idempotency-Key`
- [ ] OpenAPI generation with CI drift check
- [ ] Authorization matrix tests; crash-injection tests

**Exit:** a module can be written, mounted, authorized and called — with nothing
domain-specific in the kernel.

## Phase 3b — `modules/ledger` · 2–3 weeks

The first real module, and the proof that the module seam works. Built as a
module from the start rather than extracted from the kernel later.

- [ ] Accounts, journal entries, fiscal periods
- [ ] `BalancedLines` as a proof-carrying event payload
- [ ] Typestate on `JournalEntry` and `FiscalPeriod`
- [ ] Trial-balance invariant contributed to the platform health registry
- [ ] Property test: any valid command sequence leaves the ledger balanced

**Exit:** a correct ledger behind an API a third party could integrate against —
and a module a tenant can decline.

---

## Phase 4 — Modules, blueprints, provisioning, demo · 4–5 weeks

- [ ] A second business module, proving cross-module integration by event
- [ ] Entitlements → enable/disable durable workflows
- [ ] `ModuleEnabled<M>` capability tokens
- [ ] Blueprints: browse → parameterize → materialize → edit → preview → install
- [ ] Preview executes in a rolled-back transaction and reports resulting state
- [ ] Chart-of-accounts templates: generic IFRS, SOCPA-aligned, retail, services, empty
- [ ] Self-service signup as a durable workflow
- [ ] Template databases per module combination, built in CI from blueprints
- [ ] Demo tenant TTL and reaper
- [ ] **Demo blueprint with every module enabled, as a required CI check**
- [ ] Fleet migrator; per-tenant health checks including the trial-balance invariant

**Exit:** someone signs up online, picks a chart of accounts, and gets a working
system.

---

## Phase 5 — The rule engine and its simple surface · 3–4 weeks

- [ ] `spa-rules`: `Facts`, `DynCondition`, `FactRegistry`, `Rule<E>`
- [ ] Authorization and pricing both on it
- [ ] Per-request fact assembly with startup coverage assertions — an
      unsatisfiable condition fails the build, not a user's request
- [ ] Authoring levels 0–3 with `origin` round-tripping
- [ ] Rule packs as blueprints
- [ ] `explain`-backed dry run; effective-permission inspection

**Exit:** one engine behind every rule, and a surface most tenants never leave.

---

## Phase 6 — Configured domain · 5–7 weeks

- [ ] Account determination and posting rules
- [ ] The business module's ledger path migrated onto them
- [ ] `StateMachine` as data driving document workflows and approval routing
- [ ] `DocumentType` as versioned data; generic `Document` aggregate
- [ ] One document type ported end to end
- [ ] Remaining modules, each landing with its own blueprint and demo participation

**Before starting:** resolve the open question in architecture §8 about whether
the generic `Document` aggregate is right for the real document mix.

**Exit:** tenants configure charts, documents, posting and approvals without a
deploy.

---

## Running notes

- **Effects are written by commands, not derived by projections.** A projection
  deriving effects from the stream would get exactly-once for free from L4, which
  makes it the tempting design. It is wrong for one reason that settles it:
  projections are rebuildable, and a rebuild would re-derive every effect and
  re-send years of email. Command-time effects mean a rebuild sends nothing,
  which is what makes `replay_shadow` something you can run in production. It
  also matches L5 — an effect records a decision taken under the configuration in
  force at the time, and re-deriving it later would resolve against today's.

- **A missing handler must not be a delivery failure.** The first design claimed
  every due effect and failed the ones it could not handle, which backs off and
  eventually dead-letters. That turns an ordinary staggered rollout — some
  workers have a module's handler, some do not yet — into a dead-letter storm for
  every tenant using that module. The claim now filters on the kinds the
  dispatcher knows, so an unrecognised effect is simply left for a worker that
  can take it, and "nobody can handle this" surfaces through the backlog-age
  alarm instead. `effects_with_no_registered_handler_are_left_alone` is the test.

- **`impl Into<Decision>` made a rejection-only command handler uninferable.**
  `execute` first took anything convertible into a `Decision`, so a command with
  no effects could return a bare `Vec`. A closure whose only branch is
  `Err(...)` then has no way to name the `Ok` type, and the compiler's complaint
  points at `Result` rather than at the real problem. It now takes `Decision`
  directly: one fewer generic parameter, always inferable, and `Decision::one(…)`
  at every call site puts the D9 vocabulary where a reader will see it.

- **`FOR UPDATE` with `OFFSET` locks the rows the offset skipped.** A test that
  held a lock on "the second outbox row" via `ORDER BY id LIMIT 1 OFFSET 1 FOR
  UPDATE` was locking the first row as well, because discarded rows still pass
  through the `LockRows` node. It measured the wrong thing and failed for the
  right reason. Pick the id first, then lock by primary key.

- **The lease is per *visit*, not per tenant.** Two workers processing one
  projection group is already refused by the checkpoint lock (L4), so a tenant
  lease is not what makes concurrency safe — it is what stops two workers opening
  connections to the same tenant at the same moment to learn there is nothing to
  do. That reframing removed the renewal loop, the rebalancing, and the
  membership protocol: one statement claims what is due, and the mark lapses
  afterwards.

- **Idle tenants are throttled by `next_visit_at`, not by the lease.** The
  measured sizing rule is `connections ≈ active_tenants × per_tenant_pool`, so
  visiting every tenant constantly would make every tenant active. A visit that
  finds nothing pushes its tenant out by an interval, and per-tenant pools hold
  no connection in between. The jitter is derived from the tenant's own id rather
  than a random source, so it is a pure function and a restart does not reshuffle
  the fleet — without it, a batch claimed together stays synchronized forever.

- **`run_once` had to give up ownership of its transaction.** The worker takes
  its connections from `TenantDb`, which has no public pool accessor by design —
  so a runner that begins its own transaction from a `&PgPool` cannot be driven
  by a worker without breaking the boundary that makes cross-tenant access a type
  error. `run_once_in` takes the caller's connection and does everything L4 needs
  inside it; `run_once` is now a thin wrapper for tests. The obligation moves to
  the caller, and it fails safe: forgetting to commit loses a batch, and there is
  no ordering in which a caller can commit part of one.

- **Fault injection kills the connection rather than simulating a failure.**
  Returning an error from a fake proves the code's own rollback path works, which
  was never in doubt. `spa_testkit::kill_connection` issues
  `pg_terminate_backend` from a second connection, so Postgres does the rollback
  and the code finds out the way it would in production. That is what makes
  `a_crash_mid_batch_leaves_neither_rows_nor_a_moved_checkpoint` an L4 test
  rather than an error-handling test.

- **The outbox test suite asserts at-least-once, not exactly-once.** Delivery and
  the record of it are separate commits, so a crash between them redelivers.
  Asserting "exactly once" would assert something the design does not provide,
  and would first fail in production rather than in CI. The test asserts the two
  deliveries carry the *same* idempotency key, which is the property that makes
  at-least-once survivable.

- **A five-millisecond backoff made an assertion a race.** A test asserted an
  effect was *not yet* due immediately after failing delivery, with the backoff
  set to 5ms so the suite would stay quick. Under the full parallel suite it lost
  that race about one run in six — and a flake that says the code is broken when
  the test is is worse than a failure, because the response to it is to rerun.
  Fixed twice over: the assertion now reads `next_attempt_at` from the row rather
  than inferring it from a second dispatch, and waiting for a backoff polls for
  due-ness instead of sleeping a guessed duration. Verified across eight
  consecutive runs of the file and three of the workspace.

- **`just prepare` did not work from a clean checkout either.** Same class of bug
  as `cargo test` in Phase 1: `just` does not read `.env` by default, so a
  developer whose Postgres wants a password got `no password supplied` from a
  recipe while their tests passed. `set dotenv-load := true`, and both the
  type-check and admin URLs are now derived from `DATABASE_URL` so credentials
  live in one place.

- **L3 isolation caught its first bug immediately — mine.** The shadow rebuild
  set `search_path` to the shadow schema *before* reading the log, which put the
  `event` table out of scope. The isolation was working exactly as designed; the
  sequencing was wrong. `run_once` had it right: read the batch, then narrow the
  path, then apply.

- **The differ needs its own proof.** A clean shadow diff is ambiguous between
  "replay is reproducible" and "the differ does not work", and the second is
  indistinguishable from the first until it matters. So there is a projection
  that deliberately writes `now()`, and a test asserting the differ catches it —
  same discipline as the naive-event-log test in Phase 2a.

- **A concurrency test can pass without ever hitting the thing it tests.** The
  16-task retry test was asserted to exercise the retry path; counting decision
  invocations showed *16 decisions for 16 successes* — the transactions were
  short enough that they never overlapped. The retry loop now has a test that
  injects the competing write from inside the decision closure, so the conflict
  is caused rather than hoped for.

- **Do not block inside an async task to coordinate a test.** The first attempt
  at the above held two tasks at a spin-wait until both had loaded. It deadlocked
  — a spinning task starves the worker that would run the task it is waiting for
  — and then *passed* on a rerun with different scheduling, which is worse than
  failing. `block_in_place` is the supported escape hatch; better still, arrange
  for one task to need no coordination.

- **`cargo test` did not work from a clean checkout.** Cargo does not read
  `.env`, so a developer whose Postgres requires a password got
  `password authentication failed` even with a correct `.env` — the test binary
  never saw the variable. I had been masking this by exporting `.env` manually in
  every command, which is exactly how a setup bug survives to the first new
  contributor. `spa-testkit` now loads `.env` itself, and a connection failure
  reports what it tried, where the setting came from, and the password redacted.

- **L1 needs a counter row, not an advisory lock.** The architecture said
  `pg_advisory_xact_lock` per tenant. Two corrections: database-per-tenant means
  the log is already tenant-scoped, so there is nothing to key a lock on; and an
  advisory lock over a sequence gives commit *ordering* but not gaplessness,
  because a rolled-back transaction burns its number. A counter row updated with
  `UPDATE ... RETURNING` gives both — the row lock serializes, and the counter is
  transactional so a rollback returns the position. That turns the contiguity
  check from a warning into a real integrity assertion.

- **Localization completeness is now a shared audit.** `spa_i18n::testing::audit`
  checks translation coverage, plural categories per language, non-empty
  rendering, Arabic-actually-in-Arabic, and code shape. Each crate's test is one
  line, and when `Module` gains `messages()` the registry can run it across every
  module — which is what makes it impossible to ship a module without
  translations.

- **Missing translations warn and fall back; they do not fail the request.** The
  deliberate exception to L6: a Saudi user reading one English sentence is
  inconvenienced, one reading a 500 is blocked. CI is where a missing string is
  found; the runtime fallback is what happens if one slips past.

Decisions taken during implementation that amend the architecture are recorded
here and folded back into ARCHITECTURE.md.

- **Money is not generic over currency.** Recorded as D10. Currencies are tenant
  configuration and cannot be type parameters; the guarantee is preserved by
  omitting `Add` rather than by a phantom type.

- **Validated string newtypes get no `sqlx::Type` derive.** `#[sqlx(transparent)]`
  generates a `Decode` that skips the validating constructor, so a value read
  back from the database would bypass its own invariant — which is precisely
  where it matters, since that is where data written by older versions arrives.
  Callers bind with `as_str()` and decode through `new()`.

- **Test-database acquisition measures ≈280 ms, not the ~200 ms first claimed.**
  Teardown is another ≈140 ms but is off the critical path. Numbers from
  `cargo test -p spa-testkit --test harness cloning_is_fast -- --nocapture`.

- **The kernel holds no business domain; accounting is a module.** Recorded as
  D11. The earlier placement confused a universal *invariant* (debits equal
  credits) with a large *domain* (chart of accounts, statement formats, posting
  rules, fiscal calendars, multi-GAAP) — and made the most saleable module
  unremovable. Phase 3 is now kernel services only; the ledger is Phase 3b, built
  as a module from the start rather than extracted from the kernel later.

- **The connection permit was scoped to the request; it is now scoped to the
  operation.** Holding a permit across business logic caps *concurrent requests*
  at the budget, when what needs capping is *concurrent database operations* —
  ~400 connections at 10k req/s instead of ~120. `TenantDb` now holds only pools;
  `acquire`/`begin`/`read` take the permit for the duration of the operation.

- **The lane budget does not bound open connections.** The soak test refuted
  that: with a budget of 32 across 40 tenants, peak open connections was 95. A
  connection returned to a tenant's pool stays open until the idle timeout, so
  connections accumulate across every tenant touched in that window. Halving
  `max_connections_per_tenant` halved the peak; setting it to 1 produced exactly
  the tenant count. The real rule is
  `connections_per_cluster ≈ active_tenants × max_connections_per_tenant`, and it
  means **cluster count is sized by concurrently-active tenants, not by tenant
  count or request rate**. Defaults changed accordingly (per-tenant 8 → 4, idle
  timeout 30s → 10s).

- **Throughput rose as the per-tenant pool shrank** — 7.7k → 22.2k ops/s going
  from 4 connections per tenant to 1 — because connection churn cost more than
  the extra parallelism bought. A bigger pool is not automatically faster.

- **`enter()` had to be cached before it could serve real load.** Four
  control-plane queries per request is 40,000 queries/second at 10k req/s against
  a database that cannot be sharded. A 5-second TTL cache with local invalidation
  on writes brings that to ~0.1% of requests. The cost is a bounded staleness
  window on revocation, documented in `cache.rs`; shortening it below a few
  seconds needs out-of-band invalidation, which is a Phase 3 decision.
