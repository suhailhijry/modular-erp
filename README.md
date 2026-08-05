# SPA

Multi-tenant ERP backend. Rust, Postgres, one database per tenant.

## Documents

| | |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Decisions, laws, contracts. Read before writing code; update before changing a decision. |
| [docs/IMPLEMENTATION.md](docs/IMPLEMENTATION.md) | Phased build order and current state. |
| [docs/DATABASE_SETUP.md](docs/DATABASE_SETUP.md) | Which database is for what. |

## Getting started

```sh
cargo build --workspace     # no database needed — queries are checked offline
cargo test --workspace      # needs a reachable Postgres
```

Tests create and drop their own databases. `DATABASE_URL` supplies only the host
and credentials; without it, `postgres://postgres@localhost/postgres` is assumed.

```sh
just check                  # fmt, clippy, tests — what CI runs
just prepare                # after changing a migration; commit the .sqlx diff
```

## Layout

```
crates/spa-types      value types — no I/O, WASM-safe, shareable with a frontend
crates/spa-testkit    test harness — real Postgres, one database per test
crates/spa-control    control plane — identities, tenants, memberships, TenantDb
migrations/control    control-plane schema
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

## History

The prototype this replaces is preserved at tag `f2e8acd`. Its review — including
four defects reproduced against a live database — is what shaped the decisions in
`docs/ARCHITECTURE.md`.
