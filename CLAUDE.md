# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A modular, multi-tenant ERP backend in Rust (edition 2024, toolchain pinned in `rust-toolchain.toml`). Postgres 18 + Redis. Source-available (BSL 1.1); no crate is ever published. Saudi Arabia is the first market, so Arabic/i18n and ZATCA e-invoicing are first-class, not add-ons.

Design docs are unusually thorough and are the source of truth: `docs/ARCHITECTURE.md` (decisions D1–D18 and laws L1–L8; change it *before* changing a decision), `docs/RUNNING.md` (processes, env vars, ops), `docs/DATABASE_SETUP.md`, `docs/IMPLEMENTATION.md` (15k-line phase log; read "What needs work now" and "Road to selling", not the whole thing), and the handbook in `docs/book/src`.

## Commands

Task runner is `just` (`.env` is auto-loaded; needs `DATABASE_URL`). Builds are **offline**: `SQLX_OFFLINE=true` in `.cargo/config.toml`, so `cargo build` never touches a database.

```bash
just redis          # Redis on 6379 — the suite REFUSES (not skips) without it; compose's 56379 does not match
just prepare        # rebuild erp_typecheck DB + regenerate .sqlx/ — run after ANY migration or module install.sql change, commit the .sqlx diff
just check          # fmt-check + clippy (-D warnings) + nextest + doctests — what CI runs
just test           # cargo nextest run --workspace --no-fail-fast, then cargo test --workspace --doc
just lint           # cargo clippy --workspace --all-targets -- -D warnings
just deny           # cargo-deny (advisories/licences) — separate CI job
just openapi        # regenerate docs/openapi.json; `just check` fails if it drifts
just baseline       # accept an API change into docs/openapi.baseline.json (deliberate act)
just demo <pw>      # seed a demo tenant with every module enabled
just clean-databases  # drop leaked erp_test_*/erp_tmpl_*/erp_tenant_* DBs (refuses while tests run)
```

Single test (nextest runs each test in its own process; doctests are NOT run by nextest):

```bash
cargo nextest run -p ledger -E 'test(some_name)'
cargo test -p erp-eventlog --test append some_name    # plain cargo also works
cargo test --workspace --doc                          # doctests
```

Tests use a real Postgres via `erp-testkit` (a fresh DB cloned from a template per test; no DB mocks anywhere). Postgres client tools (`psql`, `pg_dump`) must be version 18 or `erp-control::restore` fails. `#[ignore]`d tests (soak, rebuild benchmark, ZATCA sandbox) need extra setup; the S3 tests skip unless `S3_BUCKET` is set. Full stack: `docker compose up` (one image, six binaries).

## Architecture

**Tenant isolation (D1):** each tenant has its own Postgres database; a control-plane database maps tenant → (cluster, database). The only route to tenant data is a `TenantDb` handle from `erp-tenant`; there is no ambient pool, so cross-tenant access is unrepresentable. Connections are budgeted by "lanes" (`Interactive`/`Client`/`Background`), a permit per DB *operation*; exhaustion returns 503.

**Two persistence styles:** the control plane (identity, membership, entitlements) is normalized tables + audit; the tenant plane is an **event-sourced write model with projected read models**. Postgres is both the log and the internal transport (no Kafka, no `pg_notify`; a test in `erp-control/tests/pooler.rs` bans session-scoped `SET`, session advisory locks and stray `LISTEN`).

**The laws (each has a test; a violation is a build failure)** — the ones that bite when writing code:
- Projections (`apply`) are pure functions of the event stream: no clock, no randomness, no I/O, no reading config (L2, `erp-projection/tests/purity.rs`). Config is resolved at command time and frozen into events (D6/L5).
- A projection group is the unit of consistency; groups never read each other's tables (L3). A module's read models live in schema `proj_<crate_name>`.
- Aggregates are loaded only while handling a command; reads come from read models (L7).
- Identity of a write comes from the caller (`Idempotency-Key`, bank reference) — handlers never mint their own ids (L8, `erp-api/tests/idempotence.rs`).
- Failures stop; nothing degrades silently (L6). `unwrap`/`expect`/`panic`/`todo` are clippy-warned outside tests; `unsafe` is forbidden; float arithmetic is denied (money is integer minor units, `Money` has no `Add`).
- Effects (email, ZATCA, etc.) are values a command writes to an **outbox**, delivered at-least-once with idempotency keys — never inline I/O, never derived by projections.
- Errors are message codes + typed args (never sentences), rendered per-locale; `erp_i18n::testing::assert_complete` fails the build if a code lacks a language.

