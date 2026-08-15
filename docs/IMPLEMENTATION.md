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
- [x] Shadow replay wired into CI *(done in Phase 4b, against the demo tenant —
      every group, on every run)*

**Exit:** a projection can be written, replayed into a shadow schema, and proven
identical by a differ rather than by assertion. **Met.** 215 tests.

---

## Phase 3 — The request path · 2–3 weeks

**Resequenced.** The original Phase 3 was "build what modules are built on"
before any module existed: a `Module` trait with one implementation, capability
permits with nothing to permit, a configuration system with nothing configurable.
Every one of those is an abstraction invented from a guess about its consumer.

So Phase 3 is now the **spine** — the shortest path from an HTTP request to a
tenant's data — and the kernel abstractions are deferred to 3c, where two real
modules can say what shape they need.

### 3a · Authentication and sessions *(carried from 1d)*
- [x] `authenticator` table; Argon2id, PHC strings so cost is raisable without a
      migration
- [x] Opaque 256-bit session tokens; only the SHA-256 is stored
- [x] Constant-time-ish login: an unknown handle costs the same as a wrong
      password, so the API is not an account-enumeration oracle
- [x] `SessionToken`'s `Debug` is redacted — a token in a log line is a working
      credential
- [x] `log_out`, `log_out_everywhere`, `sweep_sessions`
- [ ] MFA, OIDC, API keys *(more rows in `authenticator`, not more tables)*

### 3b · The HTTP surface
- [x] `spa-api` on axum; `bin/api` with body limit, timeout, graceful shutdown
- [x] problem+json (RFC 9457) plus `code` and `args` — a client branches on the
      code, never on the prose
- [x] `Accept-Language` honoured on every response including failures
- [x] Composite catalog across crates, with a duplicate-code test
- [x] `Tenant` extractor: the only route to a `TenantDb`, so "did we check the
      membership?" is answered by the signature
- [x] Status mapping in one place; a tenant that exists but is not yours is
      byte-identical to one that does not exist
- [x] Nine tests through the real router, including the two enumeration oracles

### 3e · Provisioning
- [x] `ControlPlane::provision` — registers, creates and migrates the database,
      installs modules, grants the owner, activates. Idempotent throughout.
- [x] Compensation: a failure drops the database and the row, so the name is
      free again — proved by signing the same name up successfully after
- [x] `ControlPlane::sign_up` — the whole thing, plus the account and a session
- [x] `ModuleSetup`: a module *describes* its install (SQL + projection groups)
      rather than the control plane knowing what a ledger is
- [x] **`POST /v1/signups`** — one request, and the caller has a working system
      they are already logged into, with the ledger installed and usable
- [ ] A sweeper for tenants stuck in `provisioning` *(not needed while signup
      compensates synchronously; needed the moment a step goes async)*

### 3c · Kernel services

Built only where the ledger produced a second consumer.

- [x] `Job::module()` — a tenant that declined a module is not worked on its
      behalf, which is what "modular" has to mean to be worth anything
- [x] `Invariant` trait + `HealthJob` — architecture §7's per-tenant checks
      actually run, on an interval. Four kernel invariants; the ledger
      contributes the trial balance through the composition root
- [x] `bin/worker` composes the kernel and the modules; neither crate depends on
      the other
- [ ] `Module` trait and registry *(now describable: name, install SQL,
      projection groups, a router, a rejection-to-status mapping, an entitlement
      check, and an optional `requires`. `ledger_routes` and `sales_routes`
      differ only in the last three lines of each)*
- [ ] `Idempotency-Key` *(the ledger's mutations take client-chosen ids, so both
      are already idempotent — this may turn out to be unnecessary)*
- [ ] `ETag`/`If-Match` *(no update-in-place endpoint yet)*
- [x] `?consistent_after=<position>` — read your own write, with the write
      nudging the worker so the wait is a claim cycle rather than the idle backoff
- [ ] Cursors *(no list long enough)*
- [x] Roles and capabilities: `Role::allows` is the one place authorization is
      decided, and `Allowed<C>` in a handler's signature *is* the check
- [x] Authorization matrix as a test, written out rather than derived — over
      HTTP, every role against every endpoint
- [x] A 403 names the capability, in the caller's language
- [x] Member management: list, add, change role, remove — `ManageTenant`, and
      the last owner cannot remove or demote themselves
