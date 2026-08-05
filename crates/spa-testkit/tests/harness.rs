//! Tests of the harness itself.
//!
//! These require a reachable Postgres (`DATABASE_URL`, or a local default). They
//! are the reason every later test can trust its database is its own.

// `clippy.toml`'s `allow-expect-in-tests` only reaches `#[cfg(test)]` modules;
// an integration test is an ordinary crate, so the allowance is declared here.
// In a test, a panicking `expect` *is* the failure report.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use spa_testkit::{Schema, Template};

static SCHEMA: Schema = Schema::sql(
    "harness",
    &[
        "CREATE TABLE widget (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, label TEXT NOT NULL)",
        "INSERT INTO widget (label) VALUES ('seeded')",
    ],
);

/// A different schema, to prove templates are keyed by content.
static OTHER_SCHEMA: Schema = Schema::sql("harness-other", &["CREATE TABLE gadget (id INT)"]);

async fn count_widgets(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM widget")
        .fetch_one(pool)
        .await
        .expect("widget table exists in a database cloned from SCHEMA")
}

#[tokio::test]
async fn a_fresh_database_carries_the_seeded_schema() {
    let db = Template::get(&SCHEMA)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clone succeeds");

    assert_eq!(count_widgets(db.pool()).await, 1);
    db.cleanup().await.expect("cleanup succeeds");
}

/// The property everything else depends on: one test's writes are invisible to
/// another's. If this fails, every test in the workspace is suspect.
#[tokio::test]
async fn tests_cannot_see_each_others_writes() {
    let template = Template::get(&SCHEMA).await.expect("template builds");
    let first = template.fresh().await.expect("clone succeeds");
    let second = template.fresh().await.expect("clone succeeds");

    assert_ne!(first.name(), second.name());

    sqlx::query("INSERT INTO widget (label) VALUES ('only in first')")
        .execute(first.pool())
        .await
        .expect("insert succeeds");

    assert_eq!(count_widgets(first.pool()).await, 2);
    assert_eq!(
        count_widgets(second.pool()).await,
        1,
        "the second database must not observe the first's write"
    );

    first.cleanup().await.expect("cleanup succeeds");
    second.cleanup().await.expect("cleanup succeeds");
}

#[tokio::test]
async fn distinct_schemas_get_distinct_templates() {
    let one = Template::get(&SCHEMA).await.expect("template builds");
    let two = Template::get(&OTHER_SCHEMA).await.expect("template builds");
    assert_ne!(one.name(), two.name());

    let db = two.fresh().await.expect("clone succeeds");
    // `gadget` exists here; `widget` must not.
    sqlx::query("SELECT count(*) FROM gadget")
        .execute(db.pool())
        .await
        .expect("gadget exists");
    assert!(
        sqlx::query("SELECT count(*) FROM widget")
            .execute(db.pool())
            .await
            .is_err(),
        "a database cloned from OTHER_SCHEMA must not have SCHEMA's tables"
    );
    db.cleanup().await.expect("cleanup succeeds");
}

#[tokio::test]
async fn the_same_schema_resolves_to_one_template() {
    // Memoization: asking twice must not build twice.
    let one = Template::get(&SCHEMA).await.expect("template builds");
    let two = Template::get(&SCHEMA).await.expect("template builds");
    assert_eq!(one.name(), two.name());
}

/// Cloning has to be fast enough that per-test isolation is the default rather
/// than something people avoid. The threshold is deliberately loose — this is a
/// regression guard against an accidental migrate-per-test, not a benchmark.
///
/// Acquisition and teardown are measured separately because only acquisition is
/// on a test's critical path: teardown is normally left to the startup sweep.
#[tokio::test]
async fn cloning_is_fast_enough_to_use_everywhere() {
    const ROUNDS: u32 = 5;

    let template = Template::get(&SCHEMA).await.expect("template builds");

    // One warm-up clone so the measurement excludes first-touch costs.
    template
        .fresh()
        .await
        .expect("clone succeeds")
        .cleanup()
        .await
        .expect("cleanup succeeds");

    let mut acquire = std::time::Duration::ZERO;
    let mut teardown = std::time::Duration::ZERO;
    for _ in 0..ROUNDS {
        let start = std::time::Instant::now();
        let db = template.fresh().await.expect("clone succeeds");
        acquire += start.elapsed();

        let start = std::time::Instant::now();
        db.cleanup().await.expect("cleanup succeeds");
        teardown += start.elapsed();
    }

    let per_acquire = acquire / ROUNDS;
    let per_teardown = teardown / ROUNDS;
    println!("acquire (clone + connect): {per_acquire:?}");
    println!("teardown (drop):           {per_teardown:?}");

    assert!(
        per_acquire < std::time::Duration::from_secs(2),
        "acquiring a database took {per_acquire:?}, which suggests the template \
         is being rebuilt per test rather than cloned"
    );
}

/// A crashed build must not leave a half-applied schema that later runs treat as
/// good — the one failure mode a template-based harness cannot have.
#[tokio::test]
async fn a_schema_that_fails_to_apply_does_not_leave_a_usable_template() {
    static BROKEN: Schema = Schema::sql(
        "harness-broken",
        &[
            "CREATE TABLE fine (id INT)",
            "CREATE TABLE oops (id INT) THIS IS NOT SQL",
        ],
    );

    assert!(
        Template::get(&BROKEN).await.is_err(),
        "a schema that fails to apply must not yield a template"
    );

    // And the partial database must have been cleaned up, so a retry starts
    // clean rather than finding a half-built database and trusting it.
    assert!(
        Template::get(&BROKEN).await.is_err(),
        "a second attempt must fail the same way, not succeed against residue"
    );
}
