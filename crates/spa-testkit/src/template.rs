//! Template databases and per-test clones.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, PgPool};
use tokio::sync::{Mutex, OnceCell};

use crate::schema::Schema;
use crate::{SWEEP_GRACE_MILLIS, TEMPLATE_DB_PREFIX, TEST_DB_PREFIX, database_url};

static TEMPLATES: OnceLock<Mutex<HashMap<String, &'static Template>>> = OnceLock::new();
static SWEPT: OnceCell<()> = OnceCell::const_new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A prepared database that tests are cloned from.
#[derive(Debug)]
pub struct Template {
    name: String,
}

impl Template {
    /// The template for this schema, building it if it does not exist.
    ///
    /// Memoized per fingerprint for the life of the process, and serialized
    /// across processes with an advisory lock, so a `cargo test` that starts
    /// several test binaries at once builds each template exactly once.
    ///
    /// Returns `&'static` because a template outlives every test that uses it
    /// and there is no meaningful teardown — it is reused by the next run.
    pub async fn get(schema: &Schema) -> anyhow::Result<&'static Self> {
        sweep_once().await?;

        let fingerprint = schema.fingerprint();
        let name = format!("{TEMPLATE_DB_PREFIX}{fingerprint}");

        let cache = TEMPLATES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock().await;
        if let Some(existing) = guard.get(&name) {
            return Ok(existing);
        }

        ensure_template(&name, schema).await?;

