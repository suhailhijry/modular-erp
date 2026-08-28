# erp-testkit

Real Postgres, one fresh database per test, cheap enough that isolation is the
default.

**Depends on:** everything, as a dev-dependency.
**Used by:** every crate's tests.

## Why no mocks

Three of the four defects found in the prototype were an `ON CONFLICT` clause
with no matching constraint, a query naming a column that does not exist, and two
tables that existed only inside a comment. All three are invisible to any test
that mocks the database and unmissable to any test that does not.

So there is no database mock in this codebase, and this crate exists to make
going without one cheap.

## How it stays fast

`CREATE DATABASE … TEMPLATE` copies a prepared database at the filesystem level.
A migrated template is built once per schema fingerprint and reused across runs,
and each test clones it.

Measured on local Postgres 18 with a trivial schema: **≈280 ms to acquire** and
≈140 ms to drop. Only acquisition is on a test's critical path, because teardown
is normally left to the startup sweep. At eight-way parallelism that is a few
seconds of setup across a suite of hundreds.

Reprint the numbers for your machine:

```bash
cargo test -p erp-testkit --test harness cloning_is_fast -- --nocapture
```

## Usage

```rust
use erp_testkit::{Schema, Template};

static SCHEMA: Schema = Schema::sql("example", &["CREATE TABLE t (id INT)"]);

let db = Template::get(&SCHEMA).await?.fresh().await?;
sqlx::query("INSERT INTO t (id) VALUES (1)").execute(db.pool()).await?;
```

For a real schema, point it at a migrator:

```rust
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);
```

## Schema

```rust
pub struct Schema { … }

impl Schema {
    pub const fn migrations(label: &'static str, migrator: &'static Migrator) -> Self;
    pub const fn sql(label: &'static str, statements: &'static [&'static str]) -> Self;
    pub const fn label(&self) -> &'static str;
}
```

Both variants are wanted. `migrations` is the production path; `sql` is for tests
of the harness itself and for small fixtures that do not warrant a migration
directory.

The **fingerprint** is the important part. It identifies the template database
for this exact schema content, so changing a migration changes the fingerprint
and the next test run builds a new template. Tests passing against a stale
template is the one failure mode a template-based harness must not have.

## Template and TestDb

```rust
pub struct Template { … }
impl Template {
    pub async fn get(schema: &Schema) -> anyhow::Result<&'static Self>;
    pub async fn fresh(&self) -> anyhow::Result<TestDb>;
    pub fn name(&self) -> &str;
}

pub struct TestDb { … }
impl TestDb {
    pub fn pool(&self) -> &PgPool;
    pub fn name(&self) -> &str;
    pub async fn cleanup(self) -> anyhow::Result<()>;
}

pub async fn create_named_database(name: &str, schema: &Schema) -> anyhow::Result<()>;
pub async fn drop_named_database(name: &str) -> anyhow::Result<()>;
```

`Template::get` is memoized per fingerprint for the life of the process and
serialized across processes with an advisory lock, so a `cargo test` that starts
several test binaries at once builds each template exactly once. It returns
`&'static` because a template outlives every test that uses it and there is no
meaningful teardown.

### Cleanup is by sweep, not by Drop

`Drop` makes a best-effort attempt, but it cannot `await`, and under
`#[tokio::test]`'s single-threaded runtime a task spawned from `Drop` usually
will not run before the runtime shuts down.

So cleanup is *guaranteed* by the startup sweep: leftovers are removed at the
beginning of the next run. Call `TestDb::cleanup` to drop one immediately.

The sweep only ever considers names starting with `erp_test_` or `erp_tmpl_`, so
no database it did not create can be caught by it. A database is protected for a
minute regardless of whether anything is connected, which closes the only race:
a running test holds a pool and pooled connections are visible in
`pg_stat_activity` even when idle, and the one gap is the microseconds between
`CREATE DATABASE` and the first connection.

`create_named_database` is for tests that need a database at a **specific** name,
which is provisioning tests, where the control plane has already recorded what
the tenant's database will be called. Names are validated as identifiers, so it
cannot be pointed at an arbitrary database by a badly-built string.

## Fault injection

```rust
pub async fn kill_connection(victim: &mut PgConnection) -> anyhow::Result<()>;
pub async fn backend_pid(conn: &mut PgConnection) -> anyhow::Result<i32>;
pub async fn kill_backend(database: &str, pid: i32) -> anyhow::Result<bool>;
```

```rust
sqlx::query("INSERT INTO t (id) VALUES (1)").execute(&mut *conn).await?;
kill_connection(&mut *conn).await?;
// The insert is gone, and the next statement fails.
```

**This is a real kill, not a simulated one.** The property under test is that a
transaction interrupted at an arbitrary point leaves no partial state. A test
that simulates the crash, by returning an error from a fake or rolling back
deliberately, proves that the code's own rollback path works, which was never in
doubt. What is in doubt is what happens when the process is not asked politely.

So `kill_connection` issues `pg_terminate_backend` from a *second* connection
against the first one's backend PID. The victim's socket is severed by Postgres,
its transaction is rolled back by Postgres, and the Rust code finds out the way
it would in production: the next statement fails with a connection error.

Pair `backend_pid` with `kill_backend` when the test holds a pool and not a
connection. Take the PID while the connection is healthy, then kill it from
elsewhere, which is how a test crashes a worker it does not hold a reference to.

**What this cannot do** is kill the Rust side. The process keeps running, so a
defect that depends on in-memory state surviving is not covered here. That is
what the shutdown tests in `erp-worker` are for. Between the two, the interesting
orderings are covered: `kill_connection` for "the database went away
mid-transaction", cancellation for "the process is going away".

## Where the database comes from

```rust
pub fn database_url() -> String;
pub fn redacted_url(url: &str) -> String;
```

Resolution order, first hit wins:

1. `DATABASE_URL` already in the environment
2. `DATABASE_URL` in a `.env` file, searched from the current directory up
3. `postgres://postgres@localhost/postgres`

Step 2 exists because **cargo does not read `.env`**. Without it, a developer
whose Postgres needs a password gets `password authentication failed` from
`cargo test` even though their `.env` is correct, because the test binary never
saw it. `sqlx-cli` loads `.env` for the same reason, and so does the `justfile`.

Only the host and credentials are used. The harness connects to the `postgres`
maintenance database to create and drop.

`redacted_url` replaces the password, so a URL is safe to put in an error or a
log.
