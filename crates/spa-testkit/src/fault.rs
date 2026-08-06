//! Making a process die where it is least convenient.
//!
//! # Why this is a real kill and not a simulated one
//!
//! The property under test is that a transaction interrupted at an arbitrary
//! point leaves no partial state. A test that simulates the crash — returning an
//! error from a fake, or rolling back deliberately — proves that *the code's own
//! rollback path* works, which was never in doubt. What is in doubt is what
//! happens when the process is not asked politely.
//!
//! So [`kill_connection`] issues `pg_terminate_backend` from a *second*
//! connection against the first one's backend PID. The victim's socket is
//! severed by Postgres, its transaction is rolled back by Postgres, and the
//! Rust code finds out the way it would in production: the next statement fails
//! with a connection error.
//!
//! # What this cannot do
//!
//! It kills the database side. The Rust process keeps running, so a defect that
//! depends on in-memory state surviving is not covered here — that is what the
//! shutdown tests in `spa-worker` are for. Between the two, the interesting
//! orderings are covered: `kill_connection` for "the database went away
//! mid-transaction", cancellation for "the process is going away".

use sqlx::{Connection, PgConnection};

use crate::database_url;

/// Severs a connection from outside, the way an operator, an OOM killer, or a
/// failover does.
///
/// The connection is unusable afterwards, and any transaction it had open has
/// been rolled back by the server. Using it again returns a connection error,
/// which is the point: that is what the code under test has to survive.
///
/// ```no_run
/// # use spa_testkit::kill_connection;
/// # async fn example(conn: &mut sqlx::PgConnection) -> anyhow::Result<()> {
/// sqlx::query("INSERT INTO t (id) VALUES (1)").execute(&mut *conn).await?;
/// kill_connection(&mut *conn).await?;
/// // The insert is gone, and the next statement fails.
/// assert!(sqlx::query("SELECT 1").execute(conn).await.is_err());
/// # Ok(())
/// # }
/// ```
pub async fn kill_connection(victim: &mut PgConnection) -> anyhow::Result<()> {
    // Asked before the kill, obviously — afterwards there is nobody to ask.
    let (pid, database): (i32, String) =
        sqlx::query_as("SELECT pg_backend_pid(), current_database()")
            .fetch_one(&mut *victim)
            .await?;

    let mut executioner = connect_to(&database).await?;
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&mut executioner)
        .await?;
    executioner.close().await?;

    Ok(())
}

/// The backend PID behind a connection, for a test that wants to kill it later
/// rather than now.
pub async fn backend_pid(conn: &mut PgConnection) -> anyhow::Result<i32> {
    Ok(sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(conn)
        .await?)
}

/// Kills a backend by PID, for a test holding a pool rather than a connection.
///
/// Pair with [`backend_pid`]: take the PID while the connection is healthy, then
/// kill it from elsewhere — which is how a test crashes a worker it does not
/// hold a reference to.
pub async fn kill_backend(database: &str, pid: i32) -> anyhow::Result<bool> {
    let mut executioner = connect_to(database).await?;
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&mut executioner)
        .await?;
    executioner.close().await?;
    Ok(killed)
}

async fn connect_to(database: &str) -> anyhow::Result<PgConnection> {
    use sqlx::postgres::PgConnectOptions;
    let options = database_url()
        .parse::<PgConnectOptions>()?
        .database(database);
    Ok(PgConnection::connect_with(&options).await?)
}