        // Leaked deliberately: one per schema per process, alive until exit.
        let template: &'static Template = Box::leak(Box::new(Template { name: name.clone() }));
        guard.insert(name, template);
        Ok(template)
    }

    /// A fresh, isolated database cloned from this template.
    pub async fn fresh(&self) -> anyhow::Result<TestDb> {
        let name = unique_test_db_name();
        let mut admin = admin_connection().await?;

        let statement = format!(
            "CREATE DATABASE {} TEMPLATE {}",
            quote_ident(&name)?,
            quote_ident(&self.name)?
        );

        // `CREATE DATABASE … TEMPLATE` fails if anything is connected to the
        // source. Nothing here connects to a template after building it, but a
        // stray session (a psql window, a paused debugger) shouldn't fail a test
        // run, so retry briefly.
        let mut attempts = 0;
        loop {
            match sqlx::raw_sql(AssertSqlSafe(statement.clone()))
                .execute(&mut admin)
                .await
            {
                Ok(_) => break,
                Err(e) if is_source_in_use(&e) && attempts < 20 => {
                    attempts += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        admin.close().await?;

        let pool = PgPoolOptions::new()
            // Small on purpose: a test that needs many concurrent connections is
            // usually a test that should be asserting something else.
            .max_connections(4)
            .connect_with(connect_options()?.database(&name))
            .await?;

        Ok(TestDb { name, pool })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A database owned by one test.
///
/// # Cleanup
///
/// `Drop` makes a best-effort attempt to drop the database, but it cannot
/// `await`, and under `#[tokio::test]`'s single-threaded runtime a task spawned
/// from `Drop` usually will not run before the runtime shuts down. So cleanup is
/// *guaranteed* by the startup sweep, not by `Drop`: leftovers are removed at the
/// beginning of the next run. Call [`TestDb::cleanup`] to drop one immediately.
#[derive(Debug)]
pub struct TestDb {
    name: String,
    pool: PgPool,
}

impl TestDb {
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Drop this database now rather than waiting for the next run's sweep.
    pub async fn cleanup(self) -> anyhow::Result<()> {
        let name = self.name.clone();
        self.pool.close().await;
        drop_database(&name).await
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let name = self.name.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = drop_database(&name).await {
                    tracing::debug!(database = %name, error = %e, "deferred to the startup sweep");
                }
            });
        }
    }
}

/// Creates a database under a caller-chosen name and applies a schema to it.
///
/// For tests that need a database at a *specific* name — provisioning tests,
/// where the control plane has already recorded what the tenant's database will
/// be called. Ordinary tests should use [`Template::fresh`], which is faster and
/// names itself.
///
/// Names are validated as identifiers, so this cannot be pointed at an arbitrary
/// database by a badly-built string.
pub async fn create_named_database(name: &str, schema: &Schema) -> anyhow::Result<()> {
    let create = format!("CREATE DATABASE {}", quote_ident(name)?);
    let mut admin = admin_connection().await?;
    sqlx::raw_sql(AssertSqlSafe(create))
        .execute(&mut admin)
        .await?;
    admin.close().await?;

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(connect_options()?.database(name))
        .await?;
    let applied = schema.apply(&pool).await;
    pool.close().await;
    applied
}

/// Drops a database created by [`create_named_database`].
pub async fn drop_named_database(name: &str) -> anyhow::Result<()> {
    drop_database(name).await
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn connect_options() -> anyhow::Result<PgConnectOptions> {
    Ok(database_url().parse::<PgConnectOptions>()?)
}

/// Quotes a database name for use as a SQL identifier, refusing anything that
/// isn't plainly safe.
///
/// `CREATE`/`DROP DATABASE` cannot take a bind parameter, so the name has to be
/// interpolated. Every name reaching here is generated by this crate or derived
/// from a `&'static str` schema label, but "it's internal" is how injection bugs
/// are argued into existence — so the character set is checked rather than
/// assumed, and a violation is an error rather than an escape.
fn quote_ident(name: &str) -> anyhow::Result<String> {
    let ok = !name.is_empty()
        && name.len() < 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    anyhow::ensure!(ok, "refusing to use {name:?} as a database identifier");
    Ok(format!("\"{name}\""))
}

/// `DROP DATABASE ... WITH (FORCE)` terminates leftover sessions rather than
/// failing (Postgres 13+).
fn drop_database_sql(name: &str) -> anyhow::Result<String> {
    Ok(format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote_ident(name)?
    ))
}

/// A connection to the maintenance database. `CREATE`/`DROP DATABASE` cannot run
/// from inside the database being operated on, nor inside a transaction.
async fn admin_connection() -> anyhow::Result<sqlx::PgConnection> {
    Ok(connect_options()?.database("postgres").connect().await?)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn unique_test_db_name() -> String {
    // Millis for the sweeper's age check, pid to separate concurrent test
    // binaries, counter to separate tests within one binary.
    format!(
        "{TEST_DB_PREFIX}{millis}_{pid}_{n}",
        millis = now_millis(),
        pid = std::process::id(),
        n = COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

/// Postgres 55006: "source database is being accessed by other users".
fn is_source_in_use(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("55006"))
}

async fn database_exists(admin: &mut sqlx::PgConnection, name: &str) -> anyhow::Result<bool> {
    let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(name)
        .fetch_optional(&mut *admin)
        .await?;
    Ok(found.is_some())
}

async fn drop_database(name: &str) -> anyhow::Result<()> {
    let sql = drop_database_sql(name)?;
    let mut admin = admin_connection().await?;
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&mut admin)
        .await?;
    admin.close().await?;
    Ok(())
}

/// Builds the template if it is absent or was left half-built.
///
/// Completion is recorded by a marker table created *after* the schema applies.
/// A database that exists without the marker is the residue of a crashed build
/// and is rebuilt — otherwise a partial schema would silently become the thing
/// every test runs against, which is the one failure mode a template-based
/// harness must not have.
async fn ensure_template(name: &str, schema: &Schema) -> anyhow::Result<()> {
    const MARKER: &str = "_spa_testkit_template_ready";

    let mut admin = admin_connection().await?;

    // Serialize builders across processes. Session-scoped, because
    // CREATE DATABASE cannot run inside a transaction.
    let lock_key = advisory_key(name);
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_key)
        .execute(&mut admin)
        .await?;

    let result = build_template_locked(&mut admin, name, schema, MARKER).await;

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_key)
        .execute(&mut admin)
        .await?;
    admin.close().await?;

    result
}