- [x] Invitations — a link the inviter passes on, single-use, expiring,
      revocable. The recipient sets their own password and the owner never sees
      it. No email: sending one is an outbox effect and belongs with the first
      real handler
- [ ] Tenant-local authorization as a projection (L7) *(roles live in the
      control plane for now; the projection is for fact-derived permissions)*
- [ ] `spa-config`: declarations, layers, versioned resolution, provenance
- [ ] Numbering (gapless per-tenant document sequences)
- [ ] OpenAPI generation with CI drift check
- [ ] Authorization matrix tests

**Exit for 3a+3b:** a person signs in and reads their own tenant over HTTP, in
Arabic, and cannot read anybody else's. **Met.** 230 tests.

## Phase 3d — `modules/ledger` · 2–3 weeks

The first real module, and the proof that the module seam works. Built as a
module from the start rather than extracted from the kernel later.

**This is what 3c waits on.** Writing the ledger against the bare spine shows
which kernel services are genuinely shared and which were invented — and the
first `Idempotency-Key` and `ETag` have a real mutation to attach to.

- [x] Accounts (open, rename, close) and journal entries
- [x] `BalancedLines` as a proof-carrying event payload — revalidates on
      `Deserialize`, so a stored unbalanced entry will not decode
- [x] Signed amounts rather than a debit/credit pair: "debits equal credits"
      becomes "sums to zero", one check on one number
- [x] Trial balance and account balances as **views**, not maintained tables
- [x] `imbalances()` — the health check this module contributes
- [x] Property tests on `BalancedLines`, plus a generated command sequence whose
      stored postings must still sum to zero
- [x] Shadow replay proves the ledger rebuilds identically
- [x] HTTP routes; the same `Tenant` extractor, so isolation is inherited
- [ ] Fiscal periods, drafts, reversals, multi-currency entries with FX
      *(each needs someone to want it before its shape is decided)*
- [x] Chart-of-accounts templates — `services` and `retail`, bilingual, with
      Saudi VAT and Zakat accounts in both
- [x] `GET /v1/ledger/charts` (unauthenticated — a signup form has to show the
      choices) and `POST /v1/tenants/{slug}/ledger/chart`
- [x] Installing is idempotent, so a half-finished install is fixed by retrying,
      and `retail` on top of `services` opens only the difference

**Exit:** a correct ledger behind an API a third party could integrate against —
and a module a tenant can decline. **Met.** 256 tests.

---

## Phase 4 — Modules, blueprints, provisioning, demo · 4–5 weeks

### 4a · `modules/sales`

The second module: invoicing with Saudi VAT, posting to the ledger.

- [x] `Invoice` aggregate — issue and record payment, both idempotent by
      client-chosen id
- [x] VAT as `standard` / `zero` / `exempt`, with the **rate resolved at issue
      and stored on the line** so a future rate change cannot restate a filed
      return
- [x] Tax computed per band, not per line — which is what the authority
      computes, and provably different from summing line-level rounding
- [x] Rounding half **away from zero**, so an invoice and its exact credit
      reverse without leaving a halala in VAT payable
- [x] The customer is a snapshot on the document, not a foreign key
- [x] `sales → ledger` in **one transaction** (`ledger::post_entry_in`), rather
      than by event through the outbox — see the running note and the amendment
      in architecture §1.11
- [x] `sales::requires()` — signup refuses sales without the ledger
- [x] Module gating on routes: a tenant that did not enable a module gets a 404,
      not a 500 from a missing table
- [ ] A VAT return and ZATCA clearance *(the module's commercial reason to
      exist; a return needs fiscal periods and clearance needs a certificate and
      an outbox handler)*
- [ ] Credit notes, cancellation, customers as records, quantities and unit
      prices, statutory gapless numbering

### 4b · The demo tenant

- [x] `spa-demo`: a tenant with every module enabled, filled **through the
      public HTTP API** — so a demo built out of internal calls cannot be
      perfect while the API a customer would use is broken
- [x] Deterministic: fixed dates, amounts and identifiers, so the numbers can be
      screenshotted and a CI failure is never "did the data change?"
- [x] Something in every state a screen has to render: settled, part-paid and
      untouched invoices; all three VAT treatments; expenses as well as sales
- [x] **A required CI check.** `cargo test --workspace` builds the whole demo
      and asserts every module answers, every group replays, every invariant is
      clean
