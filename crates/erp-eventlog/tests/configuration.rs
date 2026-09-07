//! The configuration store: one key, one version, and a write that can say
//! which version it read.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use erp_eventlog::ConfigError;
use erp_eventlog::configuration::{get, set, version_of};
use erp_testkit::{Schema, Template, TestDb};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

async fn tenant_db() -> TestDb {
    Template::get(&TENANT)
        .await
        .expect("tenant template builds")
        .fresh()
        .await
        .expect("tenant database clones")
}

/// **The second of two people editing the same setting is told so.** The first
/// version's `set` was an unconditional upsert: every settings screen was
/// last-write-wins, and the loser never knew.
#[tokio::test]
async fn a_conditional_write_lands_only_on_the_version_it_read() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    assert_eq!(
        version_of(&mut conn, "k").await.expect("reads"),
        0,
        "a key nobody has set is at version zero"
    );

    // "I read nothing" is a condition too, and a fresh key satisfies it.
    let first = set(&mut conn, "k", &1, None, Some(0))
        .await
        .expect("a fresh key is at version zero");
    assert_eq!(version_of(&mut conn, "k").await.expect("reads"), first);

    let second = set(&mut conn, "k", &2, None, Some(first))
        .await
        .expect("the version it read is the version it is at");
    assert!(second > first);

    let stale = set(&mut conn, "k", &3, None, Some(first)).await;
    assert!(
        matches!(
            stale,
            Err(ConfigError::Conflict { expected, found, .. }) if expected == first && found == second
        ),
        "a write against a version that has moved on was accepted: {stale:?}"
    );
    assert_eq!(
        get::<i32>(&mut conn, "k")
            .await
            .expect("reads")
            .expect("set")
            .value,
        2,
        "the stale write changed nothing"
    );

    let read_nothing = set(&mut conn, "k", &3, None, Some(0)).await;
    assert!(
        matches!(read_nothing, Err(ConfigError::Conflict { expected: 0, .. })),
        "somebody who read nothing is second to whoever set it: {read_nothing:?}"
    );

    let unconditional = set(&mut conn, "k", &4, None, None)
        .await
        .expect("no condition, no conflict");
    assert!(unconditional > second);
    assert_eq!(
        get::<i32>(&mut conn, "k")
            .await
            .expect("reads")
            .expect("set")
            .value,
        4
    );
}
