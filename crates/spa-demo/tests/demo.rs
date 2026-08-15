//! **The demo tenant, as a required check.**
//!
//! The widest test in the system: it signs up through the public API with every
//! module enabled, fills the tenant the way a customer would, and then asks the
//! three questions no narrower test can.
//!
//! 1. Is every module actually working? — `every_module_is_enabled_and_answering`
//! 2. Does every group rebuild from the log to exactly what is live? —
//!    `the_demo_replays_to_exactly_what_is_live`, which is where architecture
//!    L2's claim stops being a claim.
//! 3. Is every invariant clean? — `the_demo_passes_every_invariant`
//!
//! The demo signs up for whatever `spa_api::modules()` offers, so a module added
//! to the API is one the demo enables without anyone remembering to. What still
//! needs a person is teaching the seeder to *use* it — and the first test is
//! where that omission surfaces, because each module is asked for something only
//! it can answer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use spa_api::AppState;
use spa_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use spa_demo::Seeded;
use spa_projection::{Projection, replay_shadow};
use spa_testkit::{Schema, TestDb};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
/// A database that has never run anything. What `just demo` is pointed at the
/// first time anyone tries it.
static EMPTY: Schema = Schema::sql("empty", &[]);

/// Long enough that the demo's own writes cannot collide with another test's.
const PASSWORD: &str = "demo-password-not-a-secret";

struct Demo {
    seeded: Seeded,
    state: AppState,
    _control_db: TestDb,
    tenant_database: String,
}

impl Demo {
    /// Builds the whole demo. Every test here starts from this.
    async fn build(slug: &str) -> Self {
        let control_db = spa_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &spa_testkit::database_url())
            .expect("the test database URL parses");

        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
        control
            .register_cluster(
                "primary",
                "SPA_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");

        let state = AppState::new(control);
        // No expiry: these tests drop their own databases, and a reaper running
        // in another test must not race them.
        let seeded = spa_demo::seed(&state, slug, PASSWORD, None)
            .await
            .expect("the demo builds");

        Self {
            tenant_database: seeded.database.clone(),
            seeded,
            state,
            _control_db: control_db,
        }
    }

    async fn get(&self, path: &str) -> serde_json::Value {
        spa_demo::get(
            &spa_api::router(self.state.clone()),
            path,
            &self.seeded.token,
        )
        .await
        .expect("the demo's own reads work")
    }

    /// A pool straight at the tenant database, for the shadow differ — an
    /// operator tool, not a request path.
    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = spa_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    async fn cleanup(self) {
        drop(self.state);
        let _ = spa_testkit::drop_named_database(&self.tenant_database).await;
    }
}

/// **The requirement, checked.** "The demo should have every module enabled and
/// working."
#[tokio::test]
async fn every_module_is_enabled_and_answering() {
    let demo = Demo::build("demo").await;

    let tenant = demo.get("/v1/tenants/demo").await;
    let mut reported: Vec<&str> = tenant["modules"]
        .as_array()
        .expect("a module list")
        .iter()
        .map(|m| m.as_str().expect("a name"))
        .collect();
    reported.sort_unstable();

    let mut expected = spa_demo::modules();
    expected.sort_unstable();

    assert_eq!(
        reported, expected,
        "a module the demo does not enable is a module nothing demonstrates"
    );

    // Enabled is not the same as working, so every module is asked for
    // something only it can answer.
    let accounts = demo.get("/v1/tenants/demo/ledger/accounts").await;
    assert!(
        accounts.as_array().expect("a list").len() > 10,
        "the ledger has a chart"
    );

    let invoices = demo.get("/v1/tenants/demo/sales/invoices").await;
    assert_eq!(
        invoices.as_array().expect("a list").len(),
        demo.seeded.invoices,
        "sales has its invoices"
    );

    demo.cleanup().await;
}

