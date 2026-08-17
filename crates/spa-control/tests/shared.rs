//! Two nodes, one Redis, and the two things that only matter when there are two.
//!
//! Needs a Redis at `REDIS_URL` (default `redis://127.0.0.1/`), the same way
//! every other test here needs a Postgres. A missing one **fails** rather than
//! skips: a test that quietly does not run is worse than no test, and this file
//! exists precisely because the single-node behaviour looked fine.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use spa_control::shared::{Invalidate, SESSION_TTL, Shared};
use spa_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, Role, Scope, TenantPools};
use spa_testkit::{Schema, TestDb};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);

fn redis_url() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_owned())
}

async fn shared() -> Shared {
    Shared::connect(&redis_url()).await.unwrap_or_else(|e| {
        panic!(
            "these tests need a Redis at {} — set REDIS_URL or start one: {e}",
            redis_url()
        )
    })
}

/// Two control planes over **one** control database, as two API replicas are.
struct TwoNodes {
    a: Arc<ControlPlane>,
    b: Arc<ControlPlane>,
    _db: TestDb,
}

impl TwoNodes {
    async fn new() -> Self {
        let db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");

        let node = |shared: Shared| {
            let clusters = ClusterRegistry::new()
                .with_url("primary", &spa_testkit::database_url())
                .expect("parses");
            Arc::new(
                ControlPlane::new(
                    db.pool().clone(),
                    TenantPools::new(clusters, PoolConfig::default()),
                )
                .sharing(shared),
            )
        };

        let a = node(shared().await);
        let b = node(shared().await);
        a.register_cluster(
            "primary",
            "SPA_CLUSTER_PRIMARY_URL",
            None,
            10_000,
            10_000,
            Actor::system(),
        )
        .await
        .expect("registers");
        spa_control::shared::apply_invalidations_in_background(&a);
        spa_control::shared::apply_invalidations_in_background(&b);

        // The subscribers connect asynchronously; a publish before either is
        // listening is a message nobody receives, which would make this file
        // flaky rather than wrong.
        tokio::time::sleep(Duration::from_millis(300)).await;

        Self { a, b, _db: db }
    }
}

/// **The window this exists to close.**
///
/// Node A demotes somebody. Node B has their old role cached and, without a
/// broadcast, keeps serving it until its TTL lapses — five seconds of a former
/// accountant still being an accountant, on every node that did not handle the
/// change.
///
/// Reverting `ControlPlane::forget` to a local `invalidate` fails this.
#[tokio::test]
async fn a_role_change_on_one_node_reaches_the_others() {
    let nodes = TwoNodes::new().await;

    let tenant = nodes
        .a
        .register_tenant_on("acme", "Acme", "primary", Actor::system())
        .await
        .expect("tenant");
    let person = nodes
        .a
        .create_identity(Actor::system())
        .await
        .expect("identity");
    nodes
        .a
        .grant_membership(
            person.id,
            Scope::Tenant(tenant.id),
            "accountant",
            Actor::system(),
        )
        .await
        .expect("membership");

    // **B caches it.** This is the step that makes the test mean something: an
    // empty cache would pick up the change from the database no matter what.
    let before = nodes
        .b
        .access(person.id, tenant.id)
        .await
        .expect("reads")
        .expect("a membership");
    assert_eq!(before.role, Role::Accountant);

    // A demotes them. `change_role`, not `grant_membership`: the latter
    // deliberately refuses to change a *live* member's role, because doing so
    // would be a way around the last-owner guard.
    nodes
        .a
        .change_role(tenant.id, person.id, Role::Viewer, Actor::system())
        .await
        .expect("demotes");

    // Redis delivers asynchronously — well inside the five-second TTL this is
    // beating, which is the entire claim.
    let mut role = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        role = nodes
            .b
            .access(person.id, tenant.id)
            .await
            .expect("reads")
            .map(|access| access.role);
        if role == Some(Role::Viewer) {
            break;
        }
    }
    assert_eq!(
        role,
        Some(Role::Viewer),
        "node B still believes a demoted person is an accountant"
    );

    let _ = spa_testkit::drop_named_database(&tenant.database_name).await;
}

