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

- [ ] Gapless append with per-tenant `pg_advisory_xact_lock` (L1)
- [ ] Concurrent-append test asserting contiguity and commit order
- [ ] `Envelope`, `StreamId`, aggregate load/append
- [ ] Projection groups, one Postgres schema each, `search_path` isolation (L3)
- [ ] `ProjectionCtx` — no clock, no RNG, no pool (L2)
- [ ] Checkpoint-in-transaction with `FOR UPDATE` leases (L4)
- [ ] Scheduler leasing *tenants with pending work*, not a loop per tenant
- [ ] `replay --shadow` + table differ, wired into CI
- [ ] Outbox schema and dispatcher; effects as values (D9)
- [ ] `CancellationToken` + `TaskTracker` drain; `bin/worker`
- [ ] SIGTERM-mid-batch test
- [ ] Event `schema_version` + upcaster registry + golden-file tests
- [ ] Fault injection at transaction boundaries (carried from 1c)

**Exit:** a projection can be written, replayed into a shadow schema, and proven
identical by a differ rather than by assertion.

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