/// The demo has something in every state a screen has to render.
#[tokio::test]
async fn the_demo_shows_a_business_rather_than_a_row() {
    let demo = Demo::build("demo-shape").await;

    let invoices = demo.get("/v1/tenants/demo-shape/sales/invoices").await;
    let invoices = invoices.as_array().expect("a list");

    let settled = invoices.iter().filter(|i| i["outstanding"] == 0).count();
    let part_paid = invoices
        .iter()
        .filter(|i| i["paid"].as_i64() > Some(0) && i["outstanding"].as_i64() > Some(0))
        .count();
    let untouched = invoices.iter().filter(|i| i["paid"] == 0).count();

    assert!(settled >= 1, "something is paid off");
    assert!(part_paid >= 1, "something is part-paid");
    assert!(untouched >= 1, "something is still owed in full");

    // Every VAT treatment appears somewhere, so the tax breakdown on screen has
    // more than one row to show.
    let mut treatments: Vec<String> = Vec::new();
    for invoice in invoices {
        let id = invoice["id"].as_str().expect("an id");
        let detail = demo
            .get(&format!("/v1/tenants/demo-shape/sales/invoices/{id}"))
            .await;
        for band in detail["tax_breakdown"].as_array().expect("bands") {
            treatments.push(band["vat"].as_str().expect("a treatment").to_owned());
        }
    }
    for wanted in ["standard", "zero", "exempt"] {
        assert!(
            treatments.iter().any(|t| t == wanted),
            "no {wanted}-rated line anywhere: {treatments:?}"
        );
    }

    // A mistake and its correction, because that is what a prospective customer
    // asks about first and what a description is least convincing about.
    let credited = invoices
        .iter()
        .filter(|i| i["credit_note"].is_string())
        .count();
    assert_eq!(
        credited, demo.seeded.credited,
        "the demo shows an invoice put right, not only invoices that went well"
    );
    assert!(
        invoices
            .iter()
            .any(|i| i["credit_note"].is_string() && i["outstanding"] == 0),
        "and a credited invoice owes nothing"
    );

    // And the books are not made only of sales — an accounting demo in which
    // nothing was ever spent is not a demo of accounting.
    let accounts = demo.get("/v1/tenants/demo-shape/ledger/accounts").await;
    let expenses: i64 = accounts
        .as_array()
        .expect("a list")
        .iter()
        .filter(|a| a["kind"] == "expense")
        .filter_map(|a| a["balance"].as_i64())
        .sum();
    assert!(expenses > 0, "the business has costs");

    demo.cleanup().await;
}

/// **Shadow replay, in CI.** Architecture L2 says projections are pure functions
/// of the event stream. This is where that stops being a claim about code nobody
/// can check by reading it.
///
/// Every group, because a module added without one here would be the one that
/// silently is not reproducible.
#[tokio::test]
async fn the_demo_replays_to_exactly_what_is_live() {
    let demo = Demo::build("demo-replay").await;
    let pool = demo.tenant_pool().await;

    let owned = ledger::projections();
    let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
        owned.iter().map(AsRef::as_ref).collect();
    let ledger_report = replay_shadow::<ledger::Ledger>(&pool, &refs, ledger::upcasters(), 500)
        .await
        .expect("the ledger replays");

    let owned = sales::projections();
    let refs: Vec<&dyn Projection<Group = sales::Sales>> =
        owned.iter().map(AsRef::as_ref).collect();
    let sales_report = replay_shadow::<sales::Sales>(&pool, &refs, sales::upcasters(), 500)
        .await
        .expect("sales replays");

    pool.close().await;

    // Not vacuous: a report over an empty log is trivially reproducible, so the
    // position both were compared at has to be somewhere.
    assert!(
        ledger_report.position.get() > 20,
        "the demo should have written a substantial log, got {}",
        ledger_report.position
    );

    assert!(
        ledger_report.is_reproducible(),
        "the ledger does not rebuild to what is live: {:?}",
        ledger_report.differences()
    );
    assert!(
        sales_report.is_reproducible(),
        "sales does not rebuild to what is live: {:?}",
        sales_report.differences()
    );

    demo.cleanup().await;
}

/// Every invariant the platform checks per tenant, against the demo.
///
/// The trial balance is the one that matters: it can only be zero if commands,
/// events, both projections and the read models are all right.
#[tokio::test]
async fn the_demo_passes_every_invariant() {
    let demo = Demo::build("demo-invariants").await;

    let db = demo
        .state
        .control
        .enter_for_maintenance(demo.seeded.tenant)
        .await
        .expect("maintenance entry");
    let mut conn = db.acquire().await.expect("connection");

    let balance = ledger::trial_balance(&mut conn).await.expect("reads");
    assert!(!balance.is_empty(), "there should be something to balance");
    assert!(
        balance.iter().all(ledger::TrialBalance::balances),
        "the demo's books do not balance: {balance:?}"
    );

    let overpaid = sales::overpaid(&mut conn).await.expect("reads");
    assert!(overpaid.is_empty(), "an invoice is overpaid: {overpaid:?}");

    let outbox = spa_eventlog::outbox_health(&mut conn).await.expect("reads");
    assert_eq!(outbox.dead, 0, "the demo dead-lettered an effect");

    drop(conn);
    drop(db);
    demo.cleanup().await;
}