async fn build_template_locked(
    admin: &mut sqlx::PgConnection,
    name: &str,
    schema: &Schema,
    marker: &str,
) -> anyhow::Result<()> {
    if database_exists(admin, name).await? {
        if template_is_complete(name, marker).await? {
            return Ok(());
        }
        tracing::warn!(template = name, "half-built template found, rebuilding");
        sqlx::raw_sql(AssertSqlSafe(drop_database_sql(name)?))
            .execute(&mut *admin)
            .await?;
    }

    sqlx::raw_sql(AssertSqlSafe(format!(
        "CREATE DATABASE {}",
        quote_ident(name)?
    )))
    .execute(&mut *admin)
    .await?;

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(connect_options()?.database(name))
        .await?;

    let marker_ident = marker;
    let applied = async {
        schema.apply(&pool).await?;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE TABLE {} (ok BOOLEAN NOT NULL)",
            quote_ident(marker_ident)?
        )))
        .execute(&pool)
        .await?;
        anyhow::Ok(())
    }
    .await;

    // Always close: a template with a live connection cannot be cloned.
    pool.close().await;

    if let Err(e) = applied {
        sqlx::raw_sql(AssertSqlSafe(drop_database_sql(name)?))
            .execute(&mut *admin)
            .await?;
        return Err(e);
    }
    Ok(())
}

async fn template_is_complete(name: &str, marker: &str) -> anyhow::Result<bool> {
    let mut conn = connect_options()?.database(name).connect().await?;
    let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_tables WHERE tablename = $1")
        .bind(marker)
        .fetch_optional(&mut conn)
        .await?;
    conn.close().await?;
    Ok(found.is_some())
}

fn advisory_key(name: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish().cast_signed()
}

/// Drops leftover test databases, once per process.
///
/// Two conditions must both hold, which together make it impossible to sweep a
/// database another test binary is using:
///
/// 1. **No active connections.** A running test holds a pool, and pooled
///    connections appear in `pg_stat_activity` even when idle.
/// 2. **Older than the grace window.** Covers the microsecond gap between
///    `CREATE DATABASE` and the first connection.
///
/// Templates are never swept — they are the cache that makes this fast.
async fn sweep_once() -> anyhow::Result<()> {
    SWEPT
        .get_or_try_init(|| async {
            let mut admin = admin_connection().await?;
            let names: Vec<(String,)> = sqlx::query_as(
                "SELECT d.datname
                   FROM pg_database d
                  WHERE d.datname LIKE $1
                    AND NOT EXISTS (
                        SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname
                    )",
            )
            .bind(format!("{TEST_DB_PREFIX}%"))
            .fetch_all(&mut admin)
            .await?;

            let now = now_millis();
            let mut dropped = 0usize;
            for (name,) in names {
                let Some(age) = age_from_name(&name, now) else {
                    continue; // unparseable: not ours to reason about, leave it
                };
                if age < SWEEP_GRACE_MILLIS {
                    continue;
                }
                let Ok(sql) = drop_database_sql(&name) else {
                    continue;
                };
                if sqlx::raw_sql(AssertSqlSafe(sql))
                    .execute(&mut admin)
                    .await
                    .is_ok()
                {
                    dropped += 1;
                }
            }
            admin.close().await?;

            if dropped > 0 {
                tracing::info!(dropped, "swept leftover test databases");
            }
            anyhow::Ok(())
        })
        .await?;
    Ok(())
}

/// Age in milliseconds of a database from the timestamp embedded in its name.
/// `None` if the name does not have the expected shape.
fn age_from_name(name: &str, now: u128) -> Option<u128> {
    let rest = name.strip_prefix(TEST_DB_PREFIX)?;
    let millis: u128 = rest.split('_').next()?.parse().ok()?;
    Some(now.saturating_sub(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_is_read_from_the_name() {
        let now = 10_000;
        assert_eq!(age_from_name("spa_test_4000_123_7", now), Some(6_000));
        // Clock skew must not produce a huge age that sweeps a live database.
        assert_eq!(age_from_name("spa_test_99999_1_0", now), Some(0));
        assert_eq!(age_from_name("spa_test_notanumber_1_0", now), None);
        assert_eq!(age_from_name("some_other_database", now), None);
    }

    #[test]
    fn generated_names_are_unique_and_parseable() {
        let a = unique_test_db_name();
        let b = unique_test_db_name();
        assert_ne!(a, b);
        assert!(a.starts_with(TEST_DB_PREFIX));
        assert!(age_from_name(&a, now_millis()).is_some());
        // Postgres truncates identifiers at 63 bytes; a truncated name would
        // collide with another test's database.
        assert!(a.len() < 63, "database name too long: {a}");
    }
}