**Crate layering** (dependencies point down only):
`erp-types` → `erp-i18n`, `erp-eventlog` → `erp-projection` → `erp-tenant` (the narrow seam modules use: `TenantDb`, `ModuleSetup`, `EnabledModules`) → `erp-control` (fleet, identities, placement) → `erp-worker` (job loop, `bin/worker|migrator|reaper|operator`) / `erp-web` (extractors, problem+json) → `erp-api` (core routes + module registry + `bin/api`, the composition root) → `erp-demo`. `erp-testkit` is the harness. Leaf helper crates: `erp-occupancy`, `erp-recurrence`, `erp-links`, `erp-storage`, `erp-payments`, `erp-rules`.

**Modules** (`modules/*`, e.g. `ledger`, `sales`, `tax_sa`, `booking`) are compiled in and enabled per tenant at runtime (D7). The kernel holds no business domain (D11) — even accounting is a module. Modules may depend on `erp-tenant`/`erp-web` and on modules *below* them (`tax_sa → sales/purchases → ledger`), **never on `erp-control`** (`erp-tenant/tests/boundary.rs`) and never on another module's tables. Cross-module extension is by subscribing to events, not by calls/hooks. Each module ships its own axum routes (`http::routes()`), `CATALOG` of messages, `setup()` (`schema/install.sql` DDL vs separate `seed_sql`), projections and upcasters.

**Adding a module** touches, outside its own crate: root `Cargo.toml` (workspace dep), `crates/erp-api` (`Cargo.toml`, `src/modules.rs` `REGISTERED` list, `src/catalog.rs`), `crates/erp-worker` (`Cargo.toml`, `bin/worker.rs` job registry, `bin/migrator.rs`), `crates/erp-demo` (`Cargo.toml`, `src/lib.rs`). Its schema must be named after its crate (hyphens → underscores) — `just prepare` guesses the schema from the directory name.

## Migrations and schema

- `migrations/tenant/` and `migrations/control/` are **expand-only** (`erp-control/tests/migrations.rs`): nothing a draining pod still uses may be dropped, renamed, retyped or newly constrained, and columns added to append-only tables (`event`, `audit_entry`) need a non-NULL default. Exceptions go in `migrations/EXEMPTIONS` with a reason.
- **Never edit an applied migration**, even a comment — sqlx checksums it and strands every existing database (`VersionMismatch`). That is why exemptions live in a separate file.
- Read models carry a version; a build refuses to project into (and the API answers 503 for) a group not at its version. Rebuilds go through `just migrate-fleet`, and `migrator check|versions` are the pre-deploy gates.
- `.sqlx/` must match the migrations; CI's `offline-data` job fails on a stale one.

## Conventions

- Use `crates/erp-testkit` for anything needing a database; don't mock Postgres.
- API changes must keep `docs/openapi.json` current (`just openapi`) and pass the compatibility test against `docs/openapi.baseline.json`; a handler that is served but lacks `#[utoipa::path]` does not compile.
- One TLS/crypto stack on purpose: OpenSSL/`native-tls` everywhere (sqlx, reqwest, lettre, redis, object_store). Don't add rustls or `aws-lc-rs` dependencies (see comments in the root `Cargo.toml`).
- Old event versions must stay readable: bump an event's version with an upcaster (`Upcasters::also` folds another module's history in). Golden JSON per version lives in `crates/erp-eventlog/tests/golden` and is decoded every build.
