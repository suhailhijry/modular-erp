//! Test harness: real Postgres, one fresh database per test.
//!
//! # Why no mocks
//!
//! Three of the four defects found in the prototype — an `ON CONFLICT` clause
//! with no matching constraint, a query naming a column that doesn't exist, and
//! two tables that existed only inside a comment — are invisible to any test
//! that mocks the database and unmissable to any test that doesn't. So there is
//! no database mock in this codebase, and this crate exists to make going
//! without one cheap rather than painful.
//!
//! # How it stays fast
//!
//! `CREATE DATABASE … TEMPLATE` copies a prepared database at the filesystem
//! level. A migrated template is built once per schema fingerprint and reused
//! across runs; each test clones it.
//!
//! Measured on local Postgres 18 with a trivial schema: **≈280 ms to acquire**
//! (clone plus connect) and ≈140 ms to drop. Only acquisition is on a test's
//! critical path — teardown is normally left to the startup sweep. At eight-way
//! parallelism that is a few seconds of setup across a suite of hundreds, which
//! is the point: isolation cheap enough to be the default.
//!
//! `cargo test -p erp-testkit --test harness cloning_is_fast -- --nocapture`
//! reprints these numbers for the machine you are on.
//!
//! # Usage
//!
//! ```no_run
//! use erp_testkit::{Schema, Template};
//!
//! static SCHEMA: Schema = Schema::sql("example", &["CREATE TABLE t (id INT)"]);
//!
//! # async fn example() -> anyhow::Result<()> {
//! let db = Template::get(&SCHEMA).await?.fresh().await?;
//! sqlx::query("INSERT INTO t (id) VALUES (1)")
//!     .execute(db.pool())
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod fault;
mod schema;
mod template;

pub use fault::{backend_pid, kill_backend, kill_connection};
pub use schema::Schema;
pub use template::{Template, TestDb, create_named_database, drop_named_database};

/// Prefix for every database this harness creates. The sweeper only ever
/// considers names starting with this, so no database it did not create can be
/// caught by cleanup.
pub(crate) const TEST_DB_PREFIX: &str = "erp_test_";
pub(crate) const TEMPLATE_DB_PREFIX: &str = "erp_tmpl_";

/// A test database is protected from the sweeper for this long, regardless of
/// whether anything is connected to it.
///
/// This closes the only race in the sweep. A running test holds a pool, and
/// pooled connections are visible in `pg_stat_activity` even when idle, so an
/// in-use database is protected by the connection check. The one gap is between
/// `CREATE DATABASE` and the first connection — microseconds — and a minute of
/// grace puts it comfortably out of reach. Names carry their creation time so
/// the check needs no extra bookkeeping.
pub(crate) const SWEEP_GRACE_MILLIS: u128 = 60 * 1000;

/// Where to reach Postgres.
///
/// Resolution order, first hit wins:
///
/// 1. `DATABASE_URL` already in the environment
/// 2. `DATABASE_URL` in a `.env` file, searched from the current directory up
/// 3. `postgres://postgres@localhost/postgres`
///
/// Step 2 exists because **cargo does not read `.env`**. Without it, a developer
/// whose Postgres needs a password gets `password authentication failed` from
/// `cargo test` even though their `.env` is correct — the test binary simply
/// never saw it. `sqlx-cli` loads `.env` for the same reason.
///
/// Only the host and credentials are used. The harness connects to the
/// `postgres` maintenance database and creates its own, so the database named in
/// the URL is irrelevant and is never touched.
pub fn database_url() -> String {
    // Once per process: `.env` does not change mid-run, and this walks the
    // filesystem.
    static FROM_DOTENV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }

    let found = FROM_DOTENV.get_or_init(|| {
        // Searches upward from the current directory, so it works whether cargo
        // runs a test binary from the workspace root or a crate directory.
        dotenvy::dotenv().ok();
        std::env::var("DATABASE_URL").ok()
    });

    found
        .clone()
        .unwrap_or_else(|| "postgres://postgres@localhost/postgres".to_owned())
}

/// A connection URL with its password replaced, safe to put in an error or a log.
#[must_use]
pub fn redacted_url(url: &str) -> String {
    // postgres://user:secret@host/db  ->  postgres://user:***@host/db
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((credentials, host)) = rest.split_once('@') else {
        return url.to_owned();
    };
    let user = credentials.split(':').next().unwrap_or(credentials);
    if credentials.contains(':') {
        format!("{scheme}://{user}:***@{host}")
    } else {
        url.to_owned()
    }
}