- [x] **Shadow replay wired into CI** *(carried from Phase 2 — it needed the
      demo tenant, and now has one)*
- [x] `spa_api::modules()` — the module set in one place, so "every module
      enabled" is true by construction rather than by a second list agreeing
- [x] `bin/demo` + `just demo <password>`, so the demo is a thing a person signs
      into rather than a thing a test builds
- [x] `spa_demo::bootstrap` — migrates and registers the cluster, because a
      demo is usually the first thing pointed at an empty database
- [x] Demo tenant TTL and reaper — `set_demo_expiry`, `expired_demos`,
      `reap_demo`, and `bin/reaper` (`just reap`). Demos expire by default;
      `DEMO_TTL_DAYS=0` opts out

### 4c · Entitlements a tenant can change

- [x] `POST /v1/tenants/{slug}/modules` and `DELETE .../modules/{module}` —
      a tenant buys a module on a Tuesday and it works immediately
- [x] `ControlPlane::install_module` — read models **and** entitlement, because
      either alone is a tenant that 500s
- [x] `ModuleSetup::requires` — the dependency declared once and read by all
      three places that ask: signing up, enabling later, refusing to disable
- [x] Disabling deletes nothing. The entitlement is marked off; the events and
      read models stay, so a tenant who downgrades and returns finds their data
- [x] `GET /v1/modules` — the catalogue, unauthenticated, carrying dependencies
      so a picker can grey out impossible combinations
- [x] The test fixture installs modules through `install_module` rather than by
      hand, so it can no longer be right while the product is wrong
- [ ] `ModuleEnabled<M>` capability tokens *(`require_module` is a runtime check
      at the top of each handler; the token makes a disabled module's handler
      unconstructable. Worth it when a module has enough routes that remembering
      the call is the weak link)*

### 4d · The rest

- [x] A second business module *(shipped as 4a — and it changed how
      cross-module integration works, which is the point of building one)*
- [ ] Blueprints: browse → parameterize → materialize → edit → preview → install
- [ ] Preview executes in a rolled-back transaction and reports resulting state
- [ ] Chart-of-accounts templates: generic IFRS, SOCPA-aligned, retail, services, empty
- [ ] Self-service signup as a durable workflow
- [ ] Template databases per module combination, built in CI from blueprints
- [x] **Demo blueprint with every module enabled, as a required CI check**
      *(4b)*
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

