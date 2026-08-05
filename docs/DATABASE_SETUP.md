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

```bash
cargo test --workspace
```

That is the whole thing. No environment setup, no manual export.

### Where the connection comes from

`spa-testkit` resolves Postgres in this order, first hit wins:

1. `DATABASE_URL` already in the environment
2. `DATABASE_URL` in a `.env` file, searched from the current directory upward
3. `postgres://postgres@localhost/postgres`

Step 2 matters because **cargo does not read `.env`**. Without the harness
loading it, a correct `.env` still produces `password authentication failed`
from `cargo test`, because the test binary never saw the variable. `sqlx-cli`
loads `.env` for the same reason.

Only the host and credentials are used. The harness connects to the `postgres`
maintenance database and creates its own databases, so whatever database the URL
names is irrelevant and is never touched.

### If a test cannot connect

The failure names what it tried, where the setting came from, and what Postgres
said — with the password redacted:

```
could not connect to Postgres.
  tried:  postgres://nosuchuser:***@localhost/postgres
  from:   the DATABASE_URL environment variable
  error:  password authentication failed for user "nosuchuser"
```

The `from:` line is the useful one. If it says *the built-in default* while you
have a `.env`, the file is not on the search path from where cargo was invoked.

### Leftover databases

Test databases accumulate during a run and are swept at the start of the next
one — see `spa-testkit`'s module docs for why cleanup works that way. To clear
everything this project created:

```bash
just clean-databases
```
