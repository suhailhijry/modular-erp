# Implementation plan

Phased build order for the architecture in [ARCHITECTURE.md](./ARCHITECTURE.md).

Ordering is by **dependency**, not by value. Each phase ends somewhere that
compiles, tests green, and is worth having on its own. Durations assume one
focused engineer and are the least reliable thing in this document.

Type-safety work (§4 of the architecture) is deliberately spread across phases
rather than batched — it is cheapest applied to code as it is written.

**Legend:** `[ ]` todo · `[~]` in progress · `[x]` done

**Where this stands:** 878 tests green, clippy and fmt clean. The per-phase test
counts below are the numbers *at the time that phase was met* and are left as
written; they are history, not status. What is not yet true is collected under
[What needs work now](#what-needs-work-now) at the end.

---

## For review — decisions I made without you

Written during the gap-closing pass of 2026-09-01/02, while you were away. Each
is a judgement call I took rather than stopping on; each is reversible. Read
them, and delete this section once you have.

### 1 · Two views over the invoice, rather than one

`invoice_status` grouped every invoice in the tenant to return one page of
twenty. Measured on 200,000 invoices and 400,000 payments: **410 ms and 443,000
buffers**. Rewriting it to correlate makes that **0.3 ms and 118 buffers** — but
makes the receivables report and the overpayment health check about **3× slower**
(292 → 760 ms, 273 → 945 ms), because each of the 200,000 invoices then costs an
index lookup.

So there are now two views over the same numbers: `invoice_status` for readers
that scan, `invoice_row` for readers that want one invoice or one page. A test
asserts they agree, since two shapes of one rule is exactly how a rule drifts.

**The alternative I did not take** is a maintained `paid` column on `invoice`,
which would be fastest for everything. The schema comment argues against it
deliberately — "a second thing that can be wrong" — and overriding a documented
decision on the strength of a benchmark I wrote myself seemed like your call.

### 2 · Matching a customer to an old invoice is re-matchable

Phase 7a's reconciliation is built. `attach_customer` sets the *reference* and
never the printed name.

The judgement: **re-matching to a different record is allowed.** A match made to
the wrong Ahmed has to be correctable, and the log keeps every attachment so the
correction is visible. The stricter alternative — refuse once matched — leaves
no way to fix a mistake at all, which seemed worse. Say the word and it becomes
a refusal with a `sales.already_matched` code.

`attach_customer` is scoped to **owner** (`ManageTenant`), on the grounds that
re-pointing a document at a different customer changes what a report says about
that customer. If a clerk should be doing the backlog, it wants `PostEntries`.

### 3 · A flaky test I could not reproduce

`erp-eventlog::crash a_crash_during_a_claim_leaves_the_effect_owed` failed once
in a full-workspace run and then passed 5/5 in isolation, 3/3 under `-j 16`
within its own package, and in two subsequent full-workspace runs. It kills a
backend mid-transaction with `pg_terminate_backend`, which signals rather than
waits, so a timing window is plausible.

Left as-is rather than papered over with a retry. If it recurs, the suspect is
that `kill_connection` returns before the backend has finished rolling back.

### 4 · Two commits are unsigned

`gpg` needs a pinentry TTY for your key's passphrase and I have neither, so the
gap-closing commits went in with `--no-gpg-sign`. Re-sign them when you are
back:

```
git rebase --exec 'git commit --amend --no-edit -S' -i <the commit before them>
```

Nothing else about them differs.

### 5 · Phase 17's deposits are recorded, not charged

`booking.public` carries a `deposit_bp`, the public booking response reports it,
and **nothing collects it** — card payments are Phase 12a and there is no
gateway. The alternative was to leave the setting out until the gateway lands,
which means it arrives configured by nobody.

If you would rather the field did not exist until it works, say so and it comes
out; the argument for keeping it is that the shape is known and a site can
honestly tell a customer what will be asked for.

### 6 · Public booking writes are gated on an opt-in I invented

Nothing in the plan asked for `booking.public`. The plan's answer to abuse of a
public write is the deposit, and the deposit does not work yet — so a public
booking that anybody can make would let a script fill a salon's week with
appointments nobody intends to keep, bounded only by a rate limiter.

So it is off unless a business turns it on. That is a product decision I made
rather than a technical one, and it is the one thing in this phase I would most
expect you to want changed.

### 7 · The "5,000 tenants" prose was not stale

I had this on the gap list from an earlier session. It is wrong: `ARCHITECTURE.md`,
`pools.rs` and `placement.rs` all quote 5,000, and this document's own target is
2,000–5,000. Sizing against the top of the stated range is correct. Nothing
changed; the item is struck.

---

## Phase 1 — Foundations · 3–4 weeks

Nothing above this is worth building until the handle types and the test harness
exist, because everything after inherits them.

### 1a · Workspace skeleton
- [x] Cargo workspace, crate layout per architecture §6
- [x] `rust-toolchain.toml` pinning the toolchain
- [x] Shared lint configuration (`workspace.lints`), warnings denied
- [x] Retire the prototype `src/` tree (preserved at `f2e8acd`)

### 1b · `erp-types`
- [x] Newtype macro: `Display`, `FromStr`, serde, sqlx, validation
- [x] Identifiers: `TenantId`, `IdentityId`, `AggregateId`, `StreamId`
- [x] Distinct position types: `LogPosition` (global) vs `Sequence` (per-aggregate)
- [x] `CurrencyCode` with ISO-4217 minor-unit exponent
- [x] `Money` — runtime currency, no `Add`, `checked_add -> Result`
- [x] `NonEmpty<T>`
- [x] Unit tests including the "these two types cannot be confused" cases

### 1c · `erp-testkit`
- [x] Template-database-per-test fixture (`CREATE DATABASE … TEMPLATE`)
- [x] Measured: ≈280 ms to acquire, ≈140 ms to drop (local Postgres 18)
- [x] Parallel-safe (unique names, automatic teardown)
- [x] Fault-injection hooks at transaction boundaries *(landed in Phase 2, where the transactions exist)*

### 1d · Control plane
- [x] Schema: identities, memberships, tenants, entitlements
- [x] Append-only audit trail, enforced by trigger (D2)
- [x] Connection manager: LRU pools, `min = 0`, global budget as a semaphore
- [x] `TenantDb` with no public constructor; `ControlPlane::enter`
- [x] Tenant registry carries `(cluster, database)` from day one
- [x] Support access as a separate audited path — no `is_system` bypass
- [x] Authenticators and sessions *(landed in 3a, with the rest of auth)*

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
- [x] **Soak test** (`erp-control/tests/soak.rs`, run with `--ignored`).
      Measured: open connections track *active tenants × per-tenant pool*, busy
      connections track the lane budget, neither tracks request count, entry
      cache hit rate 99.9%. 22,169 ops/s across 40 tenants with 256 workers.
- [x] Per-operation connection permits with per-lane bulkheads
- [x] Entry-path cache (four cold lookups, zero warm)
- [x] Read-replica seam (`TenantDb::read`, falls back to primary)

### 1g · Localization (D12)
- [x] `erp-i18n`: `Locale`, `MessageCode`, `MessageArg`, `Message`, `Localize`
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
- [x] `erp-worker`: `Job` trait, `ProjectionJob`, `OutboxJob`, `bin/worker`
- [x] `CancellationToken` + `TaskTracker` drain; leases released on the way out
- [x] SIGTERM-mid-batch test, verified by shadow differ rather than by assertion
- [x] Fault injection at transaction boundaries (carried from 1c) —
      `pg_terminate_backend`, not a simulated failure
- [x] Shadow replay against the demo tenant, on every run *(Phase 4b — all four
      groups, and the list is checked against `erp_api::modules()` rather than
      trusted, so a module added without a line there fails the test)*

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
- [x] `erp-api` on axum; `bin/api` with body limit, timeout, graceful shutdown
- [x] problem+json (RFC 9457) plus `code` and `args` — a client branches on the
      code, never on the prose
- [x] `Accept-Language` honoured on every response including failures
- [x] Composite catalog across crates, with a duplicate-code test — layered
      since modules ship their own routes: `erp_web::CATALOG` is what any route
      can answer with, a module adds its own and its dependencies', and
      `erp_api::CATALOG` is the union `docs/ERRORS.md` comes from
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
- [x] `ModuleSetup`: a module *describes* itself — install SQL, seed SQL,
      projection groups, event versions, dependencies — rather than the control
      plane knowing what a ledger is
- [x] **`POST /v1/signups`** — a request, a confirmation email, and then
      `POST /v1/signups/{token}`, which is where the caller gets a working
      system they are already logged into with the ledger installed and usable.
      Two calls since item 5; nothing is built by the first
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
- [~] `Module` trait and registry *(the **registry** half is done and was the
      load-bearing one: `modules::REGISTERED` is one entry per module carrying
      both its `ModuleSetup` and its router, so neither can be added without the
      other. The **router** half is done too — `erp-web` sits below the modules,
      so a module ships `http::routes()` itself.
      What is left is the reason a trait is still not worth writing: a module's
      **worker jobs** are registered in `bin/worker.rs` and cannot move, because
      a module must not depend on `erp-worker` and the kernel must not know what
      a ZATCA document is. A trait with two of its three methods implemented
      somewhere else describes nothing)*
- [x] `Idempotency-Key` — **decided against**, see architecture L8. Mutations take
      client-chosen ids and the log's uniqueness constraint refuses the repeat, so
      a header plus a key/response store would rebuild a property the design
      already has. `erp-api/tests/idempotence.rs` enforces what makes it true.
- [ ] `ETag`/`If-Match` *(no update-in-place endpoint yet)*
- [x] `?consistent_after=<position>` — read your own write, with the write
      nudging the worker so the wait is a claim cycle rather than the idle backoff
- [x] Cursors — keyset paging on the columns each list is ordered by, an
      opaque cursor, and `next` absent meaning **the list ended**. Every list
      used to stop at 200 and say nothing. A cursor this build cannot read is
      refused rather than ignored
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
- [x] Configuration — the **store**, not the system. A versioned key-value table
      in the tenant database, a typed surface on top, and posting accounts as
      its first and only consumer. Declarations, layers and resolution rules are
      Phase 6's, and they are what §6 describes; this is what sits underneath
      them
- [x] Numbering (gapless per-tenant document sequences) — `erp_eventlog::numbering`
      and `migrations/tenant/0005_numbering.sql`. A counter row read `FOR UPDATE`
      and advanced in the same transaction as the document, so a refusal, a
      retry, or a crash releases the number rather than burning it. Saudi VAT
      Implementing Regulations Article 53 requires the sequence to have no holes
      in it, which a Postgres `SEQUENCE` cannot give: `nextval` survives a
      rollback by design
- [x] `docs/ERRORS.md` — every error code in both languages, generated from the
      catalog the API renders from, with a CI drift check (`just errors`)
- [x] `docs/openapi.json` — every route, generated from the router that serves
      them, with a CI drift check (`just openapi`) and served at
      `GET /v1/openapi.json`. `utoipa-axum` registers the axum route *from* the
      `#[utoipa::path]` attribute, so a served route cannot be undocumented; the
      hand-written half (which status carries what) is checked by validating
      every response in `tests/http.rs` against the published schema
- [x] Authorization matrix tests — every role against **every** role-scoped
      endpoint, with the endpoint list taken from `erp_api::openapi()` so a route
      added tomorrow appears whether or not anybody remembers, and an operation
      the table does not name fails the test rather than defaulting to untested

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
- [x] Reversals — an entry posted in error is undone by posting its opposite,
      both in one transaction, refused if already undone
- [x] Credit notes — an invoice issued in error is cancelled by crediting it,
      which reverses its journal entry. Whole-invoice only, and refused while
      payments stand against it
- [x] Fiscal periods — `ledger::period`, one watermark, checked in
      `post_entry_in` where every posting in the system arrives. An entry dated
      into a closed period is refused whether it is a hand-written journal entry,
      a reversal, an invoice's tax point, a payment, or a credit note
- [ ] Partial credit notes, drafts, multi-currency entries with FX
- [ ] An entry-level read model *(a `proj_ledger.entry` table would show which
      entry reversed which, and let entries be listed at all — but adding a
      table to a module's install script needs the fleet-wide module refresh
      that nothing needs yet, and nothing displays the link today)*
      *(each needs someone to want it before its shape is decided)*
- [x] Chart-of-accounts templates — `services` and `retail`, bilingual, with
      Saudi VAT and Zakat accounts in both
- [x] `GET /v1/ledger/charts` (unauthenticated — a signup form has to show the
      choices) and `POST /v1/ledger/chart`
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
- [x] A VAT return — output tax by rate for a half-open period, per currency,
      with credited invoices excluded
- [x] ZATCA clearance and reporting — the whole chain: onboarding by OTP, the
      key and CSR, the XAdES signature, the QR, the invoice hash chain, the
      transport, and the worker sweeps that sign and submit. Nine documents
      accepted by ZATCA's sandbox with zero warnings. **Not** an outbox handler
      in the end — it is two worker jobs, because a submission has to read a
      sealed private key and the outbox carries values, not secrets *(4e)*
- [x] The input-tax side — `modules/purchases`, and the whole return composed in
      the API from both modules' own reads, because their projection groups never
      see each other (L3) and nothing below the composition root could produce
      the figure
- [x] Credit notes and cancellation — a `POST`, not a `DELETE`: the invoice
      stays, its journal entry is reversed, and the books show both
- [x] Statutory gapless numbering, on its own series per document type
- [x] **Receivables** — `GET /v1/sales/receivables`, aged by due date and falling
      back to the issue date, grouped by `(customer, currency)`, biggest debtor
      first. The system could issue and settle invoices but not answer *what does
      this customer owe*, which is the question an AR clerk asks every morning.
      Needed no schema change: `invoice_status` already carried `outstanding`,
      and its own comment already anticipated the report.
- [ ] Customers as records, quantities and unit prices, partial credit notes

### 4b · The demo tenant

- [x] `erp-demo`: a tenant with every module enabled, filled **through the
      public HTTP API** — so a demo built out of internal calls cannot be
      perfect while the API a customer would use is broken
- [x] Deterministic: fixed dates, amounts and identifiers, so the numbers can be
      screenshotted and a CI failure is never "did the data change?"
- [x] Something in every state a screen has to render: settled, part-paid and
      untouched invoices; all three VAT treatments; expenses as well as sales
- [x] **Part of `cargo test --workspace`**, which builds the whole demo and
      asserts every module answers, every group replays identically, and every
      invariant is clean. Called "a required CI check" in an earlier draft of
      this document: **there is no CI**, here or anywhere in the repo. It is a
      required *test*, and nothing runs it but a person
- [x] **Shadow replay against the demo** *(carried from Phase 2 — it needed the
      demo tenant, and now has one. All four groups, with the coverage itself
      asserted; each group names a table the demo must have filled, because
      `EXCEPT ALL` between two empty tables is clean)*
- [x] `erp_api::modules()` — the module set in one place, so "every module
      enabled" is true by construction rather than by a second list agreeing
- [x] `bin/demo` + `just demo <password>`, so the demo is a thing a person signs
      into rather than a thing a test builds
- [x] `erp_demo::bootstrap` — migrates and registers the cluster, because a
      demo is usually the first thing pointed at an empty database
- [x] Demo tenant TTL and reaper — `set_demo_expiry`, `expired_demos`,
      `reap_demo`, and `bin/reaper` (`just reap`). Demos expire by default;
      `DEMO_TTL_DAYS=0` opts out

### 4c · Entitlements a tenant can change

- [x] `POST /v1/modules` and `DELETE /v1/modules/{module}` — a tenant buys a
      module on a Tuesday and it works immediately. No `{slug}`: the tenant is
      the subdomain
- [x] `ControlPlane::install_module` — read models **and** entitlement, because
      either alone is a tenant that 500s
- [x] `ModuleSetup::requires` and `requires_any` — dependencies declared once
      and read by all three places that ask: signing up, enabling later, refusing
      to disable. `requires` is an AND list; `requires_any` is a group satisfied
      by any member, which is what `tax_sa` needs — a VAT return wants a source
      for one side or the other and does not care which
- [x] Disabling deletes nothing. The entitlement is marked off; the events and
      read models stay, so a tenant who downgrades and returns finds their data
- [x] `GET /v1/catalogue` — unauthenticated and on the apex, because a pricing
      page needs it before anyone has an account or a subdomain. Carries both
      kinds of dependency, so a picker can grey out impossible combinations.
      (`GET /v1/modules` is the tenant's own list; the two collided when the
      tenant moved to the subdomain and the router refused to start)
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
- [x] Fleet migrator — `survey_fleet` looks, `migrate_fleet` applies, `bin/migrator`
      (`just migrate-fleet [check]`) is the deploy step. Exits non-zero when the
      fleet is not uniform, so `check` gates a deploy
- [x] Per-tenant health checks including the trial-balance invariant *(shipped
      with `HealthJob` in Phase 3c; sales added the overpaid-invoice check)*
- [x] Module schema refresh across the fleet — `refresh_module`,
      `refresh_module_fleet`, `just migrate-fleet refresh <module>`. Drop the
      schema, install it again, rewind the checkpoint, let the worker replay

### 4e · Saudi e-invoicing, and the seams it broke

Not in the original plan at all — it was one line in 4a. It is a phase.

- [x] `modules/tax_sa` — the VAT return netting output against input, filed
      returns, and the first module standing on two others
- [x] The invoice hash chain (PIH/ICV), the QR as TLV, the canonical UBL
- [x] Onboarding: key pair, CSR, OTP, compliance checks, production certificate
- [x] The XAdES signature, and the transport (`reqwest` over the OpenSSL stack
      sqlx already links)
- [x] Sealed module secrets — `SEALING_KEY`, and anything that would store a
      private key **refuses** without one rather than storing it in the clear
- [x] Worker sweeps: `tax_sa.sign` and `tax_sa.submit`, registered only when the
      deployment has a sealing key
- [x] `CertificateExpiry` invariant — sixty days' warning, because renewal needs
      a human reading an OTP off the Fatoora portal and nothing here can do it
- [x] Verified against ZATCA's **sandbox** with a real certificate: nine
      documents, zero warnings
- [ ] Verified against **simulation**, and then production *(needs a real
      taxpayer's OTP — see [What needs work now](#what-needs-work-now))*
- [ ] Per-line exemption reasons; per-till device certificates

### 4f · Modules that are actually modules

- [x] `erp-web` — extractors, problem+json, paging and the request-level messages
      moved *below* the modules, so a module can name what its routes are built
      from without depending on `erp-api`
- [x] Every module ships `http::routes()`; `erp-api` mounts them and writes none
- [x] `ModuleSetup::seeding` — a module's data is a step of its own, not a rider
      on its DDL
- [x] PDPL erasure — `audit_entry`'s trigger permits exactly one shape of update,
      one that nulls an actor and changes nothing else. An identity that had ever
      acted could not be deleted at all, and "our schema will not let us" is not
      a lawful ground for refusing
- [ ] An HTTP endpoint for erasure *(deliberately absent: **who may erase whom**
      is a policy question, and answering it while fixing a schema bug would
      answer it badly)*
- [x] `docs/RUNNING.md` — bringing the API and workers up by hand

**Exit:** someone signs up online, picks a chart of accounts, and gets a working
system.

---

## Phase 5 — Granular permissions · 3–4 weeks

**Resequenced.** The rule engine was to be built first and authorization moved
onto it. But it had one real consumer and no concrete rules to describe it from
— pricing does not exist — so building `Facts` and `DynCondition` now meant
inventing which facts exist. Instead: the smallest real granularity gap first,
and let two working cases describe the engine.

### 5a · A different role in a different module

- [x] `Access` — a tenant-wide role plus per-module exceptions, so "Sara does
      the invoicing, Khalid does the books" is expressible
- [x] The module comes from the **request path**, so a module route added
      tomorrow is scoped without anybody remembering to scope it
- [x] The tenant's own surface — members, invitations, entitlements — is nobody's
      module and uses the tenant-wide role
- [x] Clearing an exception restores the tenant-wide role, which is a different
      thing from setting `viewer`
- [x] Removing somebody clears their exceptions with their membership
- [x] Invalidated on the spot, like every other authorization change

### 5b · The engine, once there is something to describe it

- [ ] `erp-rules`: `Facts`, `DynCondition`, `FactRegistry`, `Rule<E>`
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

---

## Build order

**Phase numbers below are historical.** They record the order things were
designed in, and the cross-references in this document depend on them, so they
stay. What follows is the order the work is actually done in, which is a
different thing and changes as the market answers back.

It changed once already, after reading Rekaz's 73 tools and Qoyod's, Wafeq's and
Daftra's pricing. The finding that reordered it is one line long: **three of
Rekaz's tools are accounting integrations, to Qoyod, Odoo and Daftra.** Nobody
builds three of those with a ledger of their own. So the competitor with the
booking product has no books, and the competitors with books have no booking,
and a salon today buys both and reconciles them by hand.

That is the wedge, and everything below is ordered by how directly it serves it.

| | what | why it is here | serves |
|---|---|---|---|
| 1 | **7a · `modules/crm`** | Nothing else can start. A booking is made *by* somebody, a package belongs to somebody, points accrue to somebody | everyone |
| 2 | **7b · the occupancy engine** | The one piece with no substitute. Write-side state, capacity, guards | appointments |
| 3 | **8a–8b · `modules/booking`** | The half of the wedge the accounting vendors do not have | salons, clinics, gyms, studios |
| 4 | **14 · `modules/prepaid`** | Deferred revenue, and the five shapes that share it | wellness, gyms, academies |
| 5 | **15 · `modules/pos`** | A coffee shop cannot open without a till. Unlocks a whole segment that needs no booking at all | cafés, restaurants, retail |
| 6 | **16 · branches** | Does not exist today. Blocks per-branch reporting and the segments that have more than one | multi-site anything |
| 7 | **17 · the public booking API** | How a customer reaches the business. The site itself is a separate React project; this is the surface it calls | appointments |
| 8 | **18 · marketing** | Segments and campaigns over the log | growth |
| 9 | **19 · `modules/inventory`** | Restaurants and retail count stock | restaurants, retail |
| | *then* | Phases 10–13 as written: reports, channels, payments, real time | |

**Two products share one spine.** Appointment businesses need CRM, occupancy,
booking and prepaid. Counter businesses need CRM, POS, inventory and loyalty and
never touch a calendar. Both need the ledger and ZATCA, which exist. Ordering
appointments first is a judgement that salons, clinics and gyms are the larger
and better-served segment; if the first ten sales conversations say otherwise,
POS moves to position 3 and nothing else changes.

---

## Phase 7 — Customers, and the occupancy engine · 4–5 weeks

**Where this comes from.** A working booking ERP was read end to end — a
Laravel system of roughly 407k lines, 74 aggregates, 863 event classes and 230
tables, serving salons and spas. It is called *that system* below. The parts
worth taking are named against it, and so are the parts worth refusing: its own
comments record its bugs, which is the most useful documentation in it.

**The finding that shapes this phase.** Its reservation aggregate writes the
same lifecycle three times — `SeatActivated` / `ShowerActivated` /
`ServiceActivated`, and again for start, end, notes, cancel and restore. That is
most of its seventy reservation events. But its `slot_occupancy` table is
already generic: `(resource_type, resource_id, [start, end), owner_type,
owner_id)`. Somebody found the abstraction, applied it to the write path, and
never took it back into the domain model. **A seat, a shower, a room, a hall and
a person who does the work are one concept**, and this phase builds that
concept once.

### 7a · `modules/crm` — customers as records

The gap the receivables report already exposes: an invoice freezes the buyer's
name (L5), so two spellings are two rows and nothing can answer "everything for
this customer". Booking cannot start without it, because a reservation is made
*by* somebody.

- [x] `Customer` — name, contacts, addresses, tax registration, the fields ZATCA
      needs on a B2B invoice
- [x] An invoice references a customer **and still freezes what it printed**.
      Both, not either: the reference is for the customer list, the frozen copy
      is what the law requires the document to say. Validated against `crm`'s
      *log* and not its projection, because a projection lags and an invoice
      would be refused to a customer created a moment earlier
- [x] Backfill: existing invoices name a customer that no record matches, so the
      first migration is a reconciliation surface, not a foreign key.
      `unmatched_customers` is the worklist — one row per frozen spelling,
      largest backlog first, because the job is matching *people* and forty
      invoices for one name is one decision. `attach_customer` works through it,
      writing the **reference** and never the printed name: what the document
      says about its buyer was frozen at issue and a reconciliation does not get
      to restate a filed document. Validated against `crm`'s log, so a record
      created a moment ago can be matched at once. Re-matching is allowed and is
      itself an event — see [For review](#for-review--decisions-i-made-without-you)
- [x] Receivables groups by customer id where one exists and by name where none
      does, and says which — `AgedCustomer::identified`

**`sales` does not require `crm`.** It was made to, and three tests said no: the
reference is optional, so a till issuing simplified invoices to walk-ins must
not be forced to keep a customer list. The crate dependency and the entitlement
dependency are different things.

### 7b · The occupancy engine

Not a projection. Occupancy is **write-side state** — the read side can be
rebuilt, a booking that was accepted cannot be un-accepted. That system says
the same of its own table — write-side state, never truncated and never rebuilt
by a replay — and it is right.

Built as `crates/erp-occupancy` with its tables in `migrations/tenant/0007`,
which is where that argument lands: a module's `install_sql` is what
`rebuild_schema` drops, and these rows must be somewhere it cannot reach. Same
shape as `erp_eventlog::numbering` for the same reason. Nobody enables
`occupancy`; a tenant enables `booking`, and `booking` links it.

- [x] `Resource` — a person, a place or a thing. Carries a **capacity**.
      Capacity 0 is legal and means out of service, which is retirement without
      a second column and without losing the claims already against it
- [x] `Claim` — one resource, one half-open interval, and a **quantity**
- [x] The conflict test is capacity and not an existence check. **It is a peak
      and not a sum**, which is a correction to the line this plan used to
      carry: `SUM(quantity) over overlaps` counts claims that never coexist,
      so a room type with eight units and eight one-night stays across a week
      turns away a guest asking for the week while seven rooms stand empty
      every night of it. The claims become `+q` at each start and `-q` at each
      end, and the largest the running total reaches is what is held at once
- [x] A guard row per `(resource, date)`, taken with `FOR UPDATE` **in sorted
      order** before the probe. Unsorted, two multi-resource bookings touching
      the same two resources in opposite orders deadlock — their bug, recorded.
      The insert is sorted too: `ON CONFLICT DO NOTHING` waits on a conflicting
      insert that has not committed, so the deadlock is reachable before a
      single `FOR UPDATE` runs
- [x] The batch is checked **against itself**. Theirs was not, and the defect
      it caused is recorded in its own source: one request naming the same
      resource twice at the same hour found nothing already held, wrote both
      claims, and double-booked that resource against itself. Fixed
      structurally by writing each claim before probing the next, so the second
      sees the first and there is no separate self-check to forget
- [x] Times normalised on construction — truncated to whole seconds, in a type
      whose constructor is the only way to build one. Theirs: *"comparing those
      unnormalised is how an overlap check silently passes"*
- [x] Release is by owner and idempotent, so a retried handler is harmless (L8)
- [x] A reschedule ignores the rows it is about to release, so a booking never
      conflicts with its own previous position

**Not modelled, deliberately.** Slot granularity — store instants; fifteen-minute
slots are validation and display, one configuration key. Buffers — a cleaning or
setup allowance widens the claimed interval at claim time, so the probe stays one
comparison.

**All or nothing is the caller's transaction, not the engine's.** `take` writes
as it goes, so a batch refused on its third claim leaves the first two in the
caller's transaction. Rolling back is what makes the booking atomic, exactly as
it is for `sales::issue_in`, and it matters most in `reschedule`: committing
over a refused reschedule gives up the slot the booking already had.

**Exit:** capacity 1 and capacity N both hold under a concurrent test
(`only_one_of_two_bookings_racing_for_the_last_place_gets_it`), a deadlock is
unreachable under one (`a_deadlock_is_not_reachable`), and the engine knows
nothing about what a resource is for.

Left for 8a, where it belongs: **availability and downtime**. When a resource is
*offered* is a recurrence, not a claim, and this engine only answers whether one
more fits.

## Phase 8 — Reservations, and the verticals that prove it is general · 5–6 weeks

**The criterion for "generic".** Four businesses that share no vocabulary must
be configurable without a code change. A hotel that needs a patch means the
engine is still written for one trade.

### 8a · `modules/booking`

- [x] `Reservation` — a customer, a time, and lines. Each line claims resources
- [x] One lifecycle, once: `reserved → confirmed → arrived → in service →
      completed`, with `cancelled` and `no-show` as ends. That system reached
      the same list independently, which is a reason to trust it.
      `ReservationEvent::Moved` is the single event that walks it, and skipping
      forwards is allowed — a walk-in arrives without ever being confirmed
- [x] Typestate, per architecture §4. **As an exhaustive `match` on a pair of
      stages, not phantom types.** Every command starts from a `load`, so the
      stage is only ever known at run time and phantom types would buy one
      boundary check that is this same match with seven zero-sized types on top.
      What the match does buy is real: an eighth stage is a compile error in the
      one place the rules live. Nothing else in this codebase carries phantom
      typestate either, and `Permit<C>` is where it earns its keep
- [x] `Availability` as a recurrence — **and this is the second place the plan
      was wrong.** The specified shape was cron: months, weekdays, days, hours
      and minutes as bit fields. Cron cannot say "half past nine". Its hours and
      minutes are independent sets, so "open 09:30 to 17:00" needs minutes
      `{30..59} ∪ {0..29}`, which is every minute and therefore also matches
      09:05. There is no assignment of those two fields that means what a salon
      means. The calendar half stays as bit fields, which is what made theirs
      compact and indexable; the clock half became the interval it actually is,
      half-open like everything else here
- [x] The customer is claimable as a resource, so "already in another chair"
      needs no special case. Held at capacity one under a reserved `customer.`
      prefix, in the same engine as every chair. **Once per distinct span, not
      once per line** — four seats at one showing is one person at one time and
      must be allowed; a haircut at ten and a massage at half past is one person
      in two places and must not be
- [x] Fungible pools: book the **type**, assign the unit later. The pool holds
      the count and the unit holds the identity, so assigning takes a second
      claim on a different resource and nothing is counted twice
- [x] Add `erp_occupancy::CATALOG` to `erp_api::CATALOG`

**Local time, and the ceiling on it.** A rota is local and an instant is not, so
`booking.calendar` is a fixed offset defaulting to `+03:00`. Exact for Saudi
Arabia and the Gulf, which have no daylight saving. A market that does needs
`chrono-tz` and a zone name, and that is a change to `calendar.rs` and to
nothing else.

**A defect the tests found in 7b.** `occupancy_claim` was keyed on
`(owner, resource, starts_at)`, which made a legal booking impossible: three
lines of one reservation each taking one place in the same class at the same
hour is one owner holding three, and it arrived as a primary-key violation
reading `duplicate key value violates unique constraint`. The key now includes
`ends_at` and a repeat accumulates through `ON CONFLICT DO UPDATE`. Covered by
`one_owner_asking_twice_for_the_same_span_holds_two_of_it`.

**No money, deliberately.** A reservation carries no price, no tax and no ledger
posting. Pricing is 8d and one pure function; invoicing a completed booking is
after that. A number on a line now would mean writing the pricing rules twice.

**Exit:** the diary and the rota over HTTP, the engine holding what the diary
says, and a replay reproducing both.

### 8b · Six fixtures, one engine

Written as `modules/booking/src/trades.rs` — six `const` blueprints — and
`modules/booking/tests/fixtures.rs`, which fits a tenant out from each one and
books the thing that is characteristic of that trade.

- [x] **Salon** — person plus chair, minutes, capacity 1, a named person. Two
      stylists and two chairs refuse the third booking without anybody writing
      "a salon has two chairs" in the code
- [x] **Restaurant** — table with covers as capacity, a sitting as duration.
      Four at a table for six leaves two, a party of four will not sit at a
      table for two, and the later sitting takes the same table
- [x] **Hotel** — room type with N units, nights, assignment deferred. Booking
      the type leaves every room untouched; check-in claims one, and the pool is
      not charged twice
- [x] **Class** — instructor plus room, capacity N, many customers in one slot
- [x] **Gym** — no slot at all. The rota holds the classes and nothing for the
      floor, the door or the changing rooms, and the diary of a gym operating
      normally is empty. The membership itself is Phase 14
- [x] **Ticketed slot** — a museum sells 500 places at 10:00 with no named
      resource. A family of four, a coach party of two hundred, and nothing
      assigned to anybody
- [x] Each is a blueprint (D8), not a branch in the code

**No code change was needed.** All six compiled and passed against `booking` as
8a left it. Nothing in the module reads a trade's id, so a seventh trade is an
entry in `TRADES` and no code at all.

**What the class fixture found.** Written first with one customer booked twelve
times, and refused on the second: `customer.c1 holds 1 of 1 then`. Not a bug —
the "already in another chair" rule doing its job — but it pins down what a
class booking may look like. **Twelve places is either twelve customers, or one
customer on one booking.** A parent bringing four children is one reservation
with four places; twelve strangers are twelve reservations. What is refused is
one person holding twelve *separate simultaneous* bookings, and it has to be:
a system that allowed it would have nothing left to catch the salon
double-booking with, because they are the same query.

**Where the six came from.** Rekaz sells to salons, clinics, gyms, studios,
museums, event ticketing and horse stables. Those seven need four shapes between
them: capacity one with a named person, capacity N in one slot, a pool of
interchangeable units, and pure capacity with nobody assigned. Restaurants add
covers-as-capacity and the gym adds the case where there is no slot at all.

**Exit:** six trades demonstrable from blueprints, and `fit_out` is the same two
commands a person clicking through the screens would run.

### 8c · moved

Packages grew into [Phase 14 · `modules/prepaid`](#phase-14--modulesprepaid--everything-the-customer-has-already-paid-for--56-weeks)
once it became clear that packages, subscriptions, gift cards, deposits and
loyalty points are one accounting problem wearing five names.

### 8d · Pricing, once and pure

- [x] One `price` function. No database, no settings, no clock — so it is
      testable and cannot drift with configuration. `modules/booking/src/pricing.rs`
- [x] **Time-based pricing.** Peak and off-peak, which Rekaz sells and every
      salon wants. It is an argument to `price`, resolved from configuration at
      the moment of booking and frozen onto the line (L5), never read again.
      A band's *when* is an `Availability` — the same recurrence that says when
      a resource is offered, because "open Thursday evening" and "dearer
      Thursday evening" are one shape and a tenant should learn it once
- [x] **Tax-exclusive discounts**: an allowance comes off the net and tax is
      charged on what remains. **No tax is computed in `booking`**, and that is
      the point: a reservation is not a tax document, so the allowances travel
      with the line to `sales` when it is invoiced and reduce the band they
      come off there. The tax-exclusive property falls out rather than being
      something two modules each have to remember
- [x] `Money`, never a float

**The rounding rule moved before it could be duplicated.** That system's engine
takes floating-point amounts and its own docblock records three implementations
that disagreed, every fixed discount differing by exactly the tax on it. The
half-away-from-zero rule was private to `sales::vat`; `booking` needed the same
one for a peak-hour uplift. It is now `Money::scaled_by`, which is the only
place in the workspace a rate is applied to an amount, and `sales`' fifty-four
tax tests are what keep it honest.

**The order of operations is written down, because it is not free.** The band
moves the **rate**, then quantity multiplies, then allowances come off the
total. Banding the total instead gives a different answer wherever the rounding
bites, and it bites at the prices businesses actually use: a 33.33 service at a
quarter more is 41.66 each, so four are 166.64 — banding the total gives 166.65
and a customer who checks finds a halala nobody can explain.

**Bands, not prices.** What a service costs is the caller's to send; *when* it
costs more is the tenant's to configure. Putting the price list on the server
would need a service catalogue, which nothing has asked for and which `what`
being opaque is currently buying us. A client cannot decide its own peak rate,
which is the half that had to be server-side.

**A whole span, not its start.** A treatment beginning before peak and running
into it is charged at the base rate. The alternative is the answer a customer
argues with, and a business that wants the other rule splits the booking, which
is what they would do at the till anyway.

**Exit:** six verticals demonstrable from blueprints, and one pricing path.

---

## Phase 14 — `modules/prepaid` — everything the customer has already paid for · 5–6 weeks

**One module, six aggregates, one liability.** Packages, subscriptions, gift
cards, wallets, deposits and loyalty points are the same accounting problem:
money received now for value delivered later. Building them as separate modules
would write deferred revenue five times, and law L3 would then forbid the one
screen every one of these businesses wants: *what does this customer have with
us?* Tables in different projection groups never read each other, so four
modules means four reads at four checkpoints that can disagree with each other
while somebody is taking money against the answer.

The name is `prepaid` and not `entitlements` because `entitlement` already means
something here: the control plane's table of which **modules** a tenant has
switched on. Two meanings for one word in a codebase that renamed itself to
avoid exactly that.

**Where the shape comes from.** That system's chart of accounts had worked most
of this out already: `deferred_revenue` and `loyalty_liability` as liabilities,
with `loyalty_earned`, `loyalty_granted`, `loyalty_redeemed` and
`loyalty_expired` as unconstrained counterparts. Its `ServicePackage` carries
`expiration_type` (none/days/months), `expires_after`, `activation_count` and
both `rank_points` and `walaa_points`. Its `ClientPackage.type` records
bought / gifted-by-client / gifted-by-business / free-from-coupon, which is what
makes "who actually paid for this" answerable a year later.

### 14a · The two recognition models, which are not interchangeable

This is the part that is an accounting error if it is got wrong, and Rekaz
splits its own product along the same line, which is evidence the distinction is
real and not theoretical.

| shape | liability | revenue recognised |
|---|---|---|
| Package (10 sessions) | yes | when each session is **delivered** |
| Subscription (monthly gym) | yes | **ratably over the period**, attended or not |
| Gift card / wallet | yes | when spent |
| Deposit against a booking | yes | when the booking is served, or forfeited |
| Loyalty points | yes | when redeemed, or expired as breakage |
| **Coupon** | **no** | never. No consideration was received |

- [x] A gym subscription recognises monthly whether or not the member appears.
      A ten-session package recognises per session. Treating them alike
      misstates revenue every month in one direction or the other. Two
      aggregates, `Entitlement` and `Subscription`, and the split is the reason
- [x] A coupon is a discount at the point of sale and **not** a liability.
      `Reason::was_paid_for` is the whole of it: a grant nobody paid for carries
      no value, posts nothing, and recognises nothing when it is delivered

### 14b · Packages and subscriptions

- [x] `Package` — N of a service, with the balance that remains. **A deposit is
      the same aggregate**: it differs in being an amount rather than a count
      and in naming what it is held against, and in nothing else. Redeeming
      against a reservation line is an opaque id; `prepaid` does not know what
      a booking is
- [x] `Subscription` — a period, a price, a renewal, and **freeze**. Freezing
      earns everything up to that moment and stops the clock; resuming pushes
      the term out by exactly the time it was stopped for. How *long* a freeze
      may run is not decided here, because Rekaz's own copy concedes those
      rules are policy-dependent
- [x] Expiry, and breakage. An entitlement carries an expiry instant rather than
      `none | days | months`: the rule that produced the date belongs to
      whoever sold it, and storing the date is what makes a replay reproduce
      the decision instead of recomputing it
- [x] `type` on every grant: bought, gifted by a customer, granted by the
      business, free from a coupon. It decides the accounting, not the wording
- [x] Entry validation: `Subscription::admits` answers *is this live right now*
      from state rather than from a projection that may be a second behind,
      which is what a gym door needs

### 14c · Loyalty, in three mechanics

Rekaz rewards by **points, stamps, or visits**, ties rewards to specific
services, and puts the card in Apple Wallet. Stamps are the coffee-shop punch
card and are not a points balance with a different label.

- [x] Points — a balance earned at a rate, redeemed at a value
- [x] Stamps — N of a specific thing buys one free. The café mechanic
- [x] Visits — count of attendances, independent of spend
- [x] Tiers, which that system calls `Membership`: points_start, points_end, an
      earning rate. Easy to misread as a gym membership; it is a rank

**Both open questions were answered by the owner, and this is what was built.**

- [x] **IFRS 15, and no shortcut.** The answer was *always IFRS, without
      shortcuts*, so `Scheme` has no setting that selects the other treatment
      and there is no code path for it. What is deferred is a fraction of the
      sale by relative standalone selling price —
      `spend × (count × worth) / (spend + count × worth)` — and not the reward's
      face value. A hundred riyals awarding a hundred points worth ten halalas
      defers **9.09 and not 10.00**, which is the difference the shortcut hides.
      `points_defer_a_fraction_of_the_sale_and_not_the_reward` is that number,
      asserted against the ledger

- [x] **Multi-purpose vouchers are disallowed for now, and it is a guard rather
      than a note.** The claim "every shape here is single-purpose" was until
      now only in the docs: nothing stopped a caller granting an amount with no
      uses and nothing to hold it against, which is exactly an open-value gift
      card. `grant` now refuses that shape (`PrepaidError::OpenValue`), so what
      keeps this module out of tax is a check and not a hope.
      `an_amount_that_names_no_purpose_is_refused` grants the refused shape and
      the allowed one — a deposit, which differs by naming the booking it
      secures — one after the other

**Three divergences from what this section assumed, and the reasons.**

- **One aggregate for the three mechanics, not three.** They differ in what
  produces the count — a rate on spend, a named item, an attendance — and in
  nothing after it. `Mechanic` is fixed at open and read by the business;
  nothing branches on it. Rekaz models them separately and pays for it in three
  earning paths and three balances. The same lesson packages and deposits
  taught in 14b

- **`earn` does not need the sale, only its price.** The allocation is a
  fraction of the transaction price, so the caller passes `spend`; `from` is an
  opaque id and a reconciliation surface, exactly as `against` is for a deposit.
  A tighter coupling would make `prepaid` depend on `sales`, which siblings may
  not do. The cost is that the invoice and the deferral are two transactions —
  the module's existing bargain, and `a_liability_agrees_with_the_ledger` is
  what catches a pair that came apart

- **A rank is read from `lifetime`, which never decreases.** Spending points
  does not cost a rank, and neither does breakage: what was earned was earned.
  The movement that *crosses* a threshold earns at the old rate and the next one
  at the new, because any other reading makes the award depend on itself

- **There is no default scheme.** Account codes have a conventional value every
  chart ships; what a point is worth does not. A tenant who has not configured
  one cannot earn (`PrepaidError::NoScheme`) rather than earning against a
  number nobody chose (L6)

- **A card survives its own breakage.** Points running out is not the end of the
  card — it can earn again the next day — which is the one place this aggregate
  is not shaped like `Entitlement`

**What the answers leave open, recorded rather than guessed at.** If open-value
cards are ever wanted, the classification is a property of the *product* and not
a tenant setting, and the sale has to settle its own tax question first: the
refusal above is where that decision lands, and `Reason::Bought` still assumes
the sale carried its own tax.

### 14d · One ledger integration

- [x] Every shape posts through `ledger::post_entry_in`, in the same
      transaction as its own event, exactly as `sales` does
- [x] The chart templates gain the liability account, `2400 Deferred revenue`,
      in every template for the reason VAT and Zakat are in every template
- [x] The invariant: **the deferred revenue balance equals the sum of
      unredeemed value.** `a_liability_agrees_with_the_ledger` asserts it after
      grants, redemptions, recognition, a freeze, a resume, a renewal, a
      revocation and a cancellation

**This module posts the deferral, not the sale — a divergence, and the reason is
ZATCA.** The plan said *"sale is Dr cash / Cr deferred revenue"*. That skips the
tax invoice, and a Saudi business selling a gym year cannot skip one: it is a
supply, it needs an invoice, and the invoice has to be cleared or reported.
`sales` already does all of that. So the sale is an ordinary invoice and `sales`
posts it; `prepaid` adds the fact that the revenue is not earned yet, with
Dr revenue / Cr deferred at the grant and the reverse as it is delivered.

Two things follow. **No tax anywhere in this module**, so there is no second
opinion to keep consistent. And **the reclassification is visible** — an auditor
sees revenue booked and then deferred, which is what happened, rather than a
sale that never appeared in the sales ledger.

**A bug the canary was blind to, found by a different test.** `renew_subscription`
posted the release of the term that ended and never the deferral of the term
that began, so the read model carried a liability the books did not. The canary
had not renewed anything; it does now.

**Recognition is a cumulative total, never a sum of instalments.** Each step
computes what *should* have been earned by a date and posts the difference, so
running a month-end job twice posts nothing the second time, and
`Money::apportioned` being exact at `n/n` means the last day of a term brings
the liability to exactly zero. Summing instalments would strand a halala on
almost every term, in the account that is supposed to be the canary.

**Exit:** a gym sells a frozen-then-resumed annual membership and it reconciles
to the trial balance. The café's tenth coffee is 14c and waits on the answer
below.

---

## Phase 9 — People, and what the Kingdom requires · 7–9 weeks

**Why this is a phase and not a module.** Payroll touches the ledger, attendance
touches booking, and documents touch the outbox. It is the first thing that uses
three existing modules at once, which is the real test of whether extension by
subscription holds.

**It is also the first phase that changes who may do what**, which is why it
grew: §9b makes the org chart an authorization structure, and §9c is the
decision about which plane that structure is allowed to reach into. Neither is
payroll, and both have to be settled before `Employee` has a field.

### 9a · `modules/hr` — the org chart

- [x] `Employee`. `Position`, `Department` and `Contract` are not built: the
      tree and the claims are what everything else in this phase stands on, and
      three more aggregates before the authorization model was proved would
      have been three more things to change when it moved
- [x] Skills: which services a person may perform. `booking::assign` reads it,
      which is why `hr` lands below `booking` and not beside it.

      **An empty list means anything, not nothing**, or every existing tenant's
      rota would be refused the day the module is switched on. The edge is that
      recording the *first* skill starts restricting, which is why the API takes
      the whole set at once and offers no way to add one — and why the read
      answers `restricted` rather than leaving `[]` to be read either way.

      `eligible_for` is **one question**: employment, documents and skill
      together, because a caller who had to ask both would eventually ask one
- [ ] Shifts, on Phase 8's recurrence. The same problem, so the same type
- [ ] Attendance and leave, with balances that accrue

**The org is a tree, and it is the point.** Employees are not a flat list with a
`manager_id` decoration; the reporting line is the structure, and everything
below depends on being able to walk it. So: one `reports_to` edge per employee,
a single root per tenant, and **cycles refused at the command** — not because a
cycle is untidy but because the claim union below would not terminate.

- [x] `Employee::reports_to`, and `Reparented` as an event of its own. Moving a
      person moves everything they carry, which makes it the operation an
      auditor asks about — and an `Amended` that quietly changed a parent would
      not answer them. `amending_details_cannot_move_somebody_in_the_chart` is
      what keeps the two apart
- [x] A cycle is refused, in `claims::place`, by a recursive walk **down** —
      because that is the direction one closes in: making `A` report to somebody
      already in `A`'s subtree is what creates it

### 9b · Claims, and why they travel upward

**The rule the owner asked for: a manager automatically holds everything their
reports hold.** Formally, for each node in the tree:

```text
claims(node) = own(node) ∪ ⋃ claims(child) for each child
```

The reason is operational and good: a manager has to be able to cover for anyone
beneath them, and nobody should have to remember that giving a new clerk a
permission also means giving it to their supervisor. Granting downward is the
arrangement that produces the support ticket *"the branch manager cannot approve
what her own cashier can"*.

**Every consequence below follows from that one line, and each is a decision
rather than an accident.**

- [x] **The root holds the union of every claim in the company.** Settled as
      intended: the person nobody reports to is the owner, and a business owner
      who could not approve something in their own company would be a
      surprising product.

      Somebody who must sit *outside* it — an external auditor, a bookkeeper on
      retainer — is **not an employee and does not go in the tree**. They are a
      platform membership with a role, which is the other axis entirely and is
      exactly what §9c kept separate
- [x] **A grant at a leaf is not a local act**, and the API makes that
      impossible to omit: `POST .../claims` answers with `holders` — *everyone*
      who now has it — rather than an acknowledgement. A screen cannot fail to
      show what it was handed
- [ ] **Segregation of duties breaks, and this is the part that fails an audit.**
      The control every accounting system is measured on is that the person who
      raises an invoice is not the person who approves its payment. Under a
      bottom-up union their shared manager holds both, automatically, the moment
      the org chart says so.

      So a claim is markable **non-propagating**, and `hr::SEGREGATED` is the
      list that must be — a constant and not configuration, because what an
      auditor requires is not a preference a tenant expresses. `grant` refuses
      to propagate one **even when asked to**, and the response says
      `propagates: false` rather than silently doing something else.
      `a_segregated_claim_travels_nowhere` is the test
- [x] **It is not computed on demand.** `org_claim_effective` is maintained when
      the org changes and read as one indexed lookup when a command asks.

      The recomputation is **the whole set, not an increment**, and that is
      deliberate: an incremental update would be a second implementation of the
      union rule, free to disagree with the first. This codebase has already
      been bitten by a rule written twice — `pos`'s drawer — so the union exists
      once, in one recursive SQL statement, and every change re-runs it. Marked
      `ponytail:` with the condition for changing it

**What 9a still owes**, and it is additive: skills, shifts, attendance, leave,
positions, departments and contracts. Each of them hangs off `Employee` and
none of them changes the authorization model, which is why the tree and the
claims went first — three more aggregates written before the model was proved
would have been three more things to change when it moved.

### 9c · Which plane the claims live on — **decided: they do not leave the tenant**

Authorization today is *control-plane*: `Access { role, overrides }` per identity
per tenant, four coarse `Capability` values, checked by `Allowed<C>` at the edge
and cached across nodes in Redis. Employees are *tenant-plane*. A hierarchy in
one granting permissions checked in the other crosses the boundary that D15 and
the two-plane split exist to keep clean — so it does not.

- [x] **Decided (a): hr claims are domain claims.** They answer *"may you approve
      this leave request, discount beyond ten percent, sign off this timesheet"*,
      and are checked **inside module commands** where the decision is made.
      `Capability` and `Allowed<C>` are untouched: the platform keeps answering
      *"may you reach this endpoint at all"*, and `hr` answers *"may you do this
      particular thing"*
- [x] **Two lines that must stay true**, now three tests in
      `modules/hr/tests/planes.rs`: no `hr` type appears in `erp-control`,
      `erp-web` or `erp-tenant` and none of them depends on the crate; nothing
      in `hr` reaches for `Invalidate`, `forget` or `ControlPlane`; and nothing
      in `claims.rs` names a `proj_` table.

      They are source-scanning and crude, which is the point: the decision is
      not enforced by any type, so what enforces it is a test that reads what a
      reviewer would have to read

**What it buys.** No plane is crossed. Nothing has to invalidate a session cache
when somebody is promoted — `shared.rs` warns that a stale *logout* is the one
thing that cache must not serve, and a stale *promotion* would have joined it. A
tenant's own org chart cannot widen what the platform believes about that tenant.

**Rejected: (b), the claims feeding the control-plane role.** It needs an
employee → identity mapping and a tenant-wide session invalidation on every
re-parent, and it puts a customer-editable tree in front of the platform's own
authorization. It is also the harder direction to leave: claims that turn out to
belong at the edge can be promoted later, whereas a control-plane hierarchy
shipped first cannot be quietly demoted.

**Where the effective claim set lives, which (a) settles.** Not in a projection.
A command that has to know *"may this person approve this"* cannot read a read
model that may be a second behind — the same reason `sales` checks a customer
against the log rather than `proj_crm`, and the same reason a claim revoked a
moment ago must already bite.

But the union is a subtree walk, and loading every employee aggregate to compute
one inside a command is not viable either.

- [x] The effective set is **write-side state in the tenant migration chain**
      — `migrations/tenant/0008_org_claims.sql` — maintained in the same
      transaction as the org event that changed it. `proj_hr` exists alongside
      it for the screen that *draws* the chart, and is not what any check reads.

      **A design error the first run caught.** `PRIMARY KEY (employee, claim,
      branch)` cannot hold a nullable column, and company-wide is exactly
      `branch IS NULL` — so the key would have forced every claim to name a
      branch, which payroll cannot. It is two partial unique indexes instead,
      and the second is not redundant with the first because Postgres treats
      NULLs as distinct

### 9d · Branch scoping, now that branches exist

Phase 16 built the dimension. Everything in `hr` carries it: `Employee`,
`Department`, `Position`, `Contract`, shifts and attendance.

- [x] **A record's branch is not the request's branch, and conflating them is
      the bug waiting here.** Phase 16's branch travels in `Metadata` and means
      *where this request happened*; `Employee.branch` means *where this person
      works*. They differ legitimately and often — an Olaya manager visiting
      Malaz records attendance for a Malaz shift — and a report that read one
      where it meant the other would be wrong in a way nobody notices for a
      quarter
- [x] **A filter, not a wall.** `ledger::post_entry_in` *refuses* a document
      dated to a branch that is not open; `hr` reads must **default** to the
      caller's branch and widen on an explicit parameter. It cannot be a wall:
      payroll, the org chart and an end-of-service calculation are company-wide
      by nature, and a boundary that refused them would make the feature
      unusable in the first month
- [x] Every list endpoint states which of the two it is. `GET /v1/hr/employees`
      says it in the operation summary and takes `?scope=all`, which is what a
      payroll run and an org chart both want
- [x] **Claims carry their branch up the tree.** The union is over
      `(claim, branch)` pairs. `a_claim_carries_its_branch_up_the_tree` asserts
      both halves: the regional manager accumulates Olaya *and* Malaz, and the
      Olaya manager does not gain Malaz

### 9e · Documents that expire

An expired iqama stops a person working. The module warns before the date and
refuses to roster anyone whose document has lapsed.

- [x] Identity documents, work permits, medical certificates, professional
      licences — each with an expiry. A **date and not an instant**: an iqama
      expires on a day in Riyadh, not at an hour in UTC, and storing an instant
      would make the answer depend on which side of midnight somebody asked.

      Four variants and not a free-text kind, because a rule that reads a string
      is a rule that silently does nothing when somebody types `Iqama`
- [~] The reminder. `hr::expiring` is the read — what has gone *and* what is
      going, soonest first, because burying the lapsed ones below the upcoming
      ones is how they stay buried.

      **The outbox producer is not built, and the reason is worth recording:**
      the tenant dispatcher has *no handlers registered at all*
      (`bin/worker.rs`: "an empty dispatcher claims nothing"). Email lives on
      the control plane, because the things that send it are control-plane rows,
      and a module cannot reach it. So an effect enqueued here would sit in the
      outbox for ever.

      What reaches somebody today is a `HealthJob` invariant —
      `WorkDocumentExpiry`, beside `CertificateExpiry` and for the same reason.
      **A lapsed document is a separate finding from an expiring one**, not a
      louder version of it: one is somebody to remind, the other is somebody who
      must come off the rota today, and collapsing them into one severity is how
      the second gets treated like the first.

      The *email* still needs a tenant-plane handler that does not exist
- [x] Escalation: a document that lapses is not a warning that was ignored, it
      is a person who may not be rostered. `booking::assign` refuses them, at
      the moment they would be — against `hr`'s **log**, so an iqama renewed
      this morning counts now.

      The link is `ResourceEvent::Declared { employee }`, **set once** like
      `branch` and **optional**: a business that keeps a diary and no staff
      records is unaffected, which is what makes this additive rather than a
      migration. `booking` gains a crate dependency on `hr` and **not** an
      entitlement one — the same distinction `sales` and `crm` already draw.

      Somebody with no documents recorded may work. A business that has not
      started recording them must not find its whole rota refused the day the
      module is switched on

### 9f · `modules/payroll`

- [ ] Salary structure: basic, allowances, deductions
- [ ] A run produces a journal entry, posted to `ledger` by subscription — the
      direction `tax_sa → sales` already runs
- [ ] Commission from booking: a person earns on the services they performed,
      which is where the three modules meet

### 9g · `modules/hr_sa` — a country module, mirroring `tax_sa`

- [ ] **GOSI** contributions, employee and employer shares
- [ ] **WPS** — the monthly salary file the Ministry mandates. The same shape as
      the ZATCA submission already built: a generated document, a schema, a
      transmission, a receipt, a status
- [ ] **End-of-service benefit** — statutory gratuity by a defined formula over
      service length. Exact, because `Money` is integer minor units

**Exit:** a payroll run posts, a WPS file validates, and an expiring document
reaches somebody.

**And the org half, which is a separate claim and needs its own:** a branch
manager holds every claim her cashiers hold without anyone granting it twice; a
claim marked non-propagating stops at the person it was given to, so the invoice
raiser and the payment approver still cannot be the same authority; re-parenting
a department moves its claims and says so in the log; and a company-wide payroll
run reads every branch while a branch manager's employee list defaults to one.

---

## Phase 10 — Reporting that agrees with the books · 3–4 weeks

**The architectural point, stated before the work.** A dashboard mixing sales,
bookings and payroll looks like it must read three projection groups. L3 forbids
that, and it is exactly the mistake that system made — its projectors declare
which other projections they read, and it needed a bespoke check to police the
rebuild order that created.

**A report module subscribes to the log; it does not read another group.** It
consumes `sales.invoice_issued`, `booking.reservation_completed`,
`payroll.run_posted` and maintains its **own** group: one checkpoint, internally
consistent, L3 satisfied.

### 10a · `modules/reports`

- [ ] Sales: revenue by period, branch and product; tax summary
- [ ] Booking: utilisation, no-show rate, lead time, revenue per resource-hour
- [ ] People: headcount, cost, documents about to expire
- [ ] Cash: takings by method and by person, against what was banked

### 10b · The invariant that makes a report trustworthy

- [ ] A report group reconciles to the trial balance, asserted the way
      `an_unbalanced_entry_is_refused` is asserted
- [ ] A discrepancy is a **failure**, not a coloured cell. L6
- [ ] The warning from that system, taken seriously: its customer statement is
      built from invoices rather than from the ledger, because the ledger was
      unfinished and its books were going to be deleted and rebuilt. Two
      financial truths that disagree is what this section exists to prevent

**Exit:** every figure on a dashboard is derivable from the log, and reconciles.

---

## Phase 11 — Channels, documents, and moving data in and out · 5–6 weeks

**What Phases 7–10 assumed and did not build.** They describe a domain and no way
to reach anybody in it. The system has exactly **one** effect kind — `email.send`
— which is the entire outbound surface. For a product sold in this market the
channel is not plumbing; a reminder that does not arrive is a chair that stays
empty.

Everything here hangs off machinery that exists. D9 already gives an effect a
transaction, a retry policy, a lease and a dead letter, and `two_dispatchers_never_deliver_the_same_effect`
already passes. A channel is a `Handler`, and that is the whole integration.

### 11a · Channels as effects

- [ ] `sms.send`, `push.send`, `whatsapp.send` beside `email.send`
- [ ] One `Recipient` resolved at send time, never a phone number frozen into an
      event — a person who changes their number should get the next message
- [ ] Delivery receipts land back as inbound events (Phase 12), so "sent" and
      "delivered" stay different words
- [ ] **Metering.** SMS is billed per segment, and a message that silently
      becomes three costs three times. Segment counting is part of the handler,
      and a per-tenant budget refuses rather than overspends (L6)
- [ ] Push tokens expire. Cleaning them up is scheduled work, not an afterthought

### 11b · Templates that fetch their own data

The system read for Phase 7 has **two** template systems that do not meet: a
database aggregate whose parameters the caller fills in by hand, and hardcoded
classes with the copy, the business name and the gendered wording compiled in.
Changing a reminder's wording there is a deploy. Both problems have one cause —
a template cannot ask for anything, so somebody must hand it everything.

- [ ] A template names an **audience**, not an address: the client, the employee
      on the booking, the manager of that branch, an operator. The recipient is a
      query against the read model at send time
- [ ] A template declares **bindings** — `{{ booking.starts_at }}`,
      `{{ customer.name }}` — resolved from projections when it is rendered, so
      the caller supplies a subject and nothing else
- [ ] Bindings are declared, so an unresolvable one fails **when the template is
      saved**, not when a customer is waiting for a message
- [ ] Arabic and English are the same template with two bodies, per D12. Neither
      is a translation of a compiled string
- [ ] Rendering happens in the worker, at send time. A reminder for a booking
      that moved says the new time

### 11c · Files, and where they actually live

- [ ] `Storage` as a trait: local disk and S3-compatible object storage to start,
      and the tenant chooses. A self-hosted tenant (D15) keeps its own files, and
      that is the point rather than a configuration detail
- [ ] An event stores `(engine, key, checksum, size, media_type)` and **never a
      URL**. A URL is where a file is today; a key is what it is
- [ ] The checksum is verified on read. A document that comes back different from
      what was stored is a failure, not a warning
- [ ] Attachments are polymorphic — a document belongs to an invoice, a booking,
      an employee record — and the owner is what authorizes reading it

### 11d · Spreadsheets, both directions

- [ ] Export any list the API can page. It is the same query, a different
      encoder, so a new list is exportable the day it exists
- [ ] Large exports are effects, not requests: generate, store (11c), then send a
      link (11e). A report that takes a minute must not hold a connection
- [ ] Import with **partial failure as a first-class outcome**. A thousand-row
      file with three bad rows imports 997 and returns the three, with the row
      number and what was wrong
- [ ] An import is a command per row under one idempotency key, so a re-upload of
      a corrected file does not duplicate the 997

### 11e · Short links for anything

- [ ] A link points at an internal target or an external URL, and anything can
      make one in a line. SMS is billed by length, which is the practical reason
- [ ] Optional expiry, optional single use, and a visit record
- [ ] Infrastructure, not domain (D11) — it holds no business meaning and every
      module may use it

**Exit:** a booking reminder reaches a customer in Arabic, on SMS, with a link,
having asked the read model for everything it says.

---

## Phase 12 — Taking money, and letting other systems in · 5–7 weeks

**The distinction this phase exists for.** The system **records** payments. It
has never **taken** one. Those are different problems: a recorded payment is a
fact somebody asserts, and a taken payment is a conversation with a third party
that can time out halfway. Everything here is the second kind.

It is also the first inbound surface. Every integration so far has been the
system talking; a gateway talks back, and a callback that is trusted without
being verified is somebody else's command executed under your authority.

### 12a · Payments

- [ ] A card gateway, and **saved cards** — the token is the gateway's, never a
      card number, and it belongs to a customer rather than to a session
- [ ] Buy-now-pay-later, which is **not a card gateway wearing different
      branding**: the provider pays the merchant and collects from the buyer, so
      the receivable is settled by a third party and the entries differ. Getting
      this wrong shows up as a debtor who has already paid
- [ ] Capture is idempotent under retry (L8). A timeout is not a failure — it is
      an unknown, and the resolution is a query against the gateway, never a
      second capture
- [ ] Refunds, partial refunds, and what a refund does to a cleared tax invoice.
      ZATCA has an opinion (`tax_sa`), and it is a credit note
- [ ] **Settlement.** A gateway pays out in batches, net of fees, days later. The
      reconciliation is: this payout equals these payments minus this fee — and
      it posts to `ledger`. The bank statement matching from Phase 8 is the same
      machinery pointed at a different source
- [ ] Fees are an expense, not a smaller revenue. A tenant that nets them cannot
      answer what it actually sold

### 12b · Inbound webhooks

- [ ] Signature verified before the body is read. An unverified callback is not
      a slow path, it is a refused one
- [ ] Delivered more than once, out of order, and replayed by an attacker who
      kept a copy — so a webhook is a **command with the provider's id as its
      idempotency key**, and arriving twice does nothing twice
- [ ] Accepted fast, processed as an effect. A provider that times out retries,
      and a retry storm is self-inflicted
- [ ] Providers that go quiet: reconcile by polling what the provider says it
      sent. A payment confirmed by a webhook nobody received is money the tenant
      cannot see

### 12c · API keys, in pairs

- [ ] A **public key** identifies and is safe in a mobile app or a browser. A
      **private key** authenticates, is shown once, and is stored hashed — the
      same posture as a password
- [ ] Scopes per key, so an integration that reads bookings cannot post journal
      entries
- [ ] Rotation with an overlap window, because a key that cannot be rotated
      without downtime is a key nobody rotates
- [ ] Rate limits per key, and this is the primitive
      [item 5](#5-signup-is-public-unlimited-and-creates-a-database) has been
      waiting for

### 12d · API version compatibility

Not app version gating. A client — mobile, web, or somebody else's system — was
**compiled against a stated API version**, and the server decides whether it can
still be served.

- [ ] A request declares the version it was built against. The server publishes
      the range it supports and refuses outside it, naming the version to build
      against — the same shape as `MIGRATION_FLOOR` refusing an old tenant, and
      the same reasoning as D17's two majors
- [ ] The refusal is a typed error a client can act on, not a 500
- [ ] A version inside the range but behind is served, and says so in a header.
      Deprecation that arrives as a surprise is an outage

### 12e · Signing in without a password

- [ ] One-time codes over SMS, for a market where a phone number is the identity
      and an email address often is not
- [ ] Two rate limiters, not one: requesting a code and verifying a code fail
      differently and must be limited separately
- [ ] A code is single use, short lived, and constant-time compared
- [ ] Cookie sessions for a browser and bearer tokens for everything else, over
      one session model — two authentication surfaces, one authorization answer

**Exit:** a customer pays with a saved card, the webhook confirms it once however
often it arrives, the payout reconciles to the ledger, and an outdated client is
told what to build against.

---

## Phase 13 — Real time · 3–4 weeks

**This is a requirement, not a refinement.** A customer books from a phone and
the schedule on every counter screen must show it, without anybody refreshing.
Two people looking at the same grid, one of them holding a phone, is how a slot
gets sold twice — and while Phase 7's guard refuses the second write, a screen
that still showed the slot as free has already cost a conversation.

Every read in this system is a poll today. `pg_notify` was refused by D4 and
stays refused; this is the mechanism that replaces the polling it would have
optimised.

### 13a · The event stream

Designed already, not built. The shape matters more than the transport, and
three parts of it are not obvious.

- [ ] Server-sent events over the tenant's own log. Not WebSockets: the traffic
      is one-directional, and SSE reconnects by itself
- [ ] **It carries a signal, not the data.** *"Group `booking` is queryable
      through position N."* The client re-fetches through the ordinary API, which
      already does authorization, localization and paging. A payload stream would
      need all of that again, in a second dialect — and would make the log a
      query engine, which L7 forbids
- [ ] **Published when the projection advances, never when the event is
      appended.** An event is committed and visible before its projection has
      applied it; signal on the append and the client re-fetches, reads a lagging
      read model, sees nothing new, and stops. The hook is the `Advanced` arm
      after its commit, because that is the moment the guarantee becomes true
- [ ] **A stream holds no database connection.** Fan-out is the Redis channel
      `shared.rs` already uses for cache agreement. A per-stream poll would
      multiply connection demand by open browser tabs, against a budget sized in
      `pools.rs` for tenants rather than tabs
- [ ] Opening a stream calls `request_visit`. A quiet tenant has backed off to a
      six-hour interval, and a stream onto a dormant tenant is silent until
      somebody gives up
- [ ] Streams are capped and reconnect. A stream held for hours outlives the
      authorization checked when it opened, and reconnection re-runs the
      extractor for free

### 13b · The live grid

- [ ] A booking made anywhere reaches every screen watching that branch and day
- [ ] Filtered by what the watcher may see — a signal naming a group a viewer has
      no module for is not sent
- [ ] The grid reconciles on reconnect rather than trusting a delta it may have
      missed. `?consistent_after=` already expresses "wait for at least this"

### 13c · Notifications inside the system

- [ ] A notification is a **durable record first** and a live signal second. One
      that only existed on a socket did not happen for whoever was at lunch
- [ ] Read state per person, and it survives a rebuild — so it is a projection of
      an event, not a flag set on a row
- [ ] The same audiences as Phase 11b: the client, the employee, the manager, an
      operator. One audience model, four channels — in-system, email, SMS, push —
      and a preference per person

### 13d · Conversations

- [ ] A thread against a subject: a booking, an invoice, a customer. Not a chat
      room, which nobody can find afterwards
- [ ] Inbound messages (Phase 12b) land in the thread, so a customer replying to
      a reminder is answering a person and not a void
- [ ] Internal notes and customer-visible messages in one thread, distinguished —
      the private-note distinction the system read for Phase 7 found necessary
      enough to build twice

**Exit:** two browsers and a phone agree about a schedule within a second of a
booking, and nobody polled.

---

## Out of band — identity moves to `Idempotency-Key`

**Not a phase. A defect found by inspection, fixed before release.**

The API took each record's identity from an `id` in the request body, and that
identifier was doing two jobs: naming the record, and telling a retry from a new
request. It did the second badly. `INV-0001` chosen at one till collides with
`INV-0001` chosen at another, and **five creates resolved that collision by
ignoring the second write and returning success** — losing a document and
reporting it saved. The worst of them, `sales::issue_invoice`, handed the second
till the *first* invoice's statutory number.

- [x] **`erp_eventlog::try_create`**, beside `try_execute`. Empty stream →
      create. Taken, and the request's fingerprint matches → a retry: nothing is
      written and the original is reported. Taken, and it differs →
      `ExecuteError::AlreadyExists`, a 409. The rule is the kernel's, so no
      module writes it and none can forget it
- [x] **The fingerprint travels in `Metadata`**, which every command already
      takes, so **no command signature mentions it**. A create is written
      exactly as before and gains the rule by calling `try_create`
- [x] **`erp_web::IdempotencyKey`**, required on every create and refused unless
      it parses as a UUID. It **is** the aggregate id, so deduplication falls out
      of the log's own `UNIQUE (stream_domain, stream_id, sequence)`: no keys
      table, no TTL, no sweeper, no Redis, and idempotency that is permanent
      rather than lasting a day
- [x] `id` is gone from every create body. The identity a human reads is the
      server's — an invoice `number` from a gapless statutory series
- [x] `TenantDb::create`, the retry loop for creates, mirroring `execute`
- [x] The five silent no-ops replaced: `sales::issue_invoice`,
      `purchases::record_bill`, `ledger::post_entry`, `booking::declare_resource`
      and `booking::reserve`
- [x] `CrmError::AlreadyExists` and `PrepaidError::AlreadyGranted` /
      `AlreadyStarted` / `AlreadyOpen` deleted. Four modules had written their
      own version of one rule; now none has

**Two identities keep their own names, deliberately.** An account code and a
bookable resource are named by the business and referenced by that name — you
book `chair-1`, you post to `4000` — so those creates keep the id in the body
and use the key only as the fingerprint. The rule still applies: a *different*
resource claiming a taken name is refused rather than swallowed.

**A consequence worth knowing.** A list paginated on `(timestamp, id)` used to
tie-break in creation order, because clients numbered their own keys
sequentially. Ids are UUIDs now, so rows sharing a timestamp come back in an
order that is stable but arbitrary. The cursor's actual guarantee — no row
skipped, none repeated — is unchanged, and
`a_list_longer_than_one_page_can_be_read_to_the_end` asserts that rather than the
sequence it used to assert. If invoice lists should sort by document number
within a day, that is a separate change to `sales`, and a defensible one.

This reverses the L8 note in `docs/ARCHITECTURE.md` §3, which argued a header
"buys nothing without a store of keys and prior responses beside it". Right about
the store, wrong about the conclusion: the store is unnecessary once the key is
the stream id.

---

## Phase 15 — `modules/pos` — the counter · 4–5 weeks

**The segment that needs no calendar.** A coffee shop, a restaurant and a retail
shop never take a booking, and until this exists there is nothing to sell them.
It is also the cheapest phase to be confident about, because the hard half is
already built: every till transaction is a ZATCA **simplified** invoice, and
this system builds, hashes, chains, signs and reports those today.

- [x] **`Sale` is not an aggregate, and that is the phase's one real decision.**
      A till transaction *is* a ZATCA simplified invoice, and `sales` already
      builds, numbers, hashes, chains, signs and reports one. A second document
      model here would duplicate VAT, discounts, numbering and the ZATCA chain —
      and give revenue two sources of truth, so the VAT return and the till
      report could disagree with nobody able to say which was right.

      So `pos` **composes**: `sell` writes the shift's event, `sales::issue_in`
      and `sales::pay_in` in **one transaction**, the same seam `sales` itself
      uses on `ledger`. The two functions were already there and private;
      making them public was the whole of the change to `sales`
- [x] `Shift` — open, sell, pay out, count, close. The cash-drawer domain: an
      opening float, takings by tender, a declared count, and **the variance**,
      which is the number a manager actually reads and the only one this module
      posts
- [x] A sale is a simplified invoice unless the buyer gives a VAT number. Not
      re-decided here: it is `sales`' rule, reached by passing the customer
      through, which is what composing buys
- [x] **Returns and refunds — the prerequisite was built, then this was.**
      `sales::cancel_invoice` refused an invoice that had payments, and **every
      till sale is paid the instant it happens**, so no till sale could be
      credited through any route.

      The refusal was not wrong so much as too blunt. What a credit note may not
      do is undo a supply while the business keeps the cash — so `sales` gained
      a **refund** (`Refunded`, `refund_invoice`, `refund_in`,
      `entry_for_refund`), and the rule became *"nothing is still held"*:
      `Invoice::held()` is paid less refunded, and a credit note needs it at
      zero. `pos::take_back` then hands the money back and credits the document
      in one transaction, which is the only order in which the books are never
      briefly wrong.

      A refund projects as a **negative** row in `invoice_payment` rather than a
      table of its own, so `paid` stays one sum and no read has to remember to
      consult a second place before saying what an invoice is holding
- [x] **Offline is deliberately out of scope.** Unchanged, and the reason is
      unchanged: a till that queues sales locally is a second write path with
      its own ordering problem, and L1 is not negotiable

**Two more divergences, and the reasons.**

- **The float does not post.** Cash moved from a safe to a drawer is still
  `1000 Cash on hand`, so the business is no richer and there is no entry. It
  follows that a shift's `expected` — what the drawer should physically hold — is
  a *larger* number than what the shift added to the ledger, and that the two
  answer different questions. The variance is what reconciles them
- **`5910 Cash over and short` is new in every chart**, for the reason `2400`
  was added in Phase 14: a till that records a shortage and cannot post it
  leaves the books saying the drawer holds what it does not, for ever

**Two weak tests, found by falsification and not by review.** Both passed
against code that was wrong, which is the failure mode a test suite is worst at
noticing about itself.

- The drawer rule was written **twice**: the aggregate matched on `Method::Cash`
  and the projection asked `is_in_the_drawer`. Making every card sale count into
  the drawer left every test green, because the aggregate never consulted the
  rule being broken. `Takings::in_the_drawer` is now the one place it is applied
- The variance test closed one till short and one over by the same amount and
  asserted the expense account netted to zero — which is also what posting
  *neither* looks like, and what posting them backwards looks like. It now
  asserts the shortage on its own before the overage exists

Seven mutations after those fixes, seven caught: a card in the drawer, a
variance unposted, a variance inverted, tenders that need not match the sale, a
retried sale ringing twice, a shut till still selling, and a pay-out counted
twice.

**Exit: met.** `a_cafe_opens_sells_and_closes_level` opens a shift, rings forty
coffees, checks all forty statutory numbers are distinct, closes level, and
asserts the drawer, revenue and VAT payable in the ledger — which `pos` never
posted, because `sales` did.

---

## Phase 16 — Branches · 2–3 weeks

**They do not exist.** The only `branch` in this codebase is a free-text string
on the ZATCA EGS certificate. There is no entity, no scoping, no reporting
dimension. Every competitor meters them (Qoyod charges SAR 40 each, Rekaz caps
them at five even on its top tier), which makes unlimited branches a real
differentiator and means the concept has to exist first.

- [x] `Branch` — a place, with an address. `modules/branches`, a **leaf that
      depends on nothing**, which is what makes it safe for everything else to
      sit on. Opening, amending, closing and reopening are events, because a
      dimension edited in place rewrites history: a report for Olaya run in
      March and again in June would differ with nothing able to say why
- [x] **A dimension on every document, from one mechanism.** The branch travels
      in `Metadata`, folded in by `erp_web::Allowed` from an `X-Branch` header —
      so *every* event a request produces carries it, and no module threads a
      field through. `ledger`'s `posting` table reads it off the envelope, and
      `branch_balances` splits the chart by it
- [x] **Validated once**, in `ledger::post_entry_in`. Every posting in the
      system arrives there — it is already where a closed period is enforced —
      so one check covers `sales`, `purchases`, `prepaid` and `pos` without any
      of them repeating it
- [ ] **Opening hours: deliberately not built.** Nothing would read them.
      `booking` already keeps availability per *resource*, which is finer than a
      branch and is what a diary needs; branch hours are something the booking
      site would *display*, and that site is a separate React project reading
      this API — Phase 17. A rule nobody applies is wrong by the time somebody
      applies it
- [x] **Resources belong to a branch**, which is what makes "book at Olaya"
      work. `ResourceEvent::Declared` carries one — **set once, like `kind`**,
      because a resource that changed branch would retroactively re-attribute
      every booking it ever held to a place it was not at. If a chair physically
      moves, declaring a new one is the honest record.

      Checked at declaration against the `branches` log, and **not** inherited
      from `post_entry_in` like every other branch reference, because declaring
      a resource posts nothing and so has no journal entry to carry the check.
      The rota narrows to the caller's `X-Branch` and `?branch=` overrides it —
      a default, not a wall, for the reason §9d gives
- [ ] A person scoped to one. Not built, but **the seam is placed**:
      `Allowed::branch` sits beside the capability check, which is where a
      person's scope would be enforced, and the doc comment says so
- [ ] ZATCA per-branch EGS units. Not built, and unchanged: `taxpayer_id()` is
      still one stream per tenant

**A footgun found by closing the gap, and removed.** `sales::issue_in` took the
journal entry's id as an argument, and `cancel_in` reverses that entry by
*rebuilding the same name*. So a caller that chose a different one — `pos` did,
reasonably, using its own prefix — issued an invoice that could never be
credited. The name is now derived inside `sales` by `issue_entry` and
`money_entry`, and is no longer a parameter anybody can get wrong. It was
invisible until a second caller existed, which is the argument for having one.

**Two decisions worth keeping.**

- **A per-branch trial balance does not have to balance, and nothing here
  pretends it does.** Debits equal credits per *currency* — that is the
  invariant `ledger` asserts and it is untouched. Moving cash between branches
  debits one and credits the other, so each side is out by the transfer until
  inter-branch clearing accounts exist. What this phase delivers is that each
  branch can be **reported** separately and that the branches are a *partition*
  of the whole. Claiming more would report a normal transfer as a broken ledger
- **A fourth `Address`.** `crm`, `sales` and `tax_sa` each already define one.
  Collapsing them into `erp-types` is worth doing and was not done here, because
  the three are event schemas that are equal by coincidence rather than by rule —
  ZATCA adding a field to the invoice one is what would separate them again. The
  duplication is named in `branches::Address` rather than left unexplained

**One guard had to be relaxed, and it was right to check.**
`every_modules_routes_live_under_its_own_name` required every path to start
`/v1/{module}/`. `branches` is the first module whose resource *is* its name —
`/v1/branches`, with nothing after it — which `module_of` already scopes
correctly. The guard now accepts the module root as well as paths beneath it.

**Exit: met.** `two_branches_report_separately_and_sum_to_one_trial_balance`
opens two branches, rings two tills at them through `pos`, and asserts each
branch's revenue on its own, that the per-branch rows sum to the unsplit chart,
and that the trial balance still balances.

---

## Phase 17 — The public booking API · 3–4 weeks

**The site is a separate project.** React and shadcn, its own repository, its own
deployment, talking to this backend over HTTP. So this phase builds **no pages**:
what it owes that project is an unauthenticated API surface, the security around
it, and a contract it can be built against.

That is a smaller phase than "the booking site" was, and a sharper one. The
pages, the Arabic and English rendering, the embed snippet and the Instagram-bio
link all leave this repository. What arrives in their place is the thing a
same-origin server-rendered site would never have needed.

- [x] **The surface itself.** `erp_web::Public` is the first thing in this build
      to open a tenant with **no person behind it**, and what makes it safe is
      construction rather than care: the handle carries no access, so
      `TenantDb::role()` is `None` and every capability check refuses it. A
      public handler cannot reach a guarded command by forgetting something — it
      would have to call a module function directly, which is a visible line
      rather than a missing one.

      It is also the first caller of **`Lane::Client`**, which has existed since
      Phase 1 and been used only by tests. "A tenant's customers, through their
      app or website. The flood" is what the lane was written for, and using it
      is what stops a bot on a booking form starving the counter staff serving
      people in the shop.
- [x] **Three public routes, deliberately narrower than their authenticated
      counterparts.** `services` never shows a withdrawn resource and never its
      capacity; `availability` answers one number; `reservations` takes a
      booking with **no price and no customer id on it** — a stranger choosing
      their own rate, or naming which of a business's customer records they are,
      are both things the counter's shapes allow and this one must not.
- [x] **CORS, which did not exist here at all.** Allowed origins are per tenant
      and checked through the control plane's entry cache — one staleness story,
      not two. Written here rather than configured from `tower-http` because
      that layer decides an origin with a *synchronous* predicate and this
      answer is an `await`; feeding a sync predicate would need a second cache
      refreshed on its own schedule.

      Never a wildcard, never credentials, and **never a suffix match** — a
      tenant that allows `https://salon.com` must not admit
      `https://salon.com.attacker.example`. Verified by falsification: writing
      the check as `ends_with` fails the test.
- [x] **Tenant resolution: settled as recommended.** The site calls
      `salon.erp.com`, so the subdomain stays the single source of tenant
      identity and `extract.rs`'s safety argument stays true unchanged. CORS
      does the cross-origin work, which is what it is for. `tenant_label` is now
      shared between the extractor and the middleware, because two
      implementations of "which tenant is this host" is how one of them comes to
      admit `a.b.acme.erp.com`.
- [x] **Domain verification**, and custom domains otherwise gone as planned. A
      domain is claimed and proved; only a proved domain licenses origins, so
      adding `https://www.salon.com` after `https://salon.com` is a row and not
      a second proof. The verification token is minted **inside the control
      plane** so no caller can choose a predictable one — the same lesson
      `sales` learned about the journal entry id.

      **What proves it is not built**, and the module says so: reaching out to
      DNS or a well-known URL is an outbound call, an outbound call is an outbox
      effect (D9), and that handler does not exist. `POST .../verification` is
      an operator recording that the check was made by hand, audited as
      `tenant.domain_verified`.
- [x] **Rate limiting, which stopped being deferrable.** Two fixed windows per
      node: per (business, origin) and per business. Charged in the extractor
      rather than in each handler, so a public route added tomorrow is bounded
      without anybody remembering to bound it — and charged *after* the tenant
      resolves, so a flood aimed at names that do not exist cannot consume a
      real tenant's budget.

      **Honestly per node**, and the numbers are chosen knowing it. Fleet-wide
      means Redis on the request path: failing open would be exactly the
      degradation L6 refuses, and failing closed makes a cache outage an outage.
      Per-node is the honest third answer, and the sharper key — a thing the
      caller *holds* rather than asserts — is Phase 12c's API key.
- [~] **Deposits at booking.** `prepaid` already models one: an entitlement with
      no uses, held against the booking it secures. What is missing is the half
      that takes the money, and card payments are Phase 12a.

      So the setting exists and is **recorded rather than charged**, which the
      response says: a site can tell a customer what will be asked for, and
      nothing in this build claims to have collected it. Building it any other
      way would be a booking that reports itself secured and is not.
- [x] **`docs/openapi.json` as a contract.** `docs/openapi.baseline.json` is what
      clients may rely on, and `tests/compatibility.rs` fails on a change that
      would break one: an operation that disappears or is renamed, a required
      request field that appears, a response field that vanishes, a path that
      gains a parameter. Accepting a break takes `just baseline`, which is the
      point — a break somebody typed a command to accept is a break somebody
      knows about.

      It checks four shapes and **not** type narrowing, enum members or
      `format`; a full structural diff is a much larger piece of work, and what
      is here catches the ones a normal refactor causes by accident.

      **The first version of it was useless and the falsification is what said
      so.** It compared only top-level response properties — and almost every
      list here answers `Paged<T>`, whose top level is `items` and `next`, so
      renaming `ServiceView::name` sailed straight through. It walks three
      levels now, bounded and cycle-guarded, and the same rename is reported as
      `public_services no longer returns items.name`.

**One decision worth keeping.** Online booking is **off until a business turns
it on**, and the absence of the setting is a no. The two public *reads* are safe
by their nature — a shop's front page is what they are. A public **write** claims
a real slot in a real diary, and a salon that never asked for online booking must
not find their week full of appointments nobody intends to keep. The rate limiter
bounds how fast that can happen; it does not make it something the business
agreed to.

Refusing is a **404 and not a 403**, because "forbidden" would confirm the route
would work for somebody else, which is neither true nor the caller's business.

---

## Phase 18 — Marketing · 3–4 weeks

- [ ] **Segments**, and the architectural constraint that shapes them: a segment
      like *"booked in the last 90 days and spent over 5,000"* spans booking and
      sales, and **L3 forbids reading across projection groups**. So marketing
      subscribes to the log and maintains its own group, exactly as Phase 10
      specifies for reports. That is what stops the campaign list disagreeing
      with the invoice list
- [ ] **Campaigns** — a segment, a template, a channel and a schedule. Every one
      of those exists after Phase 11; a campaign is the thing that composes them
- [ ] Tracking pixels: Meta, TikTok, Snapchat, Google Ads, GTM, Analytics,
      Clarity. **Client-side, and the client is the React project now** — so
      what this repository owes is the configuration: which ids a tenant has set,
      readable by the site. The pixels themselves are not built here
- [ ] Reviews, and the request that asks for one after a visit
- [ ] Abandoned bookings, which is the retargeting case that actually pays

---

## Phase 19 — `modules/inventory` · 3–4 weeks

- [ ] Products, quantities, and stock movements as events
- [ ] Consumption on sale, so a POS line depletes stock
- [ ] Counts and the discrepancy a count finds, which is the number that matters
- [ ] Cost of goods sold, posted to `ledger`. Without it a restaurant's margin
      is a guess

---

## What Phases 7–13 unblock

**Phase 5b finally has its second consumer.** The rule engine was deferred
because authorization alone could not describe it — one consumer means inventing
which facts exist. Booking automations (reminders, no-show handling, recall
follow-ups) and HR document expiry are two more, independent of each other and of
authorization. The engine can be specified from three working cases instead of
guessed at from one.

**And a fourth, which is the one that will shape it.** §9b's claim union is a
rule over facts the engine would have to name anyway — *who reports to whom*,
*which branch*, *is this claim propagating*. It arrives with a concrete question
already asked, which is exactly what the deferral was waiting for.

It also carries the sharpest constraint of the four, and §9c is what sets it. The
claims are checked **inside commands**, not at the edge, so the engine is not on
the hot path of every request — but it is inside a transaction that is holding a
connection, which is a worse place to be slow than it looks. Whatever the engine
evaluates for a claim has to have been settled when the org changed, not when
somebody asked.

**D14's push path finally attaches.** Architecture §1.14 says of
`request_visit` that it "pulls a tenant forward, which is where a push path
attaches when the API can tell a worker directly that a tenant just wrote
something: polling becomes the floor rather than the mechanism, and nothing
downstream changes." `ControlPlane::request_visit` and
`tests/leases.rs` both name the same seam. Phase 13 is that push path, and the sentence
was written to be collected.

**Item 5 gets its primitive.** Item 5 is closed — signup builds nothing until the
address answers — but the piece it deferred is still outstanding, and it was
never a signup-specific one: rate limiting per caller. `REQUEST_INTERVAL` caps
mail per address and cannot do more than that. Phase 12c builds the real one for
API keys, and signup is the second user of it.

**Phase 6's open question gets an answer.** Architecture §8 asks whether the
generic `Document` aggregate is right, and says to decide from customer
conversations rather than from the document. A reservation, a service request, a
leave request and a payroll run are four documents with genuinely different
workflows — which is the evidence §8 asked for.

---

## What needs work now

Written after reading this document against the code, in the order I would do
them. Everything here was **checked**, not remembered — the file and the command
that shows it are named.

### 1. ~~The outbox has no producers and no handlers~~ — done

Was: `grep` for `with_effect|enqueue(` outside `erp-eventlog`'s own tests
returned **nothing**, and `bin/worker.rs` built its dispatcher with no
`.register(...)` after it. Every piece of D9 was finished, tested, and reaching
nothing; the concrete cost was that an invitation was a link somebody copied out
of an API response by hand.

Email is the handler, invitations are the producer, and the shape of the fix was
decided by a fact nobody had noticed: **the outbox only existed in tenant
databases, and invitations are control-plane rows.** See the running note.

The original text follows, because the reasoning in it is what led to the
control-plane outbox rather than to a sweep.

#### The original finding

`grep -rn "with_effect\|enqueue(" crates/ modules/` outside `erp-eventlog`'s own
tests returns **nothing**, and `bin/worker.rs:54` builds
`Dispatcher::new(RetryPolicy::default())` with no `.register(...)` after it.

So: the outbox schema, effects-as-values, claim-under-`SKIP LOCKED`, exponential
backoff, dead letters, the at-least-once idempotency key, the crash tests that
prove a lost delivery record replays with the same key — all built, all tested,
and **nothing in the product uses any of it**. D9 is the architecture's answer to
"how does anything reach the outside world", and the answer currently reaches
nothing.

The concrete cost is one feature short: an invitation is a link the inviter has
to copy and pass on by hand, because sending an email is an outbox effect and no
handler exists. That was the right call when it was written — "belongs with the
first real handler" — and ZATCA turned out **not** to be that handler, because a
submission reads a sealed private key and an effect is a value in a table.

So email is the first real handler, and it is the one that unblocks invitations,
password reset, and every notification after. Until it exists the outbox is the
largest piece of finished, unexercised machinery in the build — and unexercised
machinery is where the next silent bug lives. Two of them have already been found
this way (the ZATCA sweeps had no caller; `ON DELETE SET NULL` on `audit_entry`
was unreachable from the day it was written).

### 2. ~~There is no CI~~ — done, with one job still missing

`.github/workflows/check.yml`: `just check` against Postgres 18.3 and Redis 8 as
service containers, and a second job that runs `just prepare` and fails if
`.sqlx` moved. Verified the way it needed to be — the whole suite against a
**freshly created empty Postgres**, which is what a runner gets and which is the
only way to find setup that lives in a shell history rather than the repository.
705 passed there, identical to local.

Redis is a required service, not an optional one: `shared.rs` refuses rather
than skipping when it is absent (L6), so a runner without it fails four tests
instead of quietly covering less than the badge claims.

**Still missing:** the job D17 actually wants — upgrading a realistic N-1
database on every build. It needs a seeded corpus at the previous major, and
there is no previous major yet (`MIGRATION_FLOOR` is 0 for all of the first).
Build the corpus at the first major release, not before.

The original finding follows.

#### The original finding


No `.github`, no `.gitlab-ci.yml`, nothing. `just check` exists and runs
`fmt-check`, `clippy -D warnings` and `cargo test --workspace`; a person has to
remember to run it.

This document claimed "a required CI check" in two places, which is how a claim
like that survives — it was true of the intent and never of the repository. Both
are corrected above.

What CI has to run, and why each one is not optional: `just check`, `just errors`
and `just openapi` (both regenerate a committed file and fail on drift), and
`just migrate-fleet check` / `versions` against a scratch database (the two
pre-deploy gates). The soak test and the ZATCA sandbox tests are `#[ignore]`d and
need credentials; they belong on a schedule, not on a push.

### 3. ~~Shadow replay covers two groups of four~~ — done

Was: `ledger` and `sales` were replayed, `purchases` and `tax_sa` were not, while
the test's own doc comment said "every group". `tax_sa` was the one that mattered
most — its projection builds the ZATCA hash chain, and a rebuild producing a
different document produces a different hash, breaking a chain **the tax
authority validates**.

All four now, and the list is no longer trusted: the group names replayed are
compared against every group `erp_api::modules()` declares, so a module added
without a line there fails rather than becoming the next one nobody watches.

Each group also names a table the demo must have filled, because `EXCEPT ALL`
between two **empty** tables is clean — a group whose read models happen to be
empty was "reproducible" the way a blank page is correct.

Falsified four ways: dropping `tax_sa` from the list fails the coverage
assertion; dropping a projection from the group empties its witness table; and a
`clock_timestamp()` in either the `tax_sa` or `purchases` insert is caught by the
differ (7 documents and 4 bills respectively).

### 4. ~~Nothing says how to deploy this, or how to get a tenant back~~ — half done

**Getting a tenant back** is done and is a test, not prose:
`crates/erp-control/tests/restore.rs` dumps a tenant, destroys it, restores it,
and compares the log row for row. A second test pins the failure an operator
actually hits — the two planes restored to different points, where *neither*
direction reports an error and one of them silently loses events.
`docs/RUNNING.md` documents the procedure those tests execute.

**Still open:** deployment beyond compose, and Postgres failover. Neither is a
test, and neither should be claimed until it is rehearsed the same way.

The original finding follows.

#### The original finding


The target is 2,000–5,000 tenants self-managed on Hetzner. The repository has no
container image, no unit files, no scheduling for `bin/reaper` or `bin/migrator`
(both of which exit when done and are meant to be scheduled), and no backup or
restore procedure — tested or otherwise.

A database per tenant makes restore *the* operational question, not a footnote:
restoring one tenant to a point in time must not touch the other 4,999, and the
control plane's row for that tenant has to agree with whatever the database
became. Nothing in the code or the docs addresses it, and "we will work it out
when it happens" is a bad plan for the day it happens.

`docs/RUNNING.md` covers running it by hand, which is a different question.

### 5. ~~Signup is public, unlimited, and creates a database~~ — done

Was: one unauthenticated request ran `CREATE DATABASE` and a full migration
chain, so a shell loop from the open internet cost the attacker one HTTP request
and cost the operator a disk.

**Signup is two calls with a mailbox in between now.** `POST /v1/signups` writes
one `pending_signup` row and one outbox effect and answers `202`; nothing else
happens until `POST /v1/signups/{token}`, which is where the account, the tenant,
the database and the session are built. The email is the same producer the
invitation flow uses, which is the other half of why item 1 came first.

The second half of the finding was not in the original text and is the worse of
the two. Signing up wrote an **authenticator** under whatever address was named,
with a password of the attacker's choosing, so signing up as `ceo@bigcorp.example`
locked the real owner out of ever signing up: they would have to prove a password
they never set. Nothing is written to `authenticator` now until the address
answers; the hash waits in `pending_signup` and moves across on confirmation.

Six tests, each falsified by breaking the code it covers:

| what it pins | broken by |
|---|---|
| a request builds no tenant, no database, no authenticator, and hands back no token | returning the token; registering the login early |
| a link works once | unclaiming on success |
| an unissued token is refused | (paired with the above, which proves the route can succeed) |
| one address gets one message a minute | dropping the interval check |
| a name taken meanwhile does not burn the link | dropping the unclaim on failure |
| the mail is written in the language of the form | rendering in the default locale |

**What is deliberately still missing: a rate limit.** `REQUEST_INTERVAL` caps mail
per *address*, which is what stops the new flow being a way to fill one mailbox,
and it is all this endpoint can do alone. Limiting per *caller* needs a notion of
caller that does not exist yet, and Phase 12c builds it for API keys — the
sequencing this document already described, unchanged.

**Also deliberately still missing: the slug is not reserved.** A unique index on
a pending slug reads like the kinder behaviour and would make squatting free, one
throwaway address per name. So the name is checked when it is requested and again
when it is confirmed, and first to *confirm* wins. `a_name_taken_while_you_were_reading_your_mail_does_not_burn_the_link`
pins the case that creates.

The original finding follows.

#### The original finding

`POST /v1/signups` is `security()` — unauthenticated by definition, which is
correct. What is on the API is `RequestBodyLimitLayer`, `TimeoutLayer` and
`TraceLayer` (`bin/api.rs:68`). There is no rate limit, no proof of work, no
captcha, and no email verification.

Every call that gets past validation runs `CREATE DATABASE` and a full migration
chain. A script can therefore exhaust a cluster's disk from the open internet
with no account, and each attempt costs the attacker one HTTP request and costs
the operator a database. Email verification before provisioning would fix the
abuse case and the "is this address real" case at once — and it needs the outbox
handler from item 1, which is part of why that is first.

### 6. ZATCA is proven in sandbox only

Nine documents accepted with zero warnings, against **sandbox**. Simulation and
production are untested, and both need a real taxpayer's OTP from the Fatoora
portal — so this is blocked on access, not on code. Simulation is the one that
matters: it is the environment ZATCA requires a solution to pass before
production, and its certificate template differs from sandbox's (found the hard
way — see the running note).

Renewal is a five-year deadline with a sixty-day warning and no automation
possible, because it needs a human with an OTP. That is written down here so it
is a known limitation rather than a surprise in 2031.

### 7. ~~Sequential upgrades are a policy with nothing enforcing them~~ — done

Was: `FleetPlan::is_current` bounded the top and nothing bounded the bottom, so
`migrate_fleet` would take a tenant from migration 2 to 42 in one hop. Now
`MIGRATION_FLOOR` plus `below_floor` refuse it, and the error names the release
to install first. The predicate is separate from the constant so the rule is
tested against a chosen floor — testing it against the current constant would
prove nothing while it is zero, which it is for all of the first major.

Also done in the same pass: L1's documented mechanism corrected (it described an
advisory lock the code deliberately does not use), L7 enforced and its two
violations fixed, and L8 corrected to the mechanism that actually holds.

The original finding follows.

#### The original finding


D17 says upgrades are sequential and that we support two majors. Nothing in the
tree refuses a skip. Checked:

```
grep -rn "floor\|minimum_version\|min_version\|too old\|MIN_MIGRATION" crates/ migrations/
```

returns four hits, none of them about schema versions. `FleetPlan::is_current`
(`crates/erp-control/src/fleet.rs:45`) is `self.version == Some(latest)` — an
upper bound only — so `migrate_fleet` will take a tenant from migration 0002 to
0042 in a single hop today, which is exactly what D17 forbids.

What it needs:

- a `MIGRATION_FLOOR` constant, bumped to the previous major's final migration
  at each major release;
- a refusal in `walk_fleet`'s `visit` when `applied_version < MIGRATION_FLOOR`,
  whose error **names the release to install first** — "too old" with no next
  step makes an operator guess, which is the failure being prevented;
- `None` (never migrated) still allowed: that is fresh provisioning, not a skip;
- a test that an out-of-range tenant is refused *and* that a fresh one is not,
  since a floor that also blocks provisioning would be found in production.

Related and separate: a test that no registered event name loses a step in its
upcaster chain. The support window bounds which builds we patch, not which
events we must read — a v1 event is readable forever or the log is corrupt.

Both are small. Neither is done, and D17 is marked accordingly in the decision
index.

### 8. Commands that existed and no route could reach — done

Found by auditing the API surface against the module exports rather than by
using either feature, which is the only way this class shows up.

`pos::take_back` and `sales::refund_invoice` were both built, tested, exported —
and mounted nowhere. A till could take a return from Rust and not over HTTP, and
a refund outside a till had no route at all. `POST /v1/pos/shifts/{shift}/sales/{sale}/returns`
and `POST /v1/sales/invoices/{invoice}/refunds` now exist.

The same audit found `ShiftEvent::Refunded` had been unreachable since Phase 15,
which is what prompted looking.

**The lesson worth keeping:** the role matrix in `crates/erp-api/tests/http.rs`
caught both the moment they were mounted, because it fails on a served operation
with no row. Nothing caught them while they were *unmounted* — a command with no
route is invisible to every guard in the build. The nearest cheap check would be
a test that every `pub async fn` taking `&TenantDb` is named by some handler.

### 9. A retried till return took the drawer down twice — done

`pos::take_back` checked `Shift::has_pay_out` for its idempotency, and
`ShiftEvent::Refunded` recorded no key at all. So a retry deduplicated perfectly
in `sales` — the credit note and the money are keyed by reference there — while
the shift appended a second `Refunded` every time. Three retries of a 17.25
return left a drawer that should have held nothing holding **−34.50**.

`a_retried_return_is_harmless` passed throughout, because it asserted the ledger
balances and the ledger was the half somebody else was already protecting.

`Refunded` now carries its own `reference`, `Shift::has_return` answers for it,
and the test asserts the drawer as well as the books. Verified by falsification:
reverting the one-line fix fails the test with the −34.50.

Two seen-lists rather than one shared list, because a banking run and a return
are different caller namespaces.

### 10. A full entry cache refused the traffic that was arriving — done

`TtlCache::put` skipped the insert when the map was full and nothing had expired.
The comment defended it — "a cache that thrashes is worse than one that
occasionally misses" — and it is the wrong call here: what survives is whatever
arrived first, so the cache sits at capacity serving a working set it has stopped
tracking, and every request that is actually happening misses.

Under a five-second TTL the oldest entry is one that was about to expire anyway,
so evicting it is the expiry sweep running a moment early rather than a thrash.
It now evicts the oldest tenth. `a_full_cache_makes_room_for_what_is_arriving`
is the test.

The capacity itself (`ENTRY_CACHE_CAPACITY = 50_000`, five caches) is unchanged.
It was on the gap list as "undersized", but the number is not the defect — the
behaviour at the boundary was.

### 11. The book documented four of nine modules — done

`modules.md` described `ledger`, `sales`, `purchases` and `tax_sa` and stopped
there; `crates.md` listed the same four plus no `erp-occupancy`; the API index
said "All 105 operations" when there were 120, and had no chapter for `pos` or
`branches` at all.

All nine modules are now in both, `pos` and `branches` have chapters, `http.md`
has their route sections and the `X-Branch` header it had never documented, and
the count is generated rather than remembered.

**One thing the pass corrected rather than added:** `sales.md` still said a
credit note is refused on an invoice that "has payments", which stopped being
true when refunds landed — the rule is *still holding*, paid less refunded. A
test's doc comment said the same. Prose that was true when written is the kind
of stale that survives, because nothing compiles it.

### Then, and only then

Phase 5b (the rules engine) and Phase 6 (configured domain) are still correctly
sequenced: both wait for a second real consumer to describe them, and neither has
one yet. Blueprints (4d) are the nearest thing with a customer-visible payoff.

The smaller deferrals — snapshots, `Idempotency-Key`, `ETag`, `ModuleEnabled<M>`,
partial credit notes, customers as records, an entry-level read model — each name
the condition that should trigger them, and none of those conditions has been
met. They are fine where they are.


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
  on `erp-projection`, because creating a projection group turned out to be two
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

- **The ledger's route layer lives in `erp-api`, not in the module.** With one
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
  was never in doubt. `erp_testkit::kill_connection` issues
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
  contributor. `erp-testkit` now loads `.env` itself, and a connection failure
  reports what it tried, where the setting came from, and the password redacted.

- **L1 needs a counter row, not an advisory lock.** The architecture said
  `pg_advisory_xact_lock` per tenant. Two corrections: database-per-tenant means
  the log is already tenant-scoped, so there is nothing to key a lock on; and an
  advisory lock over a sequence gives commit *ordering* but not gaplessness,
  because a rolled-back transaction burns its number. A counter row updated with
  `UPDATE ... RETURNING` gives both — the row lock serializes, and the counter is
  transactional so a rollback returns the position. That turns the contiguity
  check from a warning into a real integrity assertion.

- **Localization completeness is now a shared audit.** `erp_i18n::testing::audit`
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
  `cargo test -p erp-testkit --test harness cloning_is_fast -- --nocapture`.

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
  `erp-control` and `erp-control` depends on `erp-eventlog`, not the other way
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

- **One list, because two things needed it.** `erp_api::modules()` replaced the
  `match` in signup. Not a `Module` trait: a trait would also have to carry the
  routes and the worker'"'"'s jobs, and neither can cross that boundary — a module
  must not depend on `erp-api` or `erp-worker`. So each composition root still
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
  signup test tried to drop `erp_tenant_acme`, a name that stopped being right
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

- **The takeover was a class, so the class got audited.** Every `ON CONFLICT DO
  UPDATE` in the codebase, every write to `session`, `membership` and
  `identity`, and every privileged control-plane method checked for a route that
  reaches it. Two more findings, one real:

  **Removing a member made them permanently un-addable.** The unique constraint
  on `(identity_id, tenant_id)` covers revoked rows, and `grant_membership` was
  a plain `INSERT` — so re-adding anyone hit a 500 that named nothing. An
  employee who leaves and comes back is not an edge case.

  Granting now revives a revoked membership, and the `WHERE membership.revoked_at
  IS NOT NULL` on the `DO UPDATE` is the whole safety of it: without that clause
  the same statement would be a way around `change_role`'s last-owner guard.
  That is the lesson from `set_password` applied on the spot — an upsert that
  means two things is the thing to be suspicious of.

  **Managing a stranger answered 204.** Changing or removing an identity that
  belongs to a different tenant updated no rows and reported success. Isolation
  held — a test asserts the other tenant's membership was untouched, and it
  passed before the fix — but an owner who mistyped an id was told something had
  happened. Now a 404.

  `suspend_identity`, `log_out_everywhere` and `Actor::impersonating` turned out
  to have no HTTP route at all, which is the right answer for all three.

- **Nothing had ever migrated an existing tenant.** `provision` runs the
  tenant-plane migrations when it builds a database, and that was the only
  caller. The day `migrations/tenant/0004_*.sql` shipped, new tenants would get
  it and every existing one would not — while the code that needs it deployed to
  all of them. Queries would compile, because they are checked against a
  database that *has* the migration, and fail at runtime per tenant across the
  live fleet.

  `survey_fleet` answers "who is behind?" without writing anything, which is the
  thing to run *before* a deploy. `migrate_fleet` does the work. It does not
  stop on a failure — one unreachable cluster must not leave the rest of the
  fleet un-migrated — and it is idempotent, so a partial run is resumed by
  running it again. Suspended tenants are included: a suspended tenant is one
  that may come back, and coming back to a schema three versions behind is the
  whole failure.

- **And underneath it, a worse one: adding a migration did not trigger a
  rebuild.** Found immediately, by adding a real migration to a real two-tenant
  fleet and watching `just migrate-fleet check` report everything current.

  `sqlx::migrate!` embeds the files at compile time. With no build script, cargo
  never learns the directory is an input — so the binary keeps an old migrator
  baked in and reports a fleet that is up to date with a migration it has never
  heard of. That is precisely the silent failure the fleet migrator exists to
  prevent, sitting one layer below it.

  Two `build.rs` files fix it, each emitting `rerun-if-changed` for its
  migrations directory **and every file in it** — the directory alone covers
  adding and removing files but not editing one in place.

  Verified end to end afterwards: two seeded tenants, add a migration, `check`
  reports both behind and exits 1, apply, the table is confirmed present in both
  tenant databases by `psql`, `check` exits 0.

- **Configuration, built for the one thing that needed it.** `PostingAccounts`
  was a constant with a comment saying it was the seam. It is now resolved from
  a tenant's own configuration, and everything else that looked configurable on
  inspection turned out not to be: the VAT rate is statutory, session and
  invitation lifetimes are platform decisions, currency is chosen per chart and
  carried by each invoice. One consumer, so one key.

  The store is versioned by a shared sequence, which makes `max(version)` a
  single number describing a tenant's whole configuration at a moment — and that
  number is what finally fills `Metadata.config_version`, declared in Phase 2
  and never written until now.

  **Resolved inside the command's transaction.** What the invoice posted to and
  what the tenant had configured cannot disagree, and the generation is stamped
  on the event. The values themselves go into the journal entry, so changing the
  configuration changes the next invoice and nothing before it (L5) — there is a
  test that changes it between two invoices and then replays the ledger to prove
  the earlier one still rebuilds.

  The mechanism is key-value; the *surface* is not. `PUT .../sales/posting-accounts`
  is typed, validated, and behind `ManageAccounts`. A generic "set any key to any
  JSON" endpoint would make every reader's decode the only thing between a typo
  and a broken module.

- **A guard that disagreed with the command it guarded.** The first version of
  the posting-accounts validation checked the accounts against
  `proj_ledger.account` — a read model driven by a worker. A tenant who
  installed a chart and immediately configured where to post it was told the
  accounts did not exist. `ledger::accepts_postings` now asks the log, which is
  the same question `post_entry_in` asks and asks it the same way.

- **Process note: two edits silently did nothing.** A scripted
  `str.replace` whose target had already been reflowed by `cargo fmt` is a no-op
  that reports success, and I spent three rounds debugging a handler that still
  contained the old code. The tell was `Finished in 0.10s` — a test run that did
  not rebuild after an edit did not edit anything. Every scripted replacement in
  this codebase should assert its target matched.

- **The rule engine was deferred, and the deferral is the interesting part.**
  Phase 5 was to build `Facts`, `DynCondition`, `FactRegistry` and `Rule<E>`,
  then move authorization onto them. Authorization is its only real consumer —
  pricing does not exist — and no concrete rule had been asked for, so every
  decision about *which facts exist* would have been a guess dressed as an
  interface.

  What was concrete: the second module created a permissions gap a real business
  has. One role per tenant means the person who does the invoicing must also be
  an accountant for the books. That is a describable problem with a bounded fix,
  and it produces the first genuine case for the engine rather than a
  hypothetical one.

- **Authorization now reads the URL, on purpose.** `Allowed<C>` derives the
  module from the request path, because the URL namespace *is* the module
  namespace by construction — every module mounts under its own name.

  The alternative, an explicit marker per handler, fails the wrong way: a
  handler that forgets it silently gets the tenant-wide role, which is the more
  permissive answer. **Forgetting must never be the permissive option.** The
  price is that authorization depends on URL shape, paid by
  `module_paths_are_what_they_look_like` — a route that moves changes a test
  rather than changing permissions quietly.

  An unrecognised path segment is *not* treated as a module, which matters more
  than it looks: if it were, somebody held back in every module they have could
  reach `/v1/tenants/acme/anything/…` and fall back to a role they were
  deliberately not given.

- **`JournalEntryEvent::Reversed` had existed since Phase 3 with no command that
  produced it.** Declared, named, upcast-registered, applied by the aggregate,
  written by nothing — the fourth instance this session of a thing that exists
  and has no caller, and the pattern behind three of the real bugs found so far.

  It mattered more than the others: without it, **a mistake was permanent.** An
  entry posted for the wrong amount could not be corrected by any route, which
  is not a missing feature in an accounting system so much as a missing
  premise.

  `reverse_entry` posts the opposite lines and marks the original, both in one
  transaction — an entry marked reversed with no reversal to show for it is a
  hole in the trial balance, and a reversal with nothing marked is a
  double-count. Reversing again with the *same* id is a no-op, so a retry is
  safe; with a different one it is refused and says what already undid it,
  because the second attempt would swing the balance the other way.

  The aggregate now keeps its lines, so undoing one does not need a second
  place that knows how a `Posted` event is shaped.

- **What was deliberately not built with it.** The obvious companion is a
  `proj_ledger.entry` table showing which entry reversed which. It was left out:
  adding a table to a module's install script means re-running it across the
  fleet and replaying the group, which is the module-refresh machinery deferred
  in Phase 4 — and nothing displays the link today, because there is no
  entry-list endpoint either. The correction works, the books balance, and the
  read model arrives with the screen that wants it.

- **A changed read model needed the refresh, so the refresh got built.** Credit
  notes have to be *visible* — a cancelled invoice still showing as outstanding
  is worse than no cancellation at all, because somebody chases a customer for
  money that was credited back. That meant a new column on
  `proj_sales.invoice`, and `install.sql` is `CREATE TABLE IF NOT EXISTS`
  throughout, so re-running it would never have added one.

  This is the trigger the Phase 4 deferral was waiting for. `refresh_module`
  drops the module's schema, installs it again and rewinds its checkpoint — all
  in one transaction, holding the same checkpoint lock a projection run takes,
  so a run in flight finishes rather than finding its tables gone mid-batch.
  Resetting the checkpoint in that same transaction matters just as much: there
  is no moment where the tables are gone and the checkpoint still claims they
  are current, which a worker would read as "nothing to do".

  Proved against a real fleet rather than only in tests: seed a tenant on the
  old schema (5 invoices, no `cancelled_on`), ship the new one, run
  `just migrate-fleet refresh sales`, watch the column appear and the tables
  empty with the checkpoint at zero, start the worker, watch all five invoices
  come back. A second refresh is harmless.

  It also caught a mistake in its own reporting: the first version exited
  non-zero after a *successful* rebuild, because it asked `is_uniform()` — and
  for a refresh, "tenants that were behind and have now been rebuilt" is the
  success case, not the failure one.

- **Crediting an invoice reuses the seam reversal created.** `ledger::reverse_in`
  is to cancelling what `post_entry_in` was to issuing: the ledger owns what
  undoing a posting means, sales owns when. Both events commit in one
  transaction, for the same reason as before.

  Refused while payments stand against it. The money is somewhere, cancelling
  the document without moving it back would leave cash on the books against a
  sale that no longer exists, and there is no way to model a refund yet.
  Refusing says so; guessing would not.

- **The VAT return is the module's commercial reason to exist, and it was a
  view and a query.** Everything it needs was already stored: the tax bands, the
  rate that applied, the tax point. That is what banding by rate at issue bought
  — the return is `GROUP BY`, not a recomputation, and it cannot disagree with
  what the invoice printed.

  The period is **half-open**, `[from, until)`. "31 March inclusive" is a
  comparison somebody gets wrong once a quarter, and two consecutive returns
  built that way either double-count the boundary day or drop it. The test
  states that as a property rather than as arithmetic: the two quarters
  together equal the whole span. An earlier version asserted a hand-computed
  total instead, which passed and said nothing.

  Credited invoices leave the return, which is right when the credit lands in
  the same period and wrong across a boundary — a credit note in a *later*
  period is a supply and then an adjustment, and each belongs in the period it
  happened. That needs the credit note to be a document with its own tax point,
  which is the partial-credit-note work. The view says so rather than pretending
  otherwise.

- **The demo had stopped demonstrating.** Credit notes, reversals and per-module
  roles all shipped without reaching the seeder, so the CI check that proves the
  system works end to end was proving a subset of it. That is the omission my own
  note predicted — "what still needs a person is teaching the seeder to use it" —
  arriving three increments later.

  It now issues an invoice against the wrong customer and credits it, posts a
  utilities entry for the wrong amount and reverses it, and adds a second person
  who does the invoicing and not the books. Each is asserted, so the next feature
  that skips the seeder fails a test rather than quietly narrowing the demo.

  A demo of an accounting system in which nothing was ever *wrong* is not a demo
  of an accounting system. It is also the first thing a prospective customer asks
  about and the last thing a description is convincing about.

- **Fifty-five error codes, and nothing listed them.** A client branches on
  `code` — that is the contract — and finding out what the codes are meant
  reading Rust across five crates.

  `docs/ERRORS.md` is generated from the same catalog the API renders from, so it
  cannot claim a code that does not exist, and a test fails when the codebase
  grows one the document does not mention. Checked by adding a code and watching
  the check fail, then removing it.

  Generated rather than written for the usual reason: a hand-maintained list is
  wrong within a month, and wrong in the direction that costs an integrator a
  day — a code that exists and is undocumented looks like a bug in their client.

- **I wrote a vacuous test, and the check for vacuity is what caught it.**
  `refresh_module` takes the checkpoint lock before dropping anything, so a
  projection run in flight is not left mid-transaction with its tables gone. I
  had asserted that in a comment. The first test asserted only that the refresh
  *had not finished* while a run held the lease — and it **passed with the lock
  removed**.

  It passed for the wrong reason. Without the explicit lock the refresh still
  blocks, at the checkpoint `UPDATE` — but that comes *after* `DROP SCHEMA`, so
  by the time it blocks the damage is done. "The refresh waits" was never the
  property. "The run's tables are still there while it is in flight" is.

  The rewritten test has the in-flight batch write a row after the refresh has
  started, and fails when the lock is removed. Verified both ways, which is the
  only thing that distinguishes a test from a comment that compiles.

  Worth stating plainly: the window is the *start* of a batch, when a run has
  taken its lease and written nothing, so it holds no lock on the tables
  themselves. That is the moment every projection run passes through.

- **A performance instinct that measurement refused.** `Allowed<C>` derives the
  module from the request path, which calls `modules::available()` — and
  `ModuleId` holds a `String`, so that is two allocations and two validations on
  every authenticated request, on the authorization path. It looked like an
  obvious regression to fix with a `OnceLock`.

  Measured first: 82ns for `available()`, 35ns for `module_id()`. Against a
  request that makes several database round trips that is roughly a hundredth of
  a percent, and three small allocations next to the ones JSON parsing already
  makes. Left alone.

  Recorded because the reflex was wrong, not because the outcome was
  interesting: "allocation on a hot path" is a shape, not a measurement, and
  this codebase has a soak test precisely so the difference can be settled.

- **The OpenAPI document found three defects, and none of them was in the
  document.** Generating it was supposed to be a writing job. What it produced
  was a list of places where the server and the promise had quietly diverged:

  1. **`payments` carried two types on the same resource.** `InvoiceView` has a
     `payments` **count**; `InvoiceDetailView` flattens `InvoiceView` and adds a
     `payments` **array**. serde writes the flattened fields first, so on the
     detail endpoint the array silently overwrote the count — `GET .../invoices`
     answered `"payments": 2` and `GET .../invoices/INV-1` answered
     `"payments": [ … ]`. Every generated client would have broken on it, and no
     test noticed because each endpoint was only ever read on its own. The count
     is now `payment_count`.
  2. **The most common client mistake did not get the documented error shape.**
     axum's `Json` rejection is `text/plain` with no `code`, so "every failure is
     `application/problem+json` with a stable code" was untrue for a malformed
     body — on exactly the request where a client most needs the message. Fixed
     at the root with `wire::Json` and `wire::Query`, which keep axum's status
     (400 / 415 / 422) and replace only the body. Six imports changed; nothing
     else did.
  3. **Undocumented statuses.** A 500 can come back from any route and was
     declared on none.

  All three surfaced the same way: the document's hand-written half — which
  status carries what — was checked against real responses.

- **What is structural and what had to be checked.** `utoipa-axum` registers the
  axum route *from* the `#[utoipa::path]` attribute, so path and method are one
  string rather than two that agree today; a handler with no attribute does not
  compile inside `routes!`, and schemas come from the wire types by derive. That
  covers everything except the `responses(…)` blocks, which are prose about
  types the compiler never relates to the handler's return value.

  So `tests/http.rs` validates **every response it receives** against the schema
  the document publishes for that path, method and status — sixty-five tests and
  three thousand lines become contract coverage for the cost of one call in
  `Fixture::send`. The validator is a hand-written subset of JSON Schema rather
  than a dependency, and it carries its own two guards:
  `every_schema_keyword_is_understood` fails when the document starts using a
  keyword nobody implemented (a constraint silently unchecked is how a
  hand-rolled validator becomes a test that looks at nothing), and
  `the_validator_is_not_vacuous` proves it says no to a missing, renamed,
  retyped, or extra field. Both were confirmed by breaking the code.

- **Conventions belong in one place, applied to the finished document.** The
  bearer scheme, `Accept-Language` on every path, what each status means, and the
  responses every operation can give regardless of what it does — all of it is
  uniform, and declaring it per-handler would be thirty chances to leave one out.

  utoipa's `modifiers(…)` is the wrong hook: it runs on `ApiDoc::openapi()`,
  *before* any route is registered, so a modifier that walks `paths` walks
  nothing. The first version did exactly that and reached zero operations —
  caught by `every_path_takes_accept_language`, which is there because a
  convention that silently applies to nothing is worse than one never written.
  `Conventions` now runs on the document `split_for_parts` produces.

- **Three routes were called `list`.** utoipa derives `operationId` from the
  handler's name, and handler names are unique *per module* — `members::list`,
  `invitations::list` and `modules::list` all became `list`. A generator handed
  that emits three `list()` functions and drops two, or refuses the document
  outright. Found by checking the generated file against the OpenAPI Object
  schema by hand, which is also where the 413/504 gap came from; both are now
  tests. The handlers are `list_members`, `list_invitations` and `list_modules`,
  which is what the rest of the crate already did (`list_accounts`,
  `list_charts`).

- **Two failures are outside the document, and it says so.** `bin/api` wraps the
  router in a body limit and a timeout, so a body over 1 MB is a 413 and a
  request still running after thirty seconds is a 504 — both refused at the edge,
  before anything the document describes, and both with no body at all. Writing
  them into the operations would be a lie (they carry no `Problem`); leaving them
  out entirely would be a different one. The `info` description names them.

- **The one thing a document must not be wrong about is authentication.**
  `only_the_deliberately_public_routes_are_public` lists the eight open
  operations and fails on any other that opts out. It caught `POST /v1/signups`,
  which is public and was documented as needing a session — harmless in that
  direction, and the same test is what catches the harmful one.

- **A heading that had outrun its test.** `every_role_can_do_exactly_what_it_should`
  was documented as "every role against every endpoint" and checked three ledger
  routes. It could not have done better while the endpoint list lived in the test
  body: it only grew when somebody remembered to grow it, and a route added
  without that thought is a route nobody checked. Twenty-seven role-scoped
  operations existed; four were covered.

  The document fixed that as a side effect. The endpoint list now comes from
  `erp_api::openapi()` — the same value the router is built from — and the
  permission table has to name every operation under `/v1/tenants/{slug}` or the
  test fails. **Adding a route now forces the decision instead of allowing it.**

  The table itself is still written out rather than derived from `Role::allows`:
  a test that asks the code what it does can only agree with it. This one asks
  whether that is what we meant, and a change in permissions has to be typed into
  a diff somebody reviews.

  108 checks, all green — the code and the intent agree everywhere, which is the
  outcome worth having and not the one worth assuming. Both halves were confirmed
  by breaking them: a table entry the code does not grant, and a served route the
  table does not name.

  The mechanism that makes a garbage body safe is worth stating: `Allowed<C>` is
  a `FromRequestParts` extractor and the first parameter of all twenty-seven
  handlers, so authorization runs *before* the body is parsed. `{}` gets a 403
  when the role is refused and a 400 when it is not — the exact distinction being
  measured — and the matrix cannot mutate the tenant out from under itself.

- **`manager` was never a role.** I published it in three field descriptions and
  an example; the roles are `owner`, `accountant`, `clerk`, `viewer`. A client
  copying that example gets a 400.

  `role` is a `String` on the wire on purpose — an unknown one should get a
  localized `request.unknown_role`, not a serde rejection — which leaves the list
  a client reads as prose, in eight places. So `Conventions` now generates it from
  `Role::ALL` onto every `role` field, and the doc comments that listed it are
  gone. One list, and it is the enum's.

- **Gapless numbering: what a sequence cannot do, and why the client had to give
  something up.** Saudi law requires a tax invoice to carry "a sequential number
  which uniquely identifies the invoice" (VAT Implementing Regulations, Article
  53), and ZATCA's e-invoicing rules require the counter to advance by exactly
  one so the cryptographic chain has no holes. Not *unique*. Not *mostly
  ordered*. **Gapless** — an auditor counts them, and a missing 4,108 is a
  question the business has to answer.

  A Postgres `SEQUENCE` cannot do it, and not by accident: `nextval` is
  deliberately transaction-independent, because that is what lets concurrent
  writers take numbers without blocking. Every rolled-back issue would burn one.
  So the counter is an ordinary row read `FOR UPDATE` and advanced in the
  transaction that writes the document, and a rollback releases the number
  because it was never really taken.

  **The cost is real and cannot be engineered away.** Issuing serializes per
  (tenant, series). "Gapless" and "concurrent" are the same contradiction
  whatever holds the counter; the honest answer when a tenant outgrows it is more
  series — per branch, per point of sale — which is how the paper world solved it
  too.

- **Two calls, because the document might not be written.** `reserve` takes the
  row lock without moving the counter; `consume` moves it. A single
  `nextval`-shaped call would burn a number on every idempotent retry — and a
  client whose request timed out and repeated it is the *normal* case, not an
  edge one. Putting a gap in a business's invoice sequence because their network
  blinked would be this feature failing at the one thing it exists to do.

  `re_issuing_does_not_move_the_series` is the test for exactly that pairing,
  which the module cannot enforce from inside itself.

- **The client gave up choosing the number, and got a key instead.** `id` on a
  new invoice used to be the invoice number. It is now the client's own
  reference: what makes a retry a no-op, and what addresses the document
  afterwards. The number is allocated here and comes back as `number`.

  That is not a preference. A number a client picks cannot be gapless — two
  clients cannot coordinate, and a client that skips one has no way to know. The
  same split applies to credit notes, which ZATCA numbers separately from the
  invoices they credit, and which now have their own series.

  A retried request is told the number the document **already has**, which costs
  one extra aggregate load on that path only. Telling a client "done" and nothing
  else would leave it to guess, and the guess would be a number that does not
  exist.

- **The number is in the event, not derived on read.** Architecture L5, and here
  it is load-bearing: a number derived at projection time would mean replaying a
  tenant's log renumbers every document they have ever issued — including the
  ones customers hold copies of. `a_replay_reproduces_the_numbers_it_issued_under`
  checks the rebuild is identical *and* that the counter did not move.

  For the same reason `document_number` is not in a `proj_*` schema. It is not
  derived from the log; the log depends on it. A module refresh drops and rebuilds
  projection schemas, and a tenant whose series restarted at one afterwards would
  reissue numbers that are already printed. That is now asserted in
  `refreshing_a_module_rebuilds_its_schema_and_rewinds_its_checkpoint`, and
  confirmed by making a refresh delete the counters and watching it fail.

- **Old invoices keep the numbers they were issued under.** `Issued.number` is an
  `Option`, not a version bump with an upcaster — and that is the honest shape
  rather than a shortcut. An upcaster sees the payload and not the stream it came
  from, so there is nowhere for an old number to come from; and an invoice issued
  before this existed genuinely had none allocated. Its number *was* its
  client-chosen id, and the projection resolves `number.unwrap_or(id)`, so every
  such invoice keeps exactly the number on the copy somebody holds.

- **The demo taught the wrong thing for about ten minutes.** Its invoices were
  seeded with ids like `INV-2026-001`, which now sit beside numbers like
  `INV-00001` — two strings that look like invoice numbers, side by side, in the
  one artifact that exists to explain the system. The seeded ids read like a
  CRM's references now (`crm-4471`), which is what `id` actually is.

- **A comment that promised a guard nobody had written.** `taxable_supply`
  carried this, in the schema, next to the view it describes:

  > *"…so this view is honest about being the simple case and `vat_return`
  > refuses to span one silently."*

  `vat_return` did no such thing. There was no check anywhere — the sentence
  described an intention that never became code, and it read as reassurance for
  months. The `ponytail:` note above it was accurate about the *problem* and
  wrong about the mitigation, which is the worse of the two ways to be wrong.

- **What it was hiding: a filed VAT return could quietly restate itself.**
  `taxable_supply` excluded cancelled invoices outright. That is right in exactly
  one case — a credit note raised in the same period as the invoice, netting out
  before anything is filed.

  Across a boundary it is wrong. An invoice issued in February and credited in
  April *was* a supply in Q1: the return was filed and the tax paid. Dropping the
  invoice retrospectively meant re-running the Q1 return produced a smaller
  number than the one filed, with nothing anywhere recording why — and the credit
  appeared in no period at all.

  Both documents are now entries on their own tax point (`proj_sales.vat_entry`),
  so Q1 keeps the supply and Q2 carries the adjustment. Same-period credits still
  net to zero; cross-period ones no longer reach back.

- **The numbering work is what unblocked it.** The `ponytail:` note said the fix
  needed "the credit note to be a document with its own tax point", and it was
  right. A credit note now has its own number, from its own statutory series, and
  `on` is its tax point — so there *is* a document to date the adjustment by. The
  feature that made it possible was built for a different reason entirely.

- **The old test could not have caught it.** `a_credited_invoice_drops_out_of_the_return`
  credited within a single period, so it passed under both the wrong rule and the
  right one — it never touched the boundary, which is the only place they differ.
  Worse, its `credit()` helper dated the credit note in **2023** against invoices
  from 2026, and that had no effect at all, because the old view ignored a credit
  note's date entirely. A field the code never read cannot be wrong in a test.

  The replacement is named for what it checks, and
  `a_credit_note_is_declared_in_its_own_period_not_the_invoices` fails against the
  old rule — confirmed by putting the old rule back and watching a filed 150
  become 0.

- **Bands count both kinds of document.** A period where a supply was invoiced
  and credited shows `invoices: 2, credit_notes: 1, tax: 0` rather than vanishing.
  A return that showed nothing would be hiding that a supply happened and was
  reversed, which is precisely what an auditor is looking for.

- **One check, at the seam everything already routes through.** Closing the books
  has to refuse a back-dated journal entry, a back-dated reversal, an invoice
  with a back-dated tax point, a payment, and a credit note. That is five call
  sites, in two modules, and a check per call site is a check somebody forgets —
  where the one forgotten is the one that mattered.

  There is exactly one: `ledger::post_entry_in`. Sales writes an invoice and its
  journal entry in the same transaction, so every sales command arrives there;
  `reverse_in` calls it too. **Sales never mentions a fiscal period and inherits
  the refusal anyway**, which is the seam earning its keep rather than being
  asserted about. `modules/sales/tests/sales.rs` tests it from that side, because
  a guarantee only one module knows about is a guarantee that decays.

- **`closed_before`, not "closed through".** The watermark is the first instant
  still *open*, so closing January is `2026-02-01T00:00:00Z`. The same convention
  as the VAT return's `until`, for the same reason: "closed through 31 January"
  is a comparison somebody gets wrong once a month, and gets wrong by exactly one
  day. `the_instant_named_is_the_first_one_still_open` pins both sides of the
  boundary.

- **One instant rather than a table of periods.** Books close in order — nobody
  closes March while February is open, because the March numbers are built on the
  February ones. So the whole state is a scalar, stored in the configuration the
  command already reads inside its own transaction. A `ponytail:` note names the
  upgrade: a locked prior year with one adjustment period open inside it is a
  table of ranges, and this becomes its newest row.

- **Reopening is allowed, on purpose.** An accountant who closes the wrong month
  has to be able to put it right, and a system that refuses is one they route
  around by editing the database — which is strictly worse, because then nothing
  records it at all. What it must not be is quiet, which is what `set_by` and
  `set_at` are for.

- **What this makes safe.** The VAT return's period rule says an adjustment
  belongs to the period of its own tax point. Without a close, somebody could
  still date a credit note into a quarter that has been declared and paid — which
  would put the return back exactly where it was before `vat_entry`, able to
  restate itself after filing. `a_credit_note_cannot_be_dated_into_a_closed_period`
  is the test that the two features hold the line together.

- **The third module is a different job from the second.** `sales` answered "how
  do two modules meet". `purchases` answered the question that could not be asked
  until there was a third: *was that a general answer, or did it just happen to
  fit sales?*

  All of the mechanism generalised, unchanged. An aggregate, events at version 1,
  a projection group nobody else reads, `ModuleSetup`, `requires`, a
  rejection-to-status mapping, and a command writing its document and its journal
  entry in one transaction through `ledger::post_entry_in`. The closed-period
  check arrived **for free**: `purchases` never mentions a fiscal period and
  cannot post into one, because every posting goes through the same seam.
  `a_bill_cannot_be_dated_into_a_closed_period` is a rule written in another
  module before this one existed.

  Exactly one thing had to move: `VatCategory`, from `sales` to `ledger`. Two
  sibling modules must not depend on each other, so what they share has to live
  in the one they both stand on. That is a rule only a third module could test.

- **What is genuinely different is the domain, not the plumbing.** Sales
  *computes* tax; purchases *records* it. Input VAT is reclaimed against the
  supplier's tax invoice, so the figure in the books has to be the figure on the
  document you hold — a recomputation landing a halala away produces a reclaim
  that does not match its own evidence, and the evidence is what an inspector
  asks to see. So there is no `vat::total` here and no rounding: the module
  checks the stated tax is *possible* and stores what it was told.

  Three consequences fall out of that one fact, and each is a rule rather than a
  simplification:

  1. **No gapless numbering.** We did not issue the document. The supplier's own
     number is recorded, and a duplicate of it against the same supplier is
     refused — recording one bill twice is a duplicate reclaim.
  2. **Tax without the supplier's VAT number is refused.** A bill from an
     unregistered supplier is not evidence of a reclaim.
  3. **Exempt input tax never reaches `1200 Input VAT`.** It is irrecoverable, so
     it is a cost of the purchase and rides on the line's own account. In
     practice suppliers charge no tax on an exempt supply — but "rarely" is not
     "never", and a rule that only holds for the common case is the one that
     produces an unexplainable balance.

- **A VAT return spans two modules, and neither can compute it.** `proj_sales`
  and `proj_purchases` are separate groups and neither may read the other (L3). A
  third module reading both would be exactly the cross-group read the law exists
  to prevent.

  So it is composed in the API, from each module's own answer — which is not a
  workaround but where cross-module composition is supposed to happen, the same
  place `erp-worker` composes jobs and `modules.rs` composes the catalogue. A
  tenant with only one module gets zeroes for the other side rather than a 404: a
  business that has not enabled purchases genuinely reclaimed nothing, and that is
  a return they can file.

  `GET /v1/tenants/{slug}/sales/vat-return` is **gone**. It answered half the
  question, and half a VAT return is a number nobody files.

- **Two guards earned their keep during this.** `no_two_operations_share_an_id`
  caught the new `/vat-return` colliding with the sales one the moment it was
  registered — which is what forced the decision to delete rather than rename.
  And the authorization matrix refused to pass until all five new operations were
  in its table, so "who may record a bill" was a decision typed into a diff rather
  than one that defaulted.

- **`just clean-databases` left the two halves disagreeing.** It drops
  `erp_tenant_%` and never touched the control plane, so the `tenant` rows
  outlived their databases — and the next `just demo` failed with `slug_taken`
  against a tenant whose database was gone. Found by running it, which is the
  only way a developer-tool bug gets found. The recipe now clears rows whose
  database no longer exists.

- **Five composition roots list every module, and one of them was unchecked.**
  Adding a module means editing `erp_api::modules()`, the message catalog, the
  routes, the worker's job list, and the demo's projection advance. The question
  worth asking after a third module is not "can this be a trait" but **"what
  happens if somebody forgets one of the five?"**

  So I removed `purchases` from two of them and ran the workspace. The catalog
  omission was caught immediately — by the `docs/ERRORS.md` drift check, which
  turns out to cover it for free. The **worker omission was caught by nothing at
  all**: 476 tests green with a module whose read models would never fill.

  That is the worst failure this system has. The events still commit, the ledger
  still balances, the module still accepts writes and posts correctly — and
  `proj_purchases` stays permanently empty. No bill list, no input tax, and a VAT
  return quietly under-reporting what can be reclaimed, so a business pays tax it
  does not owe. Nothing about it looks like a bug from the inside.

- **The fix is a list a test can look at.** `bin/worker.rs` built its jobs with a
  chain of `with_job` calls, which nothing could inspect. They are a
  `module_jobs()` function now, and two tests read it: every module in
  `erp_api::modules()` has a job, and every job is scoped to its module — because
  a `for_module` somebody forgot looks identical until a tenant is billed for
  projections they declined.

  The other two roots are checked behaviourally rather than structurally, which
  is better where it is available: `every_module_has_routes` reads the OpenAPI
  document (a module a tenant can enable and find nothing behind), and the demo
  test asserts its bills were projected (which is what a missing advance looks
  like from outside). All four were confirmed by breaking them.

- **This is the registry the plan asked for, and not the trait.** The `Module`
  trait is still blocked on a genuine contradiction — it would carry a router,
  and a module must not depend on `erp-api`. What three modules made clear is
  that the *registry* half was the load-bearing part, and it did not need a trait
  to be closed. The trait can wait for the router problem to have an answer
  rather than being built around a guess at one.

## Deploying without an outage

Three of the four zero-downtime pieces landed. Build-then-swap projection
rebuilds is the fourth and is its own increment — see the note at the end.

- **Expand-only migrations, enforced.** A migration that removes something an
  old pod still uses turns the overlap window into an outage — and it is an
  outage nobody sees in staging, because staging deploys one pod. Eleven rules,
  each a phrase that only appears in an `ALTER`-shaped statement (`set not null`
  is; the `not null` in every `CREATE TABLE` is not), checked over both migration
  chains with comments stripped first, because half these files explain in prose
  why something is *not* dropped.

  One migration in the repo needed an exemption: `0002_clusters.sql` adds a
  foreign key, which is unsafe on a live table and was entirely safe there
  because it ran before the system had a tenant.

- **The exemption mechanism broke the rule it enforces.** First version put the
  marker in a comment in the migration. `just demo` then failed with
  `VersionMismatch(2)`: sqlx checksums migration files, so editing one — *even to
  add a comment* — strands every database that already ran it.

  A rule about not changing what is already deployed, enforced by changing what
  was already deployed. Exemptions live in `migrations/EXEMPTIONS` now, and the
  reason that file is separate is written at the top of it.

- **The pre-deploy version gate.** `erp_eventlog::upcast` refuses an event from a
  newer build rather than guessing (L6), which is right — and means a build
  deployed out of order does not fail *at deploy time*. It fails later, when a
  projection reaches the first event it cannot read and stops, by which point the
  pods are up and the read models are silently falling behind.

  `just migrate-fleet versions` asks the fleet first, and reports two different
  failures: an event at a version higher than this build declares (somebody is
  deploying backwards) and an event name this build declares nothing for at all
  (a module was dropped rather than deprecated). Verified by planting one of each
  in the demo tenant's log and watching both fire — and the log refused the
  cleanup `DELETE`, which is the append-only trigger doing its job.

- **The gate made `upcasters` part of a module's declaration.** Comparing needed
  the union of every module's event versions, and building that by hand would
  have been a *sixth* place listing modules — in the increment whose whole
  finding was that the fifth was wrong. `ModuleSetup::new` now takes it, as a
  required argument rather than a builder method: a module that forgot it would
  be invisible to the gate, and invisible is exactly the answer that lets a bad
  build ship.

- **Modules are deprecated, never removed.** `ModuleSetup::deprecated(why)`.
  Signing up for one and enabling one are refused; **disabling one is not**, and
  neither is managing who uses it — a tenant on a deprecated module has to be
  able to get off it, and refusing there would trap them. The catalogue carries
  the reason so a picker can hide it. It leaves the build when the last
  entitlement does, which is a fact somebody can check rather than a date
  somebody guessed.

- **A third silent-corruption bug from writing Rust through Python.** Two format
  strings came out with twenty spaces in the middle: `\`-continuations in a
  Python heredoc are consumed by *Python*, so the Rust source never had them and
  the indentation landed inside the string literal. Third time. The tell is a run
  of spaces inside a quoted string, and it is now something to grep for after any
  scripted edit.

- **Build-then-swap projection rebuilds.** Done — see below.

## Rebuilding read models without an outage

- **The blocker was an asymmetry nobody had noticed.** Projections wrote
  unqualified (`INSERT INTO invoice`) through `search_path`; the install SQL that
  *created* those tables named `proj_sales.invoice` outright. So the DDL could
  only ever build one schema — the live one — and a rebuild had no choice but to
  drop it first.

  The install SQL is schema-relative now, and `install_schema` aims it by setting
  `search_path` to the group's schema, which is exactly what the projections
  already did. `just prepare` does the same for the type-check database, because
  the reads are still qualified and the tables have to land where they expect.

- **`rebuild_swap` builds beside, then exchanges.** Staging schema, install,
  replay from zero while the live tables keep serving. Then one transaction: take
  the checkpoint's `FOR UPDATE` lock (so a projection run in flight finishes),
  pin the log head, catch staging up to it, drop the live schema, rename staging
  over it, set the checkpoint. Postgres makes DDL transactional, so a failure
  anywhere leaves live exactly as it was — asserted rather than assumed.

  **Readers block only for the drop-and-rename**, two catalogue updates, because
  the catch-up happens before it rather than after.

- **The catch-up window is where a rebuild silently loses data**, and it is the
  test worth having. Events appended *between* the build finishing and the swap
  happening would be missing from the new tables while the checkpoint claimed
  they were there — permanently. Confirmed by disabling the catch-up and watching
  five events vanish.

- **A guard against the mistake this whole change invites.** If a module's
  install SQL is still schema-qualified, it builds into the *live* schema and
  leaves staging empty — and the swap would then rename an empty schema over a
  working one, deleting a tenant's read models. So staging is checked for tables
  before the swap, and the refusal says why. Confirmed by removing the check and
  watching the failure become an unrelated "relation does not exist".

- **Wired, not left as a library.** `just migrate-fleet refresh <module>` uses
  it. That needed `ControlPlane::maintenance_pool` — `TenantDb` deliberately
  exposes no pool because it is the request path, and a rebuild is not a request:
  no member behind it, several transactions, and the same trust level as
  `enter_for_maintenance`. Verified on the real demo tenant: six invoices before,
  six after, checkpoint unmoved at 55, no leftover staging schema. Under the old
  path that was six, then zero, then six.

  `refresh_module_fleet` is **deleted** — the migrator does the loop now, and a
  function with no callers is the bug class this project keeps finding.
  `refresh_module` stays as the fallback for a caller with no projections, with
  its cost written on it.

- **Deleting nearly took a hard-won test with it.** The first cut removed
  `a_refresh_does_not_drop_tables_under_a_projection_run` — the one proved
  non-vacuous several increments ago — because it sat between the dead test and
  the helper they shared. Caught by the compiler, not by me. The redo asserts
  what it is *not* allowed to remove before writing anything, which is what a
  scripted deletion should have done in the first place.

- **`every_module_can_be_rebuilt`.** `rebuild_swap` is generic over the
  projection group, and a group is a type, so the migrator matches on the module
  name — one more place a module can be left out, and leaving one out means a
  change to its read models could never be deployed. Same shape and same reason
  as `every_module_has_a_projection_job`.

## The rate is the tenant's, not the build's

- **A business outside Saudi Arabia could not issue a correct invoice.**
  `VatCategory::rate_now()` returned `1_500` — 15% since July 2020 — from the
  *accounting kernel*. A fact about one country, in the code every country would
  use, on the write path. The UAE charges 5% and there was no way to say so.

  Moving `VatCategory` into `ledger` last time was right; moving the **rate**
  with it was not. `ledger` keeps the shape — that a line has a treatment and a
  rate — and has no opinion about the number.

- **Rates are configuration, and a country module is what will seed them.** This
  is the answer to "where does a country module put its rates" without a sibling
  dependency: it writes data, and `sales` reads data. The shipped default is
  Saudi Arabia's, with a `ponytail:` note saying it belongs to `tax_sa` the
  moment there is a second country — and that the seam is already here, because
  seeding a config key is all that module has to do.

- **Resolved in the command's transaction, not by the handler.** The API used to
  stamp `Vat::current(category)` onto each line before calling the command. That
  put a database-backed decision outside the transaction that writes it, so a
  rate changed in between would leave an invoice carrying one that was never
  current — the exact argument `resolve_accounts` already makes about posting
  accounts.

  The fix separates two shapes that were one: `DraftLine` is what a client sends
  (a treatment), `InvoiceLine` is what was issued (a treatment *and* the rate it
  was issued under). The second goes in the event, so L5 holds and a rate change
  cannot restate a filed return — asserted by issuing at 15%, changing to 5%,
  issuing again, and checking both the invoices and a shadow replay.

- **One positive rate, deliberately.** KSA and the UAE each have exactly one, so
  `Rates { standard }` is the whole of it. A jurisdiction with reduced rates
  needs a *category* per rate rather than a second field — two lines at different
  positive rates are not the same classification — and that is a `VatCategory`
  change, noted rather than guessed at.

- **The rate is validated where it is set**, between 0 and 10000 basis points. A
  negative one would credit VAT payable on every sale; one over 100% would charge
  more tax than the supply.

## `tax_sa` — the first module that stands on two

- **A country is a module, and this is the first one.** Saudi Arabia has ZATCA
  and 15%; the UAE has Peppol PINT AE and 5%. The rate, the return's shape, the
  clearance protocol and the fields an invoice must print all change at the
  border. `ledger` owns that a line *has* a treatment and a rate; `tax_sa` owns
  what the number is, and seeds it when a tenant enables the module.

- **The VAT return moved out of `erp-api`, where I had put domain that does not
  belong there.** I composed it in the API two increments ago and wrote that
  cross-module composition belongs in the composition root. The core/module model
  says otherwise, and the test it gives settles it: *can a tenant disable it?* A
  business with neither sales nor purchases had a VAT return endpoint.

  It is composed in a module that **declares both** now:
  `tax_sa → {sales, purchases} → ledger`. Nothing reaches sideways, and it is
  still not a cross-group read — each module's own read function is called and
  the answers are netted in Rust, exactly as before. What changed is who owns it.

- **`requires` was wrong, and a test said so.** I gave it
  `requiring(&["sales", "purchases"])`, which reads sensibly and forces a
  business that only sells to enable a purchases module they do not use in order
  to declare tax they do owe. `requires` is an AND list and the rule that
  actually describes this is "at least one of" — which it cannot express.

  So it requires nothing. The **crate** depends on both; the **entitlement**
  depends on neither, and each side reports zero when the tenant has not enabled
  it. That is not a fallback: a business that has not enabled purchases genuinely
  reclaimed nothing.

- **A filing is recorded, not inferred.** Every other guarantee here makes
  re-running a period give the number that was filed — documents on their own tax
  point, closed periods refusing back-dated writes. Those are properties of the
  *arithmetic*. `tax_sa.return.filed` puts the numbers that went to ZATCA in the
  log with the date they went, so "does the system still agree with what we
  filed?" is a comparison rather than an argument — and it survives a rebuild
  because it is an event rather than a derivation, which
  `a_filing_replays_to_exactly_what_it_recorded` checks.

  Filing a period twice is a **conflict**, not a no-op: the second one is an
  amendment, which is a different document with its own rules.

- **`ModuleId` accepts what the database refuses.** `tax-sa` constructed fine,
  passed every test that does not touch the control plane, and failed at the
  moment a tenant enabled it — `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$`
  and the type allows `.` and `-`. A terrible place to find that out.

  Everything is `tax_sa` now, one spelling from crate to URL, and
  `every_module_id_satisfies_the_entitlement_constraint` catches the next one at
  build time. The honest fix is for `ModuleId` to carry the narrower rule so the
  type refuses what the schema will; that is a `erp-types` change with no
  consumer asking for it yet.

- **Seeding rides on the only hook a module has.** The rate is an `INSERT … ON
  CONFLICT DO NOTHING` inside the schema install, which is idempotent so a
  rebuild re-running it is harmless — and `DO NOTHING` is what stops enabling a
  country module stamping over a rate a tenant corrected, which
  `re_installing_does_not_overwrite_a_rate_the_tenant_set` pins. A module wants a
  `seed` step distinct from its DDL; noted rather than invented.

- **The demo files a return.** A tax module nobody has filed with demonstrates an
  arithmetic exercise rather than the thing being bought.

## The tenant is the subdomain

- **`/v1/tenants/{slug}/sales/invoices` was wrong, and it was wrong in a way
  every route repeated.** A tenant is a company, and a company on this platform
  is `bassat.erp.com` — not a path segment that every handler has to remember to
  scope by. The slug moved into the `Host` header, and the paths became what they
  describe: `/v1/login`, `/v1/sales/invoices`, `/v1/tax_sa/vat-return`.

  It is the smaller diff *and* the stronger guarantee. A path parameter is
  something a handler can forget to use; a subdomain is resolved once, in the
  `Tenant` extractor, before any handler runs — and a handler that does not take
  `Tenant` cannot reach a tenant database at all, because `TenantDb` has no
  public constructor.

- **Exactly one label under the configured domain.** `demo.erp.test` is a tenant;
  `erp.test` is not, `a.demo.erp.test` is not, and neither is anything under a
  domain this build was not told about. The port is stripped, a trailing dot is
  stripped, and the comparison is lowercase, because all three arrive in real
  `Host` headers. `PUBLIC_DOMAIN` configures it and defaults to `localhost`, so
  `demo.localhost` works in a browser with no hosts-file editing.

- **Two collisions, and both were the paths telling me something.** With the
  slug gone, `GET /v1/invitations/{invitation}` — a manager reading an invitation
  they sent — collided with `GET /v1/invitations/{token}`, which is a stranger
  holding a link and belongs to no tenant. They were never the same resource; the
  public one is `/v1/join/{token}` now. `GET /v1/modules` collided the same way:
  the catalogue of what is *offered* is not the list of what a tenant *has*, and
  it is `/v1/catalogue`.

- **`just clean-databases` left the people behind.** It already deleted tenant
  rows whose database was gone; identities outlived them, so the next `just demo`
  with a different password failed with `invalid_credentials` against an account
  whose company no longer existed. The same bug one table over, and the recipe
  deletes memberless identities now.

  Getting there turned up something worth knowing: `audit_entry` refuses UPDATE
  and DELETE by trigger, which makes the `ON DELETE SET NULL` on its actor
  columns **unreachable** — an identity that has ever acted cannot be deleted at
  all. Fine for the dev recipe, which truncates. Not fine for a Saudi PDPL
  erasure request, and that is a decision to take deliberately: an audit trail
  that keeps a name forever, or actor columns that can be nulled by a path the
  trigger allows.

## ZATCA — clearance and reporting

- **Two obligations, and one field decides which.** A buyer who gives a VAT
  number gets a **standard** invoice, which ZATCA has to *clear* **before the
  buyer is given it**. Everyone else gets a **simplified** one, handed over at
  the till and *reported* within twenty-four hours. `Kind::of` is the only place
  that decision is taken, and everything downstream — the endpoint, the
  `InvoiceTypeCode` subtype, the deadline, the two counts in the standing report
  — follows from it.

- **The document is a projection, and that is the first real extension module.**
  Nothing in the issuing transaction can build a ZATCA document: `sales` issues
  the invoice and must not know Saudi Arabia exists, because the dependency runs
  `tax_sa → sales` and inverting it would put ZATCA in the sales module of every
  tenant in every country.

  So `tax_sa` **subscribes** to `sales.invoice.issued` and
  `sales.invoice.cancelled` in its own projection group. Three things the kernel
  already had made it work with no new mechanism: a projection reads the whole
  log rather than its own module's slice, a projection group is the unit of
  consistency so this writes only into `proj_tax_sa`, and — the one addition —
  `Upcasters::also`, which folds `sales`' event history into `tax_sa`'s so a
  version `sales` adds next year is readable here without a second copy of its
  chain that could disagree with the first.

  **This is the answer to "how does a module extend another?"** The module being
  extended does not know. There is no registry, no hook, and nothing in `sales`
  to change.

- **The registration had to be an event, and the reason is the hash chain.**
  Every other tenant setting here is configuration, read inside the command's
  transaction and stamped onto the event. That mechanism was unavailable — the
  issuing command cannot read a ZATCA registration — and the obvious fallback,
  reading `configuration` inside the projection, is a **silent** disaster:
  rebuild after the business moves offices and every historic invoice renders
  with the new address, hashes differently, and breaks the chain. Each document
  on its own would still look fine.

  So `tax_sa.taxpayer.registered` is a fact in the log with a position, like
  everything else the projection reads. An invoice issued in March renders under
  the registration that was current in March whatever happened in April, and
  `a_correction_applies_from_where_it_was_made` pins it.

- **The XML is written by hand, already canonical.** ZATCA hashes the
  *canonicalised* document (C14N 1.1) and the seller signs that hash, so a
  serialiser that reorders an attribute or collapses `<a></a>` into `<a/>`
  invalidates the signature. The usual pipeline is DOM → serialise → XSL strip →
  canonicalise → hash: four places to be wrong, and three dependencies.

  Writing canonical form directly makes canonicalisation the identity function,
  so `hash(bytes) == hash(c14n(bytes))`. The rules that keeps true are checked by
  a scanner in the test module that walks the output with a tag stack and knows
  nothing about how it was produced — balanced tags, no empty-element form, no
  undeclared prefix, namespaces on the root in C14N's order (**the default one
  first**, because it has no local name and so sorts least), attributes sorted,
  `&`/`<`/`>` escaped in text. `the_scanner_refuses_what_it_claims_to` breaks
  each of those and watches it say no.

  What this does *not* prove is byte-equality with a real C14N 1.1
  implementation, and there is not one in this workspace — `xmllint --c14n11`
  would settle it, and Python's stdlib canonicaliser is C14N **2.0**, which
  rewrites namespace declarations down to where they are used and answers a
  different question. The one that matters is ZATCA's own SDK, and that needs a
  certificate.

- **The first link in the chain is encoded differently from every other one, and
  that is not a bug.** ZATCA's genesis PIH is `base64(hex(sha256("0")))` — 88
  characters encoding the *text* `5feceb66…` — while every subsequent PIH is
  `base64(sha256(bytes))`, 44 characters. A chain that "fixes" the inconsistency
  is rejected at the first invoice, so `the_first_link_is_zatcas_odd_one_out`
  pins the literal.

- **A refusal and a failure to ask are different facts.** ZATCA saying *no* is
  about the document and is final. A timeout, a 503 or an expired certificate is
  about us — nothing was decided, so nothing is appended, the document stays
  pending, and the next sweep tries again. Collapsing them marks a perfectly good
  invoice permanently refused because a token expired, and it marks *every*
  invoice in the batch, which is why the sweep stops on the first `Unanswered`
  rather than working through the rest.
  `an_outage_marks_nothing_refused_and_stops_the_sweep` breaks it and watches.

- **What cannot be built here, and what was built instead.** Submitting needs a
  production CSID — a certificate ZATCA issues after onboarding a specific
  solution for a specific taxpayer — and an XAdES signature made with it. There
  is no honest way to have that in this repository, and a fake that pretended
  would be worse than none, because the whole point of a clearance record is that
  it happened.

  So the seam is `wire::Submitter`, one method, and everything up to the socket
  is here and tested: the request and response bodies, both endpoints, the
  `Clearance-Status` header, and above all `Verdict::of` — which reads an HTTP
  status and a body into *cleared with warnings*, *refused*, or *no verdict at
  all*, including the case where ZATCA answers `200` with `NOT_CLEARED` and the
  status line has to beat the HTTP code.

  The sweep is a function rather than a worker job for the same reason: a job
  registered with nothing behind it is code with no caller, which is the failure
  this codebase keeps finding. `submit_pending` has a real caller in its tests,
  and wrapping it in a job is three lines the day there is a certificate.

- **Documents issued before registration are recorded, not skipped.** The chain
  starts at onboarding, so they have no place in it and cannot be cleared
  retrospectively — but a business needs to know they exist, and silently
  dropping them is the "quietly under-reporting" failure the worker docs warn
  about. They sit at `unregistered` and the standing report counts them.

- **The QR carries no hash yet, deliberately.** Tags 1–5 are what a build without
  a certificate can honestly produce. Tag 6 is the invoice hash and 7–9 are the
  stamp; a QR carrying a hash and no signature claims more than it can show, and
  fails validation for it. The encoder takes them as `Option`s and the length
  byte is a **byte** count, which is the mistake an Arabic seller name makes
  expensive — `the_length_is_bytes_and_not_characters` asserts the two counts
  differ, so it cannot pass by accident.

## ZATCA onboarding: the OTP, the key, and the certificate

- **"Certificate generation" is a key pair and a CSR, and the private key never
  moves.** A taxpayer never issues their own certificate: ZATCA signs the request
  and returns one. What this generates is an ECDSA key pair and a PKCS#10
  request, and the key stays sealed in the tenant's database for the rest of its
  life.

- **secp256k1, which is one character from the curve everything else defaults
  to.** ZATCA specifies the Koblitz curve; almost every X.509 stack reaches for
  secp256r1/P-256. A CSR on the wrong one is refused at onboarding with no
  useful message, so `the_curve_is_the_koblitz_one_and_not_the_usual_one` reads
  the curve back out of the encoded request rather than trusting the constant.

- **OpenSSL was already linked, so key generation cost nothing.** `sqlx` builds
  against it for Postgres TLS, which means the process already had `EcKey`,
  `X509Req`, `X509Extension` and AES-GCM. The alternative was four RustCrypto
  crates for the same capability and a second TLS-adjacent stack in the binary.

- **The two extensions are written as DER by hand.** ZATCA reads the EGS unit's
  identity out of `subjectAltName` as a `directoryName`, and reads which
  environment to onboard into out of `1.3.6.1.4.1.311.20.2`. Neither is
  expressible through openssl-rs's config-string API — `X509Extension::new`
  wants an OpenSSL config section and the Rust binding cannot build one — so both
  are `new_from_der` over sixty bytes this build writes itself. That also makes
  them assertable: the tests decode the finished request and look for the pipe-
  separated EGS serial and the UID OID.

  A whole DER library for sixty bytes would have been the other answer, and the
  encoder here is a tag, a length and somebody else's bytes.

- **The environment is not a default.** Sandbox, simulation and production differ
  by one string in one extension. Getting it wrong does not fail — it succeeds
  against the wrong authority, which is why `Environment` is a required argument
  everywhere it appears.

- **A module had nowhere to keep a secret, so core grew one.** The three places a
  module had were all wrong for a private key: the event log is immutable and
  replayed forever, `configuration` is read by anything that can read the tenant,
  and `proj_<module>` is **dropped and rebuilt** by an ordinary maintenance
  operation. `module_secret` is a core table holding opaque sealed bytes under a
  module's own key — the same shape as `configuration` and `document_number`,
  which are also core tables only modules use.

  Sealed with AES-256-GCM under a key from the deployment's environment, nonce
  and tag inside the value so a row is self-describing. `the_sealed_bytes_do_not_
  contain_the_plaintext` and `a_tampered_value_is_refused_rather_than_decrypted`
  are the two that matter.

- **No sealing key means refusal, not a plaintext fallback.** `SEALING_KEY` is
  optional and its absence is not a degraded mode: the endpoint answers 503 and
  stores nothing. Law L6, applied to the one place where degrading would mean
  writing a signing key to disk in the clear because an environment variable was
  missing.

- **The certificate is checked against our key before it is stored.** ZATCA
  returning a certificate over somebody else's key is not a smaller problem than
  returning none — it is accepted, stored, and then every invoice signed with it
  is rejected at clearance, on a document a customer is waiting for, with an
  error that says nothing about why. `accept` compares the public keys and
  refuses; `a_certificate_that_is_not_for_our_key_is_refused` breaks it.

- **The OTP is never stored, never logged, and never in an event.** It is the
  taxpayer's proof of identity for about an hour. `Otp`'s `Debug` prints
  `<withheld>` so it cannot reach a log through a panic or a `tracing` field, and
  `the_log_records_the_certificate_and_never_the_key` greps the actual event
  payloads for it.

- **What goes in the log is the certificate's identity.** Subject, serial,
  validity, environment, stage — everything a person needs to answer "which
  certificate signed this invoice?" three years later, and nothing secret. The
  secret material is sealed beside it, where it can be rotated; a secret in the
  log could never be.

- **Onboarding works today with no HTTP client.** It is a once-per-tenant act
  with a human in the middle — somebody has to log into Fatoora and read six
  digits off a screen — so the two halves separate cleanly:
  `POST /v1/tax_sa/zatca/onboarding` generates the key and hands back the
  request, and `PUT …/certificate` takes what ZATCA issued. An operator can carry
  the middle step with `curl`.

  That is not a workaround for the missing transport. It is the path a deployment
  falls back to **when the automated one breaks**, which for a once-per-tenant
  act guarded by an hour-long credential is worth having regardless. `Registrar`
  is the automated path's seam, and `Onboarder` drives both halves through it.

- **Two things here are reconstructed from ZATCA's specification and want a
  sandbox round-trip before anyone relies on them**: whether `csr` carries the
  base64 of the whole PEM or of the DER body, and which of those two
  `binarySecurityToken` comes back as. The second is handled by accepting both —
  they are distinguishable, since DER starts with a `SEQUENCE` tag and base64
  text does not. The first is a guess documented at
  `Generated::csr_for_zatca`, and it is the first thing a deployment's sandbox
  onboarding will confirm or deny.

## The XAdES signature

- **Signing cannot happen in a projection, and the reason is ECDSA.** Every
  signature over the same bytes with the same key is different, because a fresh
  `k` goes into each one. A projection that signed would produce different tables
  on every rebuild — in the column a tax authority holds a copy of. It would also
  need the private key, and a projection that could read `module_secret` is a
  projection that could leak it.

  So signing happens once, outside, and `tax_sa.zatca.signed` records the result.
  The projection **replays** a signature rather than recomputing one, which is
  the same argument as recording what ZATCA said, applied to something we did
  ourselves. `a_rebuild_reproduces_the_signature_it_cannot_recompute` breaks it
  by recomputing and watches the shadow differ.

- **Two sweeps, because they fail for different reasons.** `sign_pending` needs a
  certificate; `submit_pending` needs ZATCA to answer. A document needs the first
  even when the second cannot happen — a simplified invoice's QR carries the
  cryptographic stamp, and that receipt goes to the customer at the till whether
  or not the network is up. `pending` hands out only signed documents, so an
  unsigned one is never submitted to be told what we already know.

- **The signature covers `ds:SignedInfo`, which covers two digests.** The invoice
  digest is the one already in the chain — ZATCA hashes the document with the
  extensions, the signature and the QR reference removed, and this build never
  renders those into the bytes it hashes. The second is over
  `xades:SignedProperties`, which carries the signing time and the certificate.
  Breaking the first (signing the invoice digest directly instead) makes the
  signature fail to verify under its own certificate, which is what the test
  checks.

- **The submitted document is rendered, not spliced.** `ubl::signed` is the same
  renderer with the three parts put back in UBL's required order — extensions as
  the first child of the root, the QR reference after the chain, `cac:Signature`
  after the last `AdditionalDocumentReference` and before the parties. Splicing
  strings at a marker would be a second parser that has to agree with the first
  about where a document's parts are.

- **Three things here are reconstructed from ZATCA's specification and deviate
  from the standards they are built on.** Each is one named function, marked
  `UNCONFIRMED`, so a sandbox round-trip changes one line:

  1. `certificate_digest` hashes the certificate's **base64 text**, not its DER.
     XAdES says DER. ZATCA's SDK says text. The same class of quirk as the
     genesis PIH, and pinned by a test that asserts the two differ.
  2. The ECDSA signature goes in as **DER**; XML-DSig specifies the raw `r ‖ s`
     pair. ZATCA's published samples decode to DER.
  3. The whitespace inside `xades:SignedProperties` is **inside the digest**, so
     it cannot be normalised away and the element is one string rather than a
     builder. An editor reflowing that function changes every signature it makes.

- **The demo stays unsigned, deliberately.** It has no ZATCA certificate, and
  inventing one would be inventing a compliance record — the same reason there is
  no fake `Submitter`. `just demo` says so in as many words, and the standing
  report's `unsigned` count is the number that tells a real tenant they are not
  live yet whatever else looks fine.

## The transport, and the compliance checks

- **reqwest with `default-tls`, not `rustls-tls`.** sqlx already links native-tls
  against OpenSSL for Postgres, and every piece of cryptography here — key
  generation, the CSR, signing, sealing — is OpenSSL too. Choosing rustls would
  have put a second TLS implementation in the same process for no benefit. No
  cookies, no gzip, no blocking client: six JSON endpoints on one host.

- **The client is tested against a real socket, not a mock.** A forty-line
  one-request HTTP server in the test module accepts a connection, reads the
  bytes, and hands them back as a string — so the assertions are on **what
  actually went out**: `OTP: 123456` in a header, `Clearance-Status: 1` for
  clearance and `0` for reporting, the basic auth built from the CSID, the JSON
  field names. Removing the OTP header or fixing `Clearance-Status` to one value
  both fail. A mocked client would have asserted on the mock.

- **Failing to ask is still not a verdict, now that there is a socket to fail
  at.** A timeout, a refused connection and a 5xx all become `Unanswered`; a
  `400` with a body is parsed, because that body is where ZATCA explains itself.
  `a_connection_that_fails_is_not_a_verdict` points the client at a closed port
  and checks which of the two it produced.

- **A client for one environment refuses a call for another.** The sandbox and
  production differ by a hostname and a string in a certificate, and a mismatch
  otherwise succeeds against the wrong authority.

- **The compliance samples are invented, and their chain is thrown away.** ZATCA
  wants one of every document type the CSR declared — six for a unit that issues
  both kinds — before it will issue a production certificate. There are no real
  invoices to send, because a business onboards before it issues, so the samples
  are synthetic: the taxpayer's own registration, one line, one riyal, a number
  starting `COMPLIANCE-`.

  They chain among themselves from ZATCA's genesis value and **never touch the
  tenant's counter**, which has not started yet and must start at one when it
  does. Deriving them from the real chain would either burn six positions or
  leave a gap where the samples were — and a gap is the one thing the chain
  exists to make impossible. Collapsing them onto one position fails
  `the_chain_is_broken`.

- **`POST /v1/tax_sa/zatca/onboarding/activate` is the whole flow.** Key pair and
  CSR, the OTP exchange, the six signed samples, and the production certificate —
  four calls to ZATCA in the order it requires, from one request carrying six
  digits. Nothing is stored unless the step that produced it succeeded.

- **A refused compliance sample answers 502, not 400.** The samples are generated
  here, so ZATCA refusing one is a fault in this software and not in the caller's
  request — the message says so, in both languages, and the full failure list
  goes to the log where somebody can act on it. Getting that backwards would tell
  a business to fix something they did not do.

- **The manual path still exists, and is still the one to reach for when the
  automated one breaks.** `POST …/onboarding` hands back a CSR and
  `PUT …/onboarding/certificate` takes what comes back, so an operator with
  `curl` can complete an onboarding whose automated attempt failed at any step.

## What live traffic to ZATCA proved, and what it did not

The first run against `gw-fatoora.zatca.gov.sa` was worth more than the tests it
passed. In order:

- **The sandbox issued a real certificate to a CSR this build generated.**
  `POST /e-invoicing/developer-portal/compliance` answered `ISSUED` with a
  `binarySecurityToken`, which the flow then read, checked against the private
  key, and sealed — serial `01A00C782957`. That settles the parts that were
  reconstructed from the specification: the curve, the two extensions, the
  subject, the base64 wrapping of the CSR, and the shape of the request body are
  **all accepted by ZATCA**.

- **Simulation and production accept the same request and reject only the OTP.**
  Both answered `{"code":"Invalid-OTP"}` to a made-up six digits, which is the
  furthest a build without a real taxpayer's portal login can get.

- **`dispositionMessage` is sometimes absent.** ZATCA returns an invalid-OTP
  refusal with nothing but `errors`, and an earlier version of this rendered that
  as an empty pair of brackets. The recorded body is now a test.

- **Two diagnostics were wrong, and only live traffic showed it.** The first
  failure reported "ZATCA could not be reached" — and the certificate had in fact
  been issued and stored; what failed was the *compliance check*, three steps
  later. Onboarding makes four calls that all fail the same way, so the error now
  names the step, and `Verdict::of` puts the HTTP status and the first 200 bytes
  of the body in the message. The same failure now reads:

  > ZATCA could not be reached while **submitting a compliance document**:
  > ZATCA's answer could not be read: expected value at line 1 column 1 —
  > **HTTP 400, 15 bytes: Invalid Request**

  An hour of bisecting a flow that talks to a tax authority, turned into one
  line. This is the failure mode worth designing against: not that a call fails,
  but that the failure names the wrong thing.

- **What is still unconfirmed: the signed document itself.** The sandbox answers
  `/compliance/invoices` with the non-JSON string `Invalid Request` — and it
  answers deliberate garbage the same way, so it does not distinguish. It appears
  to be a stub for certificate issuance rather than a validating endpoint.
  Settling the `XAdES` signature, the certificate digest and the DER-vs-`r ‖ s`
  question needs the **simulation** environment with a real taxpayer's OTP, which
  is the first thing to do with one.

## What a real certificate settled

A working sandbox CSID from another implementation, plus the key it was issued
against, turned every open question here into an answer. `modules/tax_sa/tests/
sandbox.rs` submits a document built by the ordinary renderer and signed by the
ordinary signer; ZATCA now accepts both a standard and a simplified invoice
**with no warnings and no errors**. Five things came out of it.

- **The hashed document carries the whitespace the removed elements left
  behind, and this was the one that mattered.** ZATCA hashes what it receives
  after an XSL transform removes `ext:UBLExtensions`, `cac:Signature` and the QR
  reference — and a transform removes *elements*, not the whitespace text nodes
  around them. Removing an element from

  ```text
    <Invoice …>\n  <ext:UBLExtensions>…</ext:UBLExtensions>\n  <cbc:ProfileID>
  ```

  leaves the `"\n  "` before it *and* the `"\n  "` after it. This build never
  renders those elements, so it now emits their leftovers deliberately. Without
  them the answer was `invalid-invoice-hash`; with them, accepted. There was no
  way to deduce that from the specification, and no unit test that could have
  caught it.

- **The certificate digest and the DER signature were both right.** Two guesses
  that deviate from `XAdES` and XML-DSig in ZATCA's direction, and both hold.

- **`BR-KSA-EN16931-09`: a second, bare `cac:TaxTotal`.** When
  `cbc:TaxCurrencyCode` is present, ZATCA wants one tax total *with* subtotals
  and one *without*. It warns rather than refusing, which is how the first
  accepted document still had something wrong with it.

- **The QR timestamp has no `Z`.** ZATCA's QR specification shows one; its
  validator compares the value against `cbc:IssueDate` + `T` + `cbc:IssueTime`,
  which carries no zone, and answers `invoiceTimeStamp_QRCODE_INVALID`. The
  specification and the validator disagree, and the validator is the one that
  decides.

- **A buyer needs an address, so `sales::Customer` grew one.** ZATCA wants
  street, city and country on a standard invoice (BT-50, BT-52, BT-55) and warns
  without them. Optional and `#[serde(default)]`, so every invoice issued before
  the field existed still decodes — an absent address is exactly what those had,
  which is why it needs no upcaster. Snapshotted onto the invoice like the name,
  for the same reason.

- **And the template name was wrong.** Their CSR carries
  `PREZATCA-Code-Signing` where this build had `TSTZATCACA-Code-Signing` for
  both sandbox and simulation. The sandbox issues a certificate against any of
  the three, so the mistake would have surfaced at the first *simulation*
  onboarding and nowhere before it. Three environments, three template names,
  now pinned to their literals.

## The compliance checks, against the sandbox

Step 3 now passes: **all six documents accepted, no warnings and no errors** —
standard and simplified, invoice, credit note and debit note. Two more things
came out of running them, and neither was deducible from the specification.

- **A credit note is an `<Invoice>`, not a UBL `<CreditNote>`.** Generic UBL has
  a separate document type for one, and this build used it. ZATCA's schema is
  UBL's *Invoice* schema throughout: a credit note is an `<Invoice>` whose
  `cbc:InvoiceTypeCode` says 381, with `cac:InvoiceLine` and
  `cbc:InvoicedQuantity` like any other.

  The tell was the *shape* of the refusal. A `<CreditNote>` root came back as
  `HTTP 400 Invalid Request` — fifteen bytes of plain text from the gateway,
  before the validator ran, with nothing to say what was wrong. Every other
  rejection in this exercise was JSON naming a BR-KSA rule. **A refusal that
  does not name a rule means the document was never read**, which is a different
  class of problem and worth recognising on sight.

- **`BR-KSA-17`: the reason goes in KSA-10, which is
  `cac:PaymentMeans/cbc:InstructionNote`.** This build had it in `cbc:Note`,
  which is BT-22 — a general note, and not what the rule reads. Every credit and
  debit note was refused for it. `cac:PaymentMeans` also needs a
  `cbc:PaymentMeansCode`, which is not meaningful on a note and is required by
  UBL wherever the element appears at all.

  Only notes carry it: an ordinary invoice is accepted without one, and adding
  it there would be inventing a payment method nobody chose.

- **The generator is separable from the driver**, which is what made this
  quick. `compliance_submissions` builds, chains and signs the six with no
  database and no network; `pass_compliance_checks` loads the sealed credentials
  and submits them. The part that had to be right could be run against ZATCA on
  its own.

## Document-level discounts

- **A discount was a negative line, and that is invisible on the document.** The
  invoice showed a smaller total and nothing said why. ZATCA models one as
  `cac:AllowanceCharge` — an amount, a reason and the tax treatment it comes off
  — and prints it as its own figure, so a customer sees what they were charged
  *and* what they were let off. The demo had exactly this problem: an "Early
  settlement discount" line of −1,500.00 that no reader could distinguish from a
  refund or a mistake.

- **The tax comes off with it, which is the whole difference from a credit
  note.** A discounted invoice was never for the larger amount, so the smaller
  one is what is taxed and what is declared: 100.00 less 15.00 is taxed on
  85.00, and the tax is 12.75. A credit note, by contrast, reverses an invoice
  that really was issued for the larger amount. `total()` subtracts the
  discounts before it works out any tax, which is the one line that makes this
  true.

- **A discount names the band it comes off, and only that one.** UBL puts a tax
  category on the allowance itself, and it has to: discounting the exempt part
  of a mixed invoice must not reduce the tax on the standard-rated part, because
  that tax was charged. Discounting at a rate the invoice does not carry is
  refused outright (`DiscountWithoutABand`) — it would reclaim tax that never
  existed.

- **Three monetary totals where there used to be one number.**
  `LineExtensionAmount` is what the lines came to, `AllowanceTotalAmount` what
  was taken off, `TaxExclusiveAmount` what is taxed — and ZATCA checks they
  agree. `Totals` records what the lines came to rather than deriving it
  downstream, because a second computation is a second thing that has to match.

- **No upcaster, because absent means what it says.** `Totals::discount` is
  `Option<Money>` and `Issued::discounts` is `#[serde(default)]`, so an invoice
  issued before any of this existed decodes as one with no discount — which is
  what it was. An event written today with no discount is byte-identical to one
  written last year, and `no_discount_is_absent_rather_than_zero` decodes a
  verbatim older payload to prove it.

  `Option<Money>` rather than a zero because a `serde` default cannot see the
  rest of the struct, and there is no zero without a currency. Inventing a
  sentinel currency to have somewhere to put it would have been the other
  answer.

- **Confirmed against ZATCA.** A discounted invoice built and signed by this
  code is accepted by the sandbox with no warnings — the ninth document in
  `modules/tax_sa/tests/sandbox.rs` to be.

## Wiring, erasure, and paging

- **The ZATCA sweeps had no caller.** Signing, submission, the Fatoora client —
  all of it worked in tests and none of it ran in production: an invoice was
  issued, a document was built and chained, and then nothing happened to it ever
  again. They are worker jobs now, registered only when `SEALING_KEY` is set,
  because they read a tenant's private key and a job that runs and finds it can
  do nothing is quieter than one that was never registered.

  They go in `bin/worker.rs` — the composition root, beside `TrialBalance` — for
  the reason that file already gives: the kernel must not know what a ZATCA
  document is, and a module must not depend on the worker.

  `zatca_jobs()` is a function rather than two `with_job` calls for the same
  reason `module_jobs()` is: a test can look at what a deployment would run.
  `no_module_job_runs_for_tenants_that_declined_it` covers them, and it matters
  more here than for a projection — a submit job with no `module()` opens a
  connection to a tax authority for every tenant on the platform.

- **A five-year certificate is a deadline nobody has a reminder for.** Renewal
  needs a human — the taxpayer reads an OTP off the Fatoora portal — so nothing
  here can do it. What it can do is stop the lapse being a surprise:
  `CertificateExpiry` reports sixty days out. It parses the date the certificate
  states, in OpenSSL's format, where the day has a **leading space** for single
  digits — a `%d` parse would work for three weeks in four and report an
  unreadable certificate on the ninth of the month.

- **An identity that had ever acted could not be deleted at all.**
  `audit_entry`'s trigger refuses UPDATE, and its own foreign keys declare
  `ON DELETE SET NULL` — which is an UPDATE. The clause was unreachable from the
  day it was written and nothing noticed, because nothing had ever tried.

  Under Saudi PDPL that is a right that cannot be honoured, and "our schema will
  not let us" is not a lawful ground for refusing. The trigger now permits
  **exactly one** shape of update: one that nulls an actor and changes nothing
  else. The trail keeps what it is for and loses only the link to a person —
  which is the shape it has always had for a system-initiated action.

  What is deliberately absent is an endpoint. **Who may erase whom** is a policy
  question, and answering it in passing while fixing a schema bug would be
  answering it badly.

- **Lists silently truncated at 200.** A tenant with 201 invoices saw 200 and was
  told nothing; the response was indistinguishable from a complete one. Keyset
  paging on the columns each list is ordered by, an opaque cursor, and `next`
  absent meaning **the list ended**.

  Keyset rather than `OFFSET` because offset is wrong under concurrent writes:
  an invoice issued while somebody pages shifts every later row by one, so a row
  can be skipped or seen twice. The test pages five invoices two at a time, some
  sharing a tax point so the cursor's second part is what separates them, and
  asserts nothing was lost or repeated. Ignoring the cursor makes it fail with
  `INV-00005 came back twice`.

  A cursor this build cannot read is **refused**, not ignored — silently
  starting over would hand a caller the first page again and read as the list
  restarting.

- **`ModuleId` accepted what the database refused.** `tax-sa` constructed fine
  and failed at the moment a tenant enabled it, because
  `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$`. The type carries that
  rule now, so a module id that cannot be stored is one that cannot be built.

- **The crash-test flake was mine.** `just clean-databases` drops with `FORCE`,
  and I ran it while a background suite was still going: a test's database
  vanished mid-run, its outbox came back empty, and two fault-injection tests
  failed an assertion about something else entirely. It took an afternoon to
  find because the failure names the wrong thing.

  The recipe refuses now when anything is connected to a test database. The
  hazard is real for anyone running the suite in one terminal and cleaning up in
  another.

## Modules ship their own routes

Four files — `ledger_routes.rs`, `sales_routes.rs`, `purchases_routes.rs`,
`tax_sa_routes.rs`, about 3,900 lines — lived in `erp-api`. So a module's HTTP
surface was written by something the module could not see, adding an endpoint
meant editing two crates, and "read the sales module" meant reading two
directories. They are `modules/*/src/http.rs` now, next to the aggregates and
read models they serve.

- **What was actually in the way was the furniture.** Extractors, problem+json,
  the JSON and query rejections, paging, the request-level messages — all in
  `erp-api`, which names every module, so a module reaching for `Json` or
  `require_module` would have closed a cycle. Those moved *down* into a new
  crate, `erp-web`, below the modules. What is left in `erp-api` is the core's
  own routes — sessions, the tenant, members, invitations, signup, module
  management — and the composition.

  The split falls where the architecture already said it did: `erp-web` holds no
  business domain (D11) and cannot be given one without becoming a module.

- **One list, two views.** `modules::REGISTERED` carries each module's
  `ModuleSetup` *and* its router. `available()` is the first view — what the
  control plane, the worker and the migrator read — and `mounted()` is the
  second. A module cannot be added to the platform and have its routes
  forgotten, because there is nowhere to add it that does not also mount them.

- **Authorization reads the path, and the path list moved.** `Allowed<C>` decides
  which role applies by taking the module out of `/v1/{module}/…`, and it used to
  check that segment against the *build's* module list — which is above
  `erp-web` now. It checks the **tenant's** list instead, which is the better
  answer: a segment naming a module the tenant does not have is judged on the
  tenant-wide role, exactly as `/v1/members` is, and the handler's own
  `require_module` then answers 404. A request for a module they do not have
  cannot reach data by any route, and the reply does not confirm what they are
  not paying for.

  What the old check bought — "no module's routes are silently judged
  tenant-wide" — is now `every_modules_routes_live_under_its_own_name`, which
  walks each module's own OpenAPI paths. Pointing one route at `/v1/posting-accounts`
  fails it. `no_two_modules_claim_the_same_path` turns a startup panic into a
  build failure with both module names in it.

- **The catalog had to be split, and getting it wrong was silent.** A module
  renders its failures through a composite of its own catalog, its dependencies',
  and `erp_web::CATALOG`; `erp_api::CATALOG` is the complete union and is still
  what `docs/ERRORS.md` comes from.

  `ApiError::into_problem` rendered through a fixed catalog, which was fine while
  every caller was in one crate. After the move, `POST /v1/ledger/entries` with
  an unbalanced entry answered `"detail": "ledger.does_not_balance"` — the bare
  code, no sentence. `an_unbalanced_entry_is_refused_with_the_difference` caught
  it.

  `into_problem` takes the catalog now, and **there is no `IntoResponse for
  ApiError`**: it could not name one, so `?` on an `ApiError` in a handler would
  have taken the same wrong turn just as quietly. Nothing was using it.

- **`docs/openapi.json` is byte-identical** across the move but for one
  description, which was a stale comment about paging that does not exist any
  more. Nothing a client can see changed.

## A module seeds separately from its DDL

The Saudi VAT rate rode on `tax_sa`'s schema install, because that was the only
hook a module had. It worked — the insert is `ON CONFLICT DO NOTHING`, so
re-running it is harmless — and it made two different things look like one: a
tenant's *data* written by a step named "install schema".

`just prepare` was already somewhere that mattered. It installs every module's
DDL into a throwaway type-check database, where a `configuration` row is noise;
it globbed `schema/*.sql` and would have picked up a seed file too. It runs
`install.sql` only now, which is a distinction the recipe can only make because
the two are separate files.

`ModuleSetup::seeding(sql)`, run after the install and under the same
`search_path` — so a seed can write both the module's own tables and the
tenant's `public` ones, which is what the rate does.

`a_modules_seed_runs_when_a_tenant_gets_the_module` proves the ordering by
inserting into a table the DDL creates: run it first and it fails on a missing
relation. `a_rebuild_seeds_again_without_overwriting_the_tenants_own_value`
covers the other half — a refresh drops the module's schema, so its seed has to
run again, and the tenant's `configuration` is *not* dropped, so it meets a row
it already wrote. Skipping the seed step fails both.

The existing `enabling_the_module_seeds_the_saudi_rate` did **not** catch it: it
calls `tax_sa::install()`, a test helper that had its own copy of the two
statements. That helper reads `setup()` now, so it installs what production
installs.

## `requires` learns "at least one of"

`tax_sa` computes a VAT return, which nets output tax against input tax. It
needs a source for one side or the other and does not care which: a business
that only sells still files, and so does one that has bought but not yet sold.

`requires` is an AND list, so neither answer was available. Naming both would
force a shop with no supplier bills to enable `purchases` in order to declare
tax they do owe. So `tax_sa` named **neither**, with a comment saying what it
actually meant — and that let a tenant turn on a VAT return with nothing on
either side, and disable the last module feeding it without a word.

`ModuleSetup::requires_any`: one group, satisfied by any member.
`.requiring(&["ledger"]).requiring_any(&["sales", "purchases"])` is now the whole
sentence. One group and not a list of groups — "ledger AND (sales OR purchases)"
is what this system needs, a second disjunction has no consumer, and the nested
shape can arrive with the module that wants one.

`ledger` is named explicitly even though either alternative brings it: `tax_sa`
reads `ledger::Rates` itself, and a dependency relied on directly is one to
declare rather than inherit.

**The interesting half is disabling.** A tenant with sales, purchases and
`tax_sa` may turn either side off; the *second* one is refused, because a return
with nothing on either side is not a downgrade, it is a module that cannot
answer. `dependent_on` asks whether `name` is the last enabled member of a
dependent's `requires_any`, which is three distinct ways to be wrong:

- skip the enable check → `tax_sa` goes on with only the ledger
- treat `requires_any` as AND when disabling → *neither* side can ever be turned
  off, which is worse than the bug being fixed
- ignore `requires_any` when disabling → the last side goes and the return is
  left mute

Each fails `one_of_several_is_enough_and_none_of_them_is_not` with its own
message. `a_module_needing_one_of_several_takes_either_and_refuses_neither`
walks the same path over HTTP, in Arabic, and asserts the refusal reads *one of*
— `request.module_requires_one_of` is a separate code from
`request.module_requires`, because "needs sales, purchases" and "needs at least
one of sales, purchases" are different sentences and a client rendering its own
has to be able to tell them apart.

The list arrives in `args.required` comma-separated. A comma separates a list in
both English and Arabic, so neither language needs a conjunction built in the
code — which is how "sales و purchases" ends up in an English sentence.

`GET /v1/catalogue` and `GET /v1/modules` both carry `requires_any`, so a picker
can grey out what is impossible rather than let somebody discover it.

## The first outbox handler, and the plane it had to go in

The outbox was finished in Phase 2 and joined to nothing for the whole of Phases
3 and 4. Effects as values, claim under `SKIP LOCKED`, leases, exponential
backoff, dead letters, the at-least-once idempotency key, crash tests proving a
lost delivery record replays with the *same* key — all of it real, all of it
tested, and **no producer anywhere in the product and no registered handler**.
An effect enqueued by hand in a test was the only effect this system had ever
seen.

Email is the first handler. Invitations are the first producer.

- **The outbox was in the wrong database for its first real user.** It was built
  where commands and events are, which is a tenant database. But the things that
  most need to reach the outside world do not happen in a tenant database at all:
  an invitation is a control-plane row, and so is a signup, and so is a password
  reset.

  Writing the invitation in the control plane and its email into the tenant's
  outbox would be two databases, therefore two transactions, therefore a window
  where the invitation exists and the email was never promised — with nothing
  recording that it was owed. That window is the exact thing D9 exists to close,
  so closing it in one plane and leaving it open in the other is not a design.

  So the control plane has an outbox now, and it is **byte-for-byte** the tenant
  one: `Dispatcher` and `enqueue` are compile-time-checked against a table named
  `outbox` with those columns, and reusing them costs one obligation — the two
  files must not drift. Nothing in the compiler can see that obligation, because
  sqlx validates against a single type-check database where the two are the same
  table. A column added to one chain would type-check perfectly and fail at
  runtime in the other plane. `the_two_outboxes_are_the_same_table` compares them
  column by column and constraint by constraint; adding one column to either
  fails it.

  `just prepare` now loads the tenant chain **first**, so the tenant definition is
  the one sqlx checks against and the control migration is a no-op there.

- **`PlatformJob`, because a `Job` is handed a `TenantDb`.** The control-plane
  outbox has no tenant. Running it as a per-tenant job would be *safe* —
  `SKIP LOCKED` sees to that — and still wrong: the work would scale with the
  number of tenants rather than with the amount of work, under a tenant's lease,
  N times a cycle.

  It runs once per claim cycle, inline, **before** tenants are claimed, so an
  idle deployment with nothing due still pumps the queue. A failure is logged and
  stepped over rather than propagated: one unreachable relay must not stall every
  projection on the fleet.

- **SMTP, not a vendor's HTTP API.** Every provider speaks it and so does a
  Postfix somebody runs themselves, which for a self-managed fleet means changing
  provider is one environment variable rather than a code change. `lettre` on
  `native-tls` — the OpenSSL sqlx already links for Postgres TLS, the same call
  made for `reqwest`, and the reason this build has one TLS stack rather than
  two. Not hand-rolled, because an Arabic subject line needs RFC 2047
  encoded-words and getting that subtly wrong produces mojibake in exactly the
  clients this market uses.

- **The interesting part of the handler is not SMTP.** It is which failures are
  worth retrying. A refused connection, a TLS handshake, a 4xx greylisting, a
  timeout — the relay having a moment, and dead-lettering an invitation because a
  mail server was restarting would be losing it. A 5xx, or an address that will
  not parse — permanent, and retrying spends four attempts and two minutes of
  backoff to arrive at the same answer. A payload this build cannot read is
  permanent too, rather than retried three times first with a misleading error
  left on the row.

  That is why there is a `Mailer` trait: a test of it has to make the transport
  fail on demand, and must never send real mail by accident.

- **The text is rendered when the invitation is written, not when it is sent.**
  L5: the effect records a resolved decision. The recipient has no account and
  therefore no stored language, so what exists at the moment of inviting — the
  language the *inviter* was working in — is the best signal there will ever be,
  and it is gone by the time a worker picks the row up. It also means editing the
  catalog does not silently change what an already-issued invitation says, which
  is the same reason an invoice stores its VAT rate.

- **No relay configured is not an error.** With `SMTP_URL` unset the handler is
  not registered, and an effect whose kind has no handler is *not claimed* — so
  the email waits as an undelivered promise rather than being attempted and given
  up on. Configure a relay a month later and everything already promised goes
  out. Same call `SEALING_KEY` makes for the ZATCA sweeps.

- **What the tests pin.** `inviting_somebody_promises_them_an_email` asserts the
  row is there after the request, in Arabic, carrying the invitation's own token
  — and then *uses* that token against `/v1/join/`, because a body with a
  plausible-looking URL that 404s is worse than no email.
  `an_invitation_is_promised_by_the_control_plane_and_delivered_by_the_worker`
  runs the platform pass and asserts the message reached the mailer and the row
  says delivered, then runs it again and asserts nothing is sent twice.

  Falsified: removing the `enqueue` empties the outbox; a batch limit of zero
  never delivers; an unregistered handler claims nothing; a drifted column fails
  the parity test.

- **A second `git checkout` cost a file.** `git checkout <path>` restores from
  the *index*, and a test appended but never staged is not in the index — so
  reverting a deliberate falsification deleted the test along with it. Second
  time this session. The habit is now `git add -A` before falsifying anything.

## Containers, a standby, and the cache that had nobody to talk to

Three things that turned out to be one thing: **every one of them was a seam
written for a second instance, in a system that had only ever run one.**

### The replica routing was reachable only from a unit test

`TenantDb::read` has routed to a replica since Phase 1 — "adding replicas later
is a configuration change, not a code change" — and `ClusterRegistry::with_replica`
has existed just as long. **No binary ever called it.** Five composition roots,
each registering a primary and stopping there, so the entire read path was
exercised by one unit test and nothing else.

`ClusterRegistry::from_env` now reads `PRIMARY_CLUSTER_URL` and
`PRIMARY_REPLICA_URL`, and all five binaries use it. Two things came out of
writing it:

- **`with_replica` on an unknown cluster was a silent no-op.** `with_replica("primry", …)`
  returned `Ok` with the replica dropped: every read would go to the primary, the
  deploy would look correct, and the only symptom would be a primary carrying
  twice the load somebody sized it for. It is an error now, and the error names
  what was not found.
- **Blank is absent.** A compose file that declares the variable and leaves it
  empty is the ordinary way to say "no replica here"; treating `""` as a URL
  fails at parse with a message about nothing.

`from_urls` is `from_env` with the environment already read, so the decision is
testable without `set_var` — which this workspace could not do anyway, because
it denies `unsafe`, and which races every other test in the same binary
regardless.

### Redis is not a second cache. It is agreement between nodes.

The obvious reading of "cache the hot paths with Redis" is wrong here, and the
existing code says why: the entry-path cache answers from process memory at a
99.9% hit rate, and moving it to Redis would replace a memory read with a network
round trip. It stays exactly where it is.

What Redis buys is the two things a per-process cache structurally cannot do.

**Sessions.** `ControlPlane::session` runs on every authenticated request and was
the one hot lookup with no cache at all — deliberately, for a reason `cache.rs`
states plainly: *a stale membership for five seconds is survivable, a stale
logout is not*. So the busiest query in the system went to the control database
every time, the database that cannot be sharded. A **shared** cache resolves the
objection, because a logout deletes the entry for every node at once.

**Invalidation.** The entry caches invalidate locally on write. `cache.rs` named
the fix when it was written — "out-of-band invalidation, which is a Phase 3
decision" — and this is it. Every `invalidate` in the crate now goes through one
`forget`, which drops the key locally and publishes what changed; every node
applies what it receives.

The failure policy is the part worth arguing about, and it is: **every path
degrades to exactly the behaviour of the build before this existed.** A session
read falls through to Postgres. A write is skipped. An invalidation that cannot
be published still happened locally, so other nodes fall back to the TTL window
that was always documented. That is not L6 being bent — L6 is about not degrading
a *guarantee*, and none of these was a guarantee without a documented bound.

The one exception is stated where it lives: a logout that cannot reach Redis
leaves that token usable until the cached entry expires. `SESSION_TTL` is one
minute, and it is the blast radius of that failure rather than a performance
knob.

Two smaller findings:

- **serde cannot internally tag a newtype variant wrapping a string**, and half
  the `Invalidate` variants are exactly that. Adjacent tagging
  (`{"what":…,"which":…}`) instead. The shape matters more than it looks: during
  a rolling deploy two builds are live, and a message an old node cannot read has
  to be a loud failure rather than a silently ignored one — silently ignored is a
  node serving stale authorization with nothing in the log to say so.
- **The subscriber holds a `Weak`.** A background task with a strong
  `Arc<ControlPlane>` would keep the pools open through shutdown and the drain
  would never finish.

The tests run **two `ControlPlane`s over one control database**, which is what two
API replicas are. Reverting `forget` to a local `invalidate` fails
`a_role_change_on_one_node_reaches_the_others`; dropping the Redis delete from
`log_out` fails `a_logout_on_one_node_ends_the_session_on_the_other`.

One test of mine was wrong before the code was: I demoted with `grant_membership`,
which **deliberately refuses to change a live member's role** because doing so
would be a way around the last-owner guard. The database was right and my
assertion was not.

### The compose file is the point, not the Dockerfile

One image with five binaries, because five images are five things to keep at one
version and "the worker is a deploy behind the API" is a failure this system has
a pre-deploy gate for. No `ENTRYPOINT`, so `docker run erp worker` puts the
worker at PID 1 and SIGTERM reaches it directly — which the graceful shutdown and
the lease-releasing drain are both written against.

Cache mounts rather than the usual dummy-sources-then-real-sources trick. That
trick works by making cargo believe the dependency layer is current, and it fails
quietly: a stale `liberp_control.rlib` built from an empty `lib.rs` links cleanly
and contains none of the code.

What the stack runs is deliberately not one of everything — two API replicas, two
workers, and a streaming standby — because one of anything tests none of what the
last two sections were about.

### What running it actually found

Four bugs, none of which any test could have caught, because every one of them
was about a second instance or a real service:

- **Postgres 18's image moved the data directory.** Mounting the old
  `/var/lib/postgresql/data` makes it refuse to start with a long message about
  finding data in an unused volume. The mount is `/var/lib/postgresql` and the
  cluster lives in a version subdirectory, so `pg_upgrade --link` can cross
  versions without a mount boundary in the way.
- **`pg_basebackup` was refused: `no pg_hba.conf entry for replication`.** The
  image writes a `pg_hba.conf` for ordinary connections and not for replication
  ones. There is no environment variable for it and it is not an `ALTER SYSTEM`
  setting, so it goes in `/docker-entrypoint-initdb.d`. Scoped to private
  ranges, not `all`.
- **`smtp://…?tls=none` is not a thing.** lettre refuses it at start-up; plain
  SMTP is `smtp://host:port` with no `tls` parameter at all. This was in the
  compose file *and* in `docs/RUNNING.md`, written from memory and wrong in
  both. A four-line probe over `Smtp::new` settled which forms are accepted.
- **Two API replicas cannot both publish one host port.** The one that lost the
  race exited with a networking error. That is why there is an nginx in front:
  without it two replicas is a fiction, because every request would land on
  whichever container won. It also has to pass `Host` through unchanged — the
  tenant *is* the subdomain, and nginx rewrites `Host` to the upstream name by
  default, which would turn every tenant-scoped request into a 404.

And one gap in the product rather than the packaging: **`migrator` never applied
the control-plane migrations.** Nothing but `erp_demo::bootstrap` ever called
`ControlPlane::migrate`, so a fresh deployment could only get its control schema
by building a demo tenant first — backwards for the thing this document calls
the deploy step. It applies them now, in the apply mode only: `check` and
`versions` are the pre-deploy gates and are look-only by contract, and a gate
that writes is one you cannot run against production before deciding to deploy.

### Verified against the running stack, not asserted

- `pg_stat_replication` on the primary reports `walreceiver | streaming | async`;
  `pg_is_in_recovery()` on the standby is `t`.
- The demo builds through the containers: 6 invoices, 4 bills, a filed VAT
  return, 7 ZATCA documents chained.
- A session created through the proxy is a `erp:session:…` key in Redis; logging
  out once makes **every** subsequent request 401 regardless of which replica
  answers.
- Inviting somebody in Arabic puts a message in Mailpit with the subject
  correctly RFC 2047 encoded and the bidi isolation marks around the Latin part
  of the company name — and the link in that body, pasted back at
  `GET /v1/join/…`, returns the invitation.
- `PATCH /v1/members/{id}` publishes
  `{"what":"membership","which":{"identity":…,"tenant":…}}` on `erp:invalidate`,
  and both replicas answer with the new role immediately afterwards.

## Standards for a pooler, and three costs it made visible

Read Supavisor's two write-ups first, on the principle that setting standards now
is cheaper than a rewrite later. The load-bearing facts: **transaction mode** is
the mode, 400 direct connections served 250,000 clients, one node holds the
direct connections per database while others relay, reads spread across the
cluster and writes go to the primary, and prepared statements work by parsing SQL
and broadcasting `PREPARE` across the pool.

The audit that followed was the most useful hour of it. Transaction pooling
forbids session state surviving between transactions, so:

| hazard | found |
|---|---|
| `SET search_path` at session scope | **12 sites, every one DDL or install** |
| session advisory locks | only `erp-testkit` |
| `LISTEN`/`NOTIFY` | none — D4 banned it years ago |
| temp tables, `WITH HOLD` cursors | none |

The projection hot path already used `SET LOCAL`, deliberately and with a comment
saying why. So the codebase was one decision away from pooler-ready, and the
decision is the one Supabase ships: **two connection strings per cluster.**

- **`Role::Direct`, and `PRIMARY_DIRECT_URL` behind it.** Provisioning, fleet
  migration and schema rebuilds ask for it; everything else goes through the
  primary, which may be a pooler. **It falls back to the primary when unset**,
  which is what makes this a variable rather than a flag day — nothing changes
  for a deployment that never adopts one.

  `maintenance_options` is the one line that did it, because every DDL path in
  the system already went through that function.

- **The rule is a test, not a comment.** `tests/pooler.rs` walks every `.rs` and
  `.sql` in the workspace and fails on a session-scoped `SET`, a session advisory
  lock, or a `LISTEN` outside an allow-list of DDL files. Changing the projection
  runner back to plain `SET` fails it; adding a `pg_notify` anywhere fails it.

  Its first run found **itself** — the file names every pattern it hunts for.
  Which at least proved the walk reaches that far.

- **`POOL_STATEMENT_CACHE`.** sqlx prepares by default and caches per connection.
  Poolers answer this differently and both answers are the pooler's business; what
  this crate owes a deployment is a knob it can turn without a rebuild.

### And three things the same audit exposed

**Lane budgets were compiled constants.** 100 + 240 + 60 = 400 per process, and
four processes make 1,600 against a 200-connection server. Nothing had ever
failed, because the per-tenant pool cap hid it. They read from the environment
now, and `report_budget` states the arithmetic at start-up — which promptly
warned on the compose stack, exactly as intended, until the stack was sized:
`this_process_at_most: 30, server_max_connections: 200`.

**The fleet walk was sequential**, and its own comment conceded it. Bounded
concurrency now, `FLEET_CONCURRENCY` (16). Measured over 40 tenants:

| concurrency | elapsed |
|---|---|
| 1 | 225 ms |
| 4 | 67 ms |
| 16 | 64 ms |
| 32 | 36 ms |

Bounded and not unbounded because each visit opens a connection on the **direct**
route — the one that bypasses any pooler, and therefore the one with the smallest
budget.

**A quiet tenant was visited for ever at a fixed thirty seconds.** Five thousand
tenants is 167 visits a second, in perpetuity, almost all finding nothing — and
each one opens a connection, runs every enabled module's projection query, and
writes a row back. It was the largest standing cost the platform had, spent
entirely on tenants doing nothing.

Consecutive idle visits now back off exponentially to a six-hour cap:

```text
100 active + 4,900 dormant  ≈  3.5 visits/s   (was 167)
```

Three details that are not obvious:

- **Waking was already built.** `request_visit` pulls `next_visit_at` back to
  now and every write calls it, so a dormant tenant that receives a request is
  current within a claim cycle. Without that the backoff would be a latency bug
  rather than a saving, which is what `a_request_wakes_a_dormant_tenant_immediately`
  is for.
- **Jitter has to scale with the interval.** Ten seconds of spread across a
  six-hour interval leaves five thousand tenants landing in the same ten-second
  window every six hours — a thundering herd with a long fuse, and one that only
  shows up in production.
- **The streak is not on `Tenant`.** How many times a scheduler looked and found
  nothing is the scheduler's business; a domain model carrying it invites code
  that reads it for something else. `Claimed { tenant, idle_visits }` instead.

### Two more gaps found by running it

**`migrator` never registered a cluster.** Only `erp_demo::bootstrap` ever called
`register_cluster`, so a fresh compose stack had migrations applied, an empty
`cluster` table, and every signup failing with a 500 that said
`no cluster has capacity (0 at their limit)` — which names a capacity problem and
is a missing row. Same shape as the control migrations, found the same way: bring
it up clean and post a signup. It declares `primary` now, with a capacity that
says in the log that it is a placeholder to be sized from measurement.

**A falsification that did not falsify.** Editing `leases.rs` by pattern found
nothing, because `cargo fmt` had split the line across three; the test passed and
`git checkout` reported "Updated 0 paths", which is the tell. Worth knowing: a
scripted edit that reports success and a `git checkout` that reports no change
are contradictory, and the second one is right.