/// The demo is reachable the way a person would reach it: sign in with the
/// printed credentials and read the books.
#[tokio::test]
async fn the_demo_can_be_signed_into_afterwards() {
    let demo = Demo::build("demo-login").await;
    let app = spa_api::router(demo.state.clone());

    let response = tower::ServiceExt::oneshot(
        app.clone(),
        axum::http::Request::post("/v1/sessions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "handle": demo.seeded.email, "password": PASSWORD })
                    .to_string(),
            ))
            .unwrap(),
    )
    .await
    .expect("responds");

    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let token = body["token"].as_str().expect("a token");

    let trial_balance = spa_demo::get(&app, "/v1/tenants/demo-login/ledger/trial-balance", token)
        .await
        .expect("reads");
    assert!(
        trial_balance
            .as_array()
            .expect("a list")
            .iter()
            .all(|row| row["balances"] == true),
        "{trial_balance}"
    );

    demo.cleanup().await;
}

/// **The demo bootstraps a database nobody prepared.**
///
/// Regression: `just demo` failed on a fresh checkout with
/// `relation "cluster" does not exist`, because the binary assumed a migrated
/// control plane. `ControlPlane::migrate` had existed the whole time and
/// nothing called it — the same shape of bug as `request_visit`, and the same
/// reason: a method with no caller is a method with no test.
#[tokio::test]
async fn the_demo_bootstraps_a_database_nobody_prepared() {
    let control_db = spa_testkit::Template::get(&EMPTY)
        .await
        .expect("empty template builds")
        .fresh()
        .await
        .expect("empty database clones");

    // Not vacuous: assert the database really is bare before bootstrapping it,
    // or this passes against a template that quietly had the schema all along.
    let exists: Option<bool> =
        sqlx::query_scalar("SELECT to_regclass('public.cluster') IS NOT NULL")
            .fetch_one(control_db.pool())
            .await
            .expect("reads");
    assert_eq!(exists, Some(false), "the fixture is supposed to be empty");

    let clusters = ClusterRegistry::new()
        .with_url("primary", &spa_testkit::database_url())
        .expect("the test database URL parses");
    let control = Arc::new(ControlPlane::new(
        control_db.pool().clone(),
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    spa_demo::bootstrap(&control, "primary", "SPA_CLUSTER_PRIMARY_URL")
        .await
        .expect("bootstraps");
    // Twice, because a demo is often re-run against a live deployment.
    spa_demo::bootstrap(&control, "primary", "SPA_CLUSTER_PRIMARY_URL")
        .await
        .expect("bootstrapping is idempotent");

    let state = AppState::new(control);
    let seeded = spa_demo::seed(&state, "demo-bootstrap", PASSWORD, None)
        .await
        .expect("the demo builds on a database it prepared itself");

    // Six invoices, one of them credited — the whole seed, not a truncated one.
    assert_eq!(seeded.invoices, 6);
    assert_eq!(seeded.credited, 1);

    let _ = spa_testkit::drop_named_database(&seeded.database).await;
}

/// The demo has two people in it, with different jobs — which is the only way a
/// permissions model is visible at all.
#[tokio::test]
async fn the_demo_has_somebody_who_cannot_do_everything() {
    let demo = Demo::build("demo-people").await;
    let app = spa_api::router(demo.state.clone());

    let sign_in = |handle: String| {
        axum::http::Request::post("/v1/sessions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "handle": handle, "password": PASSWORD }).to_string(),
            ))
            .unwrap()
    };

    let response = tower::ServiceExt::oneshot(app.clone(), sign_in(demo.seeded.colleague.clone()))
        .await
        .expect("responds");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let sara = body["token"].as_str().expect("a token");

    // She can see the invoices she is responsible for...
    spa_demo::get(&app, "/v1/tenants/demo-people/sales/invoices", sara)
        .await
        .expect("sales is her job");

    // ...and cannot restructure the chart of accounts.
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post("/v1/tenants/demo-people/ledger/accounts")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {sara}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "code": "9999", "name": "Nope", "kind": "asset", "currency": "SAR"
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await
    .expect("responds");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::FORBIDDEN,
        "the books are not"
    );

    demo.cleanup().await;
}
