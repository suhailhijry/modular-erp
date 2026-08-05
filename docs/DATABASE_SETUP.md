# Database setup

Two databases with different jobs. Confusing them is how a query gets validated
against the wrong schema and passes.

| Database | Purpose | Who touches it |
|---|---|---|
| `spa_typecheck` | Compile-time target for `sqlx::query!`. Holds every schema — control plane and tenant plane — so macros can be checked. Contains no data. | `just prepare` only |
| *(per test)* `spa_test_*` | Cloned from a template by `spa-testkit`, one per test, dropped by the next run's sweep. | The test suite |

There is deliberately **no shared development database**. Anything that needs
data creates a tenant.

## Builds are offline

`.sqlx/` holds prepared query metadata and `SQLX_OFFLINE=true` is set in
`.cargo/config.toml`, so `cargo build` and CI never open a database connection.
A query whose SQL no longer matches the committed metadata fails the build.

## After changing a migration

```sh
just prepare
```

which rebuilds `spa_typecheck` from `migrations/` and regenerates `.sqlx/`.
Commit the result — a diff in `.sqlx/` is the reviewable evidence that a schema
change altered a query's types.

If `just` isn't installed, the equivalent is in the `prepare` recipe of the
`justfile`.

## Running tests

```sh
cargo test --workspace
```

`spa-testkit` needs a reachable Postgres. It reads `DATABASE_URL` for host and
credentials only — it connects to the `postgres` maintenance database and creates
its own — so any valid URL on the right server works. Without one it falls back
to `postgres://postgres@localhost/postgres`.

Test databases accumulate during a run and are swept at the start of the next
one. See `spa-testkit`'s module docs for why cleanup works that way.