- **"Implementation of `Send` is not general enough" is diagnosable — from the
  right place.** An axum handler's future must be `Send`, and rustc reports a
  failure at the *route table*, naming borrows in files that look unrelated
  (rust-lang/rust#102211). `#[axum::debug_handler]` finds nothing. A whole day
  went into chasing it from the handler, moving the error around without ever
  closing it.

  What actually worked was one line, put in the crate that owns the code:

  ```rust
  const _: fn() = || {
      fn assert_send<T: Send>(_: T) {}
      fn probe(control: &ControlPlane, modules: Vec<ModuleSetup>) {
          assert_send(control.sign_up(/* … */));
      }
  };
  ```

  With the error landing next to the cause, bisecting took twenty minutes. Four
  distinct triggers, each one enough on its own:

  1. **A helper `async fn` taking several references.**
     `install_modules(&Tenant, &[ModuleSetup], &mut PgConnection)` — three
     elided lifetimes. Inlined.
  2. **A borrowed iterator held across an await.** `for setup in &modules`
     carries a `slice::Iter<'_, _>`; indexing does not.
  3. **A closure capturing by reference across an await.** `|e| f(locale)`
     needed `move`; `&Locale` alone broke the proof.
  4. **`Migrator::run`, generic over `Acquire<'_>`.** `Box::pin` does not help —
     the opaque future still carries the bound. sqlx ships `run_direct` for
     exactly this, marked `#[doc(hidden)]` with the comment *"getting around the
     annoying `implementation of Acquire is not general enough` error"*.

  Two structural improvements fell out and were kept: DDL now goes through
  helpers that **take and return the connection by value**, so the `Acquire`
  bound never reaches a caller's future; and the control plane no longer depends
  on `spa-projection`, because creating a projection group turned out to be two
  statements it can run itself.

  **The lesson is the diagnostic, not the fixes.** When an error names types from
  a file you are not editing, assert the property where the code lives.

- **Submit-then-refresh was broken, and the fix was mostly a call nobody made.**
  Projections are driven by a worker, so a read taken immediately after a write
  can legitimately miss it. Every write already returned its log position and
  nothing could be done with it.

  `?consistent_after=<position>` waits for the projection to reach it — but the
  real bug was underneath: `request_visit` had existed since the lease work and
  *nothing called it*, so a tenant that had been quiet waited out its thirty-second
  idle backoff before anything projected the write. `consistent_after` would have
  timed out on a perfectly healthy system.

  On timeout the read is a 503, not stale data with a shrug. The caller asked for
  a guarantee this response cannot make; answering anyway is the behaviour that
  made the feature necessary. A read that does *not* ask never waits, and a test
  asserts it does not pay for the option.

  ponytail: one control-plane round trip per write, and the update is a no-op for
  an already-due tenant. If write rate makes it hot, batch the ids in the API
  process and flush on a timer — the call site does not change.

- **`Path<String>` silently 404s any route with two parameters.** The `Tenant`
  extractor pulled the slug positionally, which works for
  `/tenants/{slug}` and fails for `/tenants/{slug}/members/{identity}` — the
  shape every nested route in this API will have. It surfaced as a 404 on a
  route that plainly existed, which is the most misleading failure available.
  Extracting by name from a `HashMap` fixes it for every route, present and
  future.

- **A capability with no endpoint is a capability nobody has thought about.**
  `ManageTenant` shipped last round with nothing behind it — by the standard
  written two paragraphs above its own definition. Member management is what it
  was for: a tenant had exactly one user, forever.

  The owner sets a colleague's password and hands it over. The polished flow is
  an emailed invitation link, which needs email delivery, which needs an outbox
  handler nothing has written — so it would look finished and deliver nothing.
  What shipped is how small businesses actually onboard staff, and the invitation
  flow calls the same `add_member` once someone accepts.

  Two rules are worth naming. A demotion invalidates the cache immediately
  rather than waiting out the five-second TTL, because five seconds is five
  seconds of someone doing what they were just told they cannot. And the last
  owner cannot remove or demote themselves: a tenant with no owner has nobody
  who can add one, and the only fix is a support ticket.

- **The `role` column was written and never read.** Every member of a tenant
  could do everything — post entries, close accounts, install charts — while
  `grant_membership` dutifully recorded "owner" or "clerk" for nobody. A stored
  field that no code path consults is worse than an absent one: it reads like a
  control.

  Four roles, four capabilities, and `Role::allows` is the only place the
  decision is made — which is what makes the rule engine (Phase 5) a change to
  one function rather than an audit of every handler. `Allowed<C>` in a
  handler's signature is the check, for the same reason `TenantDb` has no public
  constructor: `tenant.require(…)?` on the first line fails by *omission*, which
  is silent and invisible in review.

  Clippy found the one honest mistake in the design: `Admin` and `Accountant`
  had identical bodies. With the capabilities that exist they were the same
  role, and a role that is a synonym for another is a support question with no
  answer. Dropped.

- **Platform staff are not tenant members with a different role.** Making the
  membership cache hold a parsed tenant `Role` broke support access immediately,
  because platform memberships store `support` and `superadmin` — a different
  vocabulary. Forcing them through the same enum would let "support" answer
  questions about what someone may do inside a tenant's books. Two caches now,
  each with the type its question actually has.

  A stored role this build does not recognise is a 500, not a default.
  Defaulting down locks someone out silently; defaulting up lets them in
  silently. A test asserts the 500.

- **A chart of accounts is a template, not a fixture.** The architecture
  describes blueprints as browse → parameterize → materialize → edit → preview →
  install. What shipped is browse, preview and install: every account a template
  creates is an ordinary account that can be renamed, closed, and posted to from
  the moment it exists, so "edit before installing" solves a problem that only
  exists if installing were irreversible. It is not.

  The two decisions that took thought were bilingual account names — telling a
  Saudi bookkeeper to rename eighteen accounts is a chore, not a starting point —
  and putting VAT and Zakat in *every* chart rather than an "advanced" one,
  because a Saudi business without them has to fix the chart before its first
  invoice. A test asserts both, including that the Arabic is actually Arabic
  rather than a copied English string.

  Installing skips accounts that already exist rather than refusing. Eighteen
  accounts is eighteen commands and the fifteenth can fail; refusing would make
  the retry — the obvious next move — fail immediately and leave the chart
  half-built forever.

- **The system did not actually work outside its tests.** After the ledger
  landed, a user could post an entry over HTTP and nothing would ever project it
  — `bin/worker` had no jobs registered, so the read models only moved when a
  test drove them by hand. Worth naming because every individual piece was
  tested and green: the gap was in the composition, which is the one thing unit
  tests structurally cannot cover.

- **Listing an invariant is not checking it.** Architecture §7 has named five
  per-tenant invariants since the first draft, and nothing ran any of them. They
  run now, on an interval — in memory rather than in a table, because losing the
  schedule on a deploy costs one extra check per tenant, which is cheaper than
  the table that would avoid it.

- **`Invariant` is a trait; the kernel checks are not.** The four kernel
  invariants apply to every tenant and there is nothing to register, so they are
  written directly. The trait exists because the *ledger's* trial balance must
  not be knowable to the kernel (D11) and the worker must not be knowable to the
  module — so they meet in `bin/worker`, in three lines. That is the shape a
  `Module` trait will take when there is a second module to describe it.

- **`Money` could not be decoded out of a stored event.** `CurrencyCode`'s
  `Deserialize` took `&str`, which only works when the deserializer can point
  into its input — true for `from_str`, false for `from_value`, which is how
  every event payload is decoded. Every unit test passed; the first real event
  carrying an amount failed. Now `Cow<str>`, with a test on the `from_value`
  path specifically. The lesson generalizes: a type's serde impl must be tested
  on the path production uses, not the path that is convenient to write.

- **Two sqlx migrators cannot share one database.** The ledger's read models
  were a numbered migration chain, which failed with `VersionMissing(2)` because
  `_sqlx_migrations` already belonged to the tenant schema. The fix is not a
  second table — it is noticing that a module's read models are *derived*, so
  there is no data to preserve across a change and no chain to be in. They are
  an idempotent install script now, and a module that eventually needs real
  migrations will need its own version table then.

- **The retry loop had to move to where the connection budget is.** `execute`
  begins a transaction per attempt, and a transaction needs a permit from the
  tenant's lane — so a version taking a bare `PgPool` either hands out an
  unmetered connection or holds one permit across every attempt. `try_execute`
  is one attempt in the caller's transaction; `TenantDb::execute` is the loop.
  Same split as `run_once`/`run_once_in`, for the same reason.

- **The ledger's route layer lives in `spa-api`, not in the module.** With one
  module, a `Module` trait that mounts routers is a trait with one
  implementation. When the second module lands, what the two route layers have
  in common *is* the trait — described rather than guessed. The module still
  owns everything that matters: aggregates, the invariant, the read models.

- **Balances are views.** A maintained balance table is a second thing that can
  be wrong, and keeping it in step is the projection code most likely to
  double-count. `sum(amount)` is exact and needs no code. Marked with the
  ceiling: fine to millions of postings, wrong at hundreds of millions.

- **Phase 3 was building abstractions before their consumers existed.** A
  `Module` trait with one implementation is a factory for one product; capability
  permits with nothing to permit are a guess about what a module will ask for;
  a configuration system with nothing configurable is the largest guess of the
  three. All of it was scheduled *before* the first module, which is the one
  thing that could tell us the shape. Phase 3 is now the request path — sign in,
  enter a tenant, get an answer in your language — and the kernel services wait
  for the ledger to say what they should be.

- **The extractor is the authorization.** `Tenant` in a handler's signature is
  the check, because its only constructor is `ControlPlane::enter` and there is
  no other route to a `TenantDb`. "Did we verify the membership?" stops being a
  question you answer by reading the handler body.

- **Two enumeration oracles closed, both tested.** A tenant that exists but is
  not yours returns a response byte-identical to one that does not exist; an
  unknown login handle costs the same time and returns the same bytes as a wrong
  password. The second needs a dummy Argon2 verification on the miss path —
  identical error messages do not hide a 50ms/50µs timing difference.

- **Sessions are the one entry-path lookup that is not cached.** Everything else
  tolerates five seconds of staleness. A logged-out token that keeps working for
  five seconds does not, so `session()` hits the database every request. That is
  the cost of a revocation that means something.

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

- **The second module changed the answer, which is why it was worth building.**
  The plan said "a second business module, proving cross-module integration *by
  event*" — sales would emit, the outbox would carry a promise, and a handler
  would post to the ledger a moment later. Two things fell out of trying it.

  The first was mechanical: `EffectHandler::deliver` takes a `PendingEffect` and
  nothing else. It cannot reach a `TenantDb`, because `TenantDb` lives in
  `spa-control` and `spa-control` depends on `spa-eventlog`, not the other way
  round. Making it possible meant a context type parameter threaded through
  `Dispatcher`, `OutboxJob` and their tests — about a hundred lines of kernel
  churn for a mechanism whose first genuine user (email, ZATCA clearance) does
  not exist yet.

  The second was the real one. The outbox exists because delivering to something
  outside this process cannot be atomic with the commit. Between two aggregates
  in the *same database* it can be, and choosing not to would mean an invoice
  can exist without its journal entry — a state needing a dead-letter queue, a
  sweeper, and an operator to explain it. That trade is worth taking against an
  email server. It is not worth taking against a table two schemas over.

  So `ledger::post_entry_in` is a new seam: one attempt, in the caller's
  transaction, account checks included. `post_entry` became the retry loop
  around it, which also moved those checks *inside* the transaction and closed a
  TOCTOU nobody had noticed. `a_failed_posting_leaves_no_invoice_behind` is the
  test that holds the line.

  What the original design was actually protecting — that a module should not
  hardcode which accounts a sale moves — survives intact, as
  `sales::PostingAccounts`. Phase 6 supplies that value from configuration. It
  was never the asynchrony that provided the decoupling.

- **Two fixtures were installing a module without entitling the tenant to it.**
  Adding the 404-if-not-enabled check to the module routes failed nine tests
  that had been green. `enable_ledger` in `tests/http.rs` created `proj_ledger`
  and never called `enable_module`, so every tenant in those tests had the
  ledger's tables and no entitlement to use them — a discrepancy nothing could
  detect until something read the entitlement. The gap was in the test harness,
  but a harness that cannot represent a tenant without a module is a harness
  that cannot test declining one.

- **The demo is a client, and that is the whole value of it.** Every step goes
  through the public API: sign up, install a chart, issue an invoice, take a
  payment. A seeder that called commands directly would have been shorter and
  would have proved nothing — the failure it needs to catch is an API that
  cannot do what the internals can.

  It caught one immediately. Signing the demo up with `["ledger"]` alone fails
  at `POST /v1/tenants/demo/sales/invoices` with `request.module_not_enabled`,
  which is how the "every module enabled" requirement went from a sentence in a
  document to something that cannot be quietly false. That experiment is worth
  repeating whenever this check changes: a demo that passes because it asks
  nothing is worse than no demo.

- **One list, because two things needed it.** `spa_api::modules()` replaced the
  `match` in signup. Not a `Module` trait: a trait would also have to carry the
  routes and the worker'"'"'s jobs, and neither can cross that boundary — a module
  must not depend on `spa-api` or `spa-worker`. So each composition root still
  lists what it composes, and only the *set* is shared. That is the part the
  demo needed to make "all of them" mean something.

- **`just demo` failed the first time a person ran it, and the second.** Two
  bugs, both of the same kind: a method with no caller.

  `relation "cluster" does not exist`. `bin/demo` assumed a migrated control
  plane; `ControlPlane::migrate` had existed since Phase 1 and *nothing called
  it*. Exactly the shape of the `request_visit` bug, and for the same reason —
  the tests all start from a migrated template, so the one path that starts from
  nothing was the one path nothing exercised.

  Then `duplicate key value violates unique constraint "cluster_pkey"`, found by
  the regression test written for the first bug. `register_cluster` was a bare
  `INSERT`, so a second run against the same deployment failed — and, worse,
  there was no way at all to change a cluster's capacity after registering it.
  Registering now declares configuration: `ON CONFLICT DO UPDATE` on the
  capacity and DSN columns, and deliberately **not** on `status`, so
  re-declaring a draining cluster does not put it back into service.

  The regression test builds on `Schema::sql("empty", &[])` — a database that
  has run nothing — and asserts it really is bare before bootstrapping it. A
  bootstrap test against an already-migrated fixture passes for the wrong
  reason.

- **Two halves of "enable a module", and only one of them existed.**
  `ControlPlane::enable_module` wrote the entitlement. Installing the read
  models was a separate step, inlined in `provision`. So enabling a module on a
  live tenant — which nothing could do over HTTP anyway — would have produced a
  tenant entitled to a module whose tables did not exist: routes found, every
  one of them failing on a missing relation.

  The test fixture had been papering over it, doing both steps by hand. That is
  the tell worth naming: **a harness with its own install path is a harness that
  can be right while the product is wrong.** `install_module` now does both, and
  the fixture calls it.

  The two orderings differ on purpose. `provision` entitles *before* installing,
  because the tenant is invisible until activation and early entitlement buys
  retry visibility. `install_module` installs *before* entitling, because the
  tenant is live and the entitlement is the thing that makes the routes
  reachable.

- **The dependency moved onto the module.** `sales::setup().requiring(&["ledger"])`
  replaced a hardcoded `if requested.contains("sales")` in signup. Three places
  needed the same answer — signing up, enabling later, and refusing to disable
  something another module is standing on — and two of them did not exist when
  the first was written. A `requires` field is not an abstraction; it is the
  question being asked in one place instead of three.

- **The one place that deletes a live tenant has three guards.** `reap_demo`
  refuses anything without an expiry in Rust, re-reads the row under
  `demo_expires_at <= now()` before dropping anything, and repeats the condition
  in the final `DELETE`. The gap between a sweep and a reap is exactly where a
  demo becomes a paying customer, and a test converts one in that gap to prove
  the re-read matters.

  The expiry instant is computed by Postgres, not by the process — same
  reasoning as event times. Two machines' clocks disagree, and the one deciding
  when a database is destroyed should be the one everybody already agrees with.

  Database first, row second. The other order leaves a database no row points
  at, which nothing would ever find again; this order leaves a row pointing at
  nothing, which the next sweep retries and `DROP ... IF EXISTS` absorbs.

- **A green test run was leaking three tenant databases, and had been for a
  while.** The API fixture recorded databases as `provision()` created them —
  but tenants born from `POST /v1/signups` were never on that list, and each
  signup test tried to drop `spa_tenant_acme`, a name that stopped being right
  when database names became id-derived rather than slug-derived. Dead cleanup
  that silently did nothing.

  The fix is to stop remembering: `cleanup` reads `SELECT database_name FROM
  tenant` out of the test's own control database and drops what is actually
  there. Asking cannot drift the way recording can. Found by counting
  `pg_database` before and after a run — worth doing again after anything that
  creates tenants.

- **An unauthenticated account takeover, found while wiring invitations up.**
  `set_password` ended with `ON CONFLICT (kind, handle) DO UPDATE SET secret`.
  That is the right shape for *changing your own password* and a full takeover
  for *registering a new one* — and the function had both kinds of caller.

  Signing up with somebody else's email overwrote their password and left the
  authenticator row pointing at **their** identity. Proved before fixing:

  ```
  signup with victim@acme.test          → 201
  login as victim, attacker's password  → 201
  read the victim's tenant as owner     → 200
  victim's own password                 → 401
  ```

  Public endpoint, no credential, complete loss of the account and everything
  the account owned.

  The fix is that the two operations are different operations.
  `register_login` inserts and refuses a taken handle; a future "change my
  password" gets its own function whose `WHERE identity_id = $1` is the clause
  that makes the difference. Signing up with an address that already has an
  account now has to prove it — which also turns out to be the *right product
  behaviour*, because the same person opening a second company should not need
  a second email address. `log_in` split into `authenticate` + `start_session`
  so signup and invitation-acceptance can check a password at exactly the cost
  a login checks one.

  What is worth taking from this: **the bug was in a function with a name that
  described neither caller correctly.** "Set the password" is true of both, and
  the difference between them is the whole of the security property. It sat
  there through the auth phase, the API phase and an authorization-matrix test
  suite, and was only found because a third caller needed the same code and its
  semantics had to be stated out loud.

- **Invitations without email.** The link is returned to the inviter, once, and
  they pass it on however they already talk to that person — which for a small
  business in this market is frequently better than mail, and does not wait on a
  decision about a provider. Sending it by email is an outbox effect (D9); the
  control plane has no outbox table, and adding one to carry a handler nobody
  has written would be building the mechanism before the need.

  What the link cannot do is become somebody else's account. Acceptance always
  binds to the invited address: an existing account for it must prove itself
  with its password, and a new one is created under that address and no other.
  Wrong password does not burn the invitation — a typo should not become a
  support ticket.