/// **A logout on one node ends the session on every node.**
///
/// The reason `session` was never cached in process: an in-process cache would
/// make a logout take effect where it was served and nowhere else, and a stale
/// logout is not a survivable kind of stale. Shared, it is.
#[tokio::test]
async fn a_logout_on_one_node_ends_the_session_on_the_other() {
    let nodes = TwoNodes::new().await;

    let person = nodes
        .a
        .create_identity(Actor::system())
        .await
        .expect("identity");
    nodes
        .a
        .register_login(
            person.id,
            "someone@acme.test".to_owned(),
            "hunter2hunter2".to_owned(),
        )
        .await
        .expect("login");
    let (token, _) = nodes
        .a
        .log_in("someone@acme.test", "hunter2hunter2")
        .await
        .expect("signs in");

    // Both nodes read it, so both have it cached — B from Redis, having never
    // seen this token before.
    assert!(nodes.a.session(token.expose()).await.is_ok());
    assert!(
        nodes.b.session(token.expose()).await.is_ok(),
        "node B could not read a session node A created"
    );

    nodes.a.log_out(token.expose()).await.expect("logs out");

    assert!(
        nodes.b.session(token.expose()).await.is_err(),
        "node B still accepts a token that was logged out on node A — the \
         session cache outlived the session"
    );
}

/// Suspending somebody has to end every session they hold, not just the one in
/// front of us.
#[tokio::test]
async fn logging_out_everywhere_clears_every_cached_session() {
    let nodes = TwoNodes::new().await;

    let person = nodes
        .a
        .create_identity(Actor::system())
        .await
        .expect("identity");
    nodes
        .a
        .register_login(
            person.id,
            "many@acme.test".to_owned(),
            "hunter2hunter2".to_owned(),
        )
        .await
        .expect("login");

    // Three sessions, as three devices would be, each cached on both nodes.
    let mut tokens = Vec::new();
    for _ in 0..3 {
        let (token, _) = nodes
            .a
            .log_in("many@acme.test", "hunter2hunter2")
            .await
            .expect("signs in");
        assert!(nodes.b.session(token.expose()).await.is_ok());
        tokens.push(token);
    }

    let ended = nodes
        .a
        .log_out_everywhere(person.id)
        .await
        .expect("ends them");
    assert_eq!(ended, 3);

    for token in &tokens {
        assert!(
            nodes.b.session(token.expose()).await.is_err(),
            "a suspended person's session survived on another node"
        );
    }
}

/// The cache is a cache: what it answers has to be what the database says.
#[tokio::test]
async fn a_cached_session_says_the_same_thing_the_database_does() {
    let nodes = TwoNodes::new().await;

    let person = nodes
        .a
        .create_identity(Actor::system())
        .await
        .expect("identity");
    nodes
        .a
        .register_login(
            person.id,
            "same@acme.test".to_owned(),
            "hunter2hunter2".to_owned(),
        )
        .await
        .expect("login");
    let (token, from_login) = nodes
        .a
        .log_in("same@acme.test", "hunter2hunter2")
        .await
        .expect("signs in");

    let uncached = nodes.b.session(token.expose()).await.expect("first read");
    let cached = nodes.b.session(token.expose()).await.expect("second read");

    assert_eq!(
        uncached, cached,
        "the cached answer differs from the stored one"
    );
    assert_eq!(cached.identity, person.id);
    assert_eq!(cached.identity, from_login.identity);
    assert!(
        SESSION_TTL <= Duration::from_mins(5),
        "SESSION_TTL is the blast radius of a failed logout; it must stay short"
    );
}

/// An invalidation this build cannot read must not be applied as something else.
///
/// During a rolling deploy two builds are live at once. A message from a newer
/// node has to be a loud failure on an older one, not a silently ignored one —
/// silently ignored is a node serving stale authorization with nothing in the
/// log to say so.
#[tokio::test]
async fn an_unreadable_invalidation_is_not_guessed_at() {
    let known = serde_json::to_string(&Invalidate::Identity(spa_types::IdentityId::new()))
        .expect("serializes");
    assert!(serde_json::from_str::<Invalidate>(&known).is_ok());

    for unknown in [
        r#"{"what":"something_new","which":"01a00000-0000-7000-8000-000000000000"}"#,
        r#"{"what":"identity"}"#,
        "not json at all",
    ] {
        assert!(
            serde_json::from_str::<Invalidate>(unknown).is_err(),
            "{unknown} was accepted as an invalidation"
        );
    }
}
