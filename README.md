# SPA

Multi-tenant ERP backend. Rust, Postgres, one database per tenant.

## Documents

| | |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Decisions, laws, contracts. Read before writing code; update before changing a decision. |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | Phased build order and current state. |
| [docs/DATABASE_SETUP.md](docs/DATABASE_SETUP.md) | Which database is for what. |

## Getting started

```bash
cargo build --workspace
```

Builds need no database — queries are checked against committed offline data.

```bash
cargo test --workspace
```

Tests need a reachable Postgres and nothing else. The harness reads
`DATABASE_URL` from the environment or from `.env` (cargo does not read `.env`
itself, so the harness does), falling back to
`postgres://postgres@localhost/postgres`. It uses only the host and credentials,
creating and dropping its own databases. If it cannot connect, the error names
what it tried and where the setting came from — see
[docs/DATABASE_SETUP.md](docs/DATABASE_SETUP.md).

```bash
just check
```

fmt, clippy and tests — what CI runs. After changing a migration, `just prepare`
regenerates the offline query data; commit the `.sqlx/` diff alongside it.

## Layout

```
crates/spa-types      value types — no I/O, WASM-safe, shareable with a frontend
crates/spa-i18n       message codes and typed arguments; English and Arabic
crates/spa-testkit    test harness — real Postgres, one database per test
crates/spa-control    control plane — identities, tenants, memberships, TenantDb
crates/spa-eventlog   the tenant log: gapless append, aggregates, upcasters, outbox
crates/spa-projection projection groups, checkpoints, shadow replay and the differ
crates/spa-worker     background worker — tenant visits, jobs, drain; bin/worker
migrations/control    control-plane schema
migrations/tenant     per-tenant schema
```

Crates are added as the phases in `docs/IMPLEMENTATION.md` reach them; the full
target layout is in architecture §6.

## Three things worth knowing up front

**There is no ambient database pool.** The only route to a tenant's data is a
`TenantDb`, which has no public constructor — `ControlPlane::enter` is the sole
source, and it checks identity, tenant status, and membership first. A query
against the wrong tenant isn't prevented by a `WHERE` clause; it can't be written.

**Money has no `+`.** `Money::checked_add` returns a `Result` because currencies
are runtime data and mismatches must be handled. Amounts are integer minor units;
`float_arithmetic` is denied workspace-wide.

**Failures stop, they never degrade.** No swallowed errors, no "log a warning and
carry on with the feature disabled". In a system of record a loud failure costs
an incident and a quiet one costs an audit. This is architecture law L6, and
`unwrap`/`expect`/`panic` are lint-warned outside tests to keep it honest.

**Nothing performs I/O inline.** A command returns events *and* effects, both
written in one transaction; a worker delivers the effects afterwards. So a
rolled-back command emails nobody, a crashed one still owes what it promised, and
rebuilding a read model sends nothing at all.

## History

The prototype this replaces is preserved at tag `f2e8acd`. Its review — including
four defects reproduced against a live database — is what shaped the decisions in
`docs/ARCHITECTURE.md`.
