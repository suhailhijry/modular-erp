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
//! The demo signs up for whatever `erp_api::modules()` offers, so a module added
//! to the API is one the demo enables without anyone remembering to. What still
//! needs a person is teaching the seeder to *use* it — and the first test is
//! where that omission surfaces, because each module is asked for something only
//! it can answer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_api::AppState;
use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use erp_demo::Seeded;
use erp_projection::{Projection, replay_shadow};
use erp_testkit::{Schema, TestDb};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
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
        let control_db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("control template builds")
            .fresh()
            .await
            .expect("control database clones");

        let clusters = ClusterRegistry::new()
            .with_url("primary", &erp_testkit::database_url())
            .expect("the test database URL parses");

        let control = Arc::new(ControlPlane::new(
            control_db.pool().clone(),
            TenantPools::new(clusters, PoolConfig::default()),
        ));
        control
            .register_cluster(
                "primary",
                "ERP_CLUSTER_PRIMARY_URL",
                None,
                10_000,
                10_000,
                Actor::system(),
            )
            .await
            .expect("cluster registers");

        let state = erp_demo::with_deployment(AppState::new(control)).expect("deployment");
        // No expiry: these tests drop their own databases, and a reaper running
        // in another test must not race them.
        let seeded = erp_demo::seed(&state, slug, PASSWORD, None)
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
        erp_demo::get(
            &erp_api::router(self.state.clone()),
            &self.seeded.slug,
            path,
            &self.seeded.token,
        )
        .await
        .expect("the demo's own reads work")
    }

    /// A pool straight at the tenant database, for the shadow differ — an
    /// operator tool, not a request path.
    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    async fn cleanup(self) {
        drop(self.state);
        let _ = erp_testkit::drop_named_database(&self.tenant_database).await;
    }
}

/// **The requirement, checked.** "The demo should have every module enabled and
/// working."
#[tokio::test]
async fn every_module_is_enabled_and_answering() {
    let demo = Demo::build("demo").await;

    let tenant = demo.get("/v1/tenant").await;
    let mut reported: Vec<&str> = tenant["modules"]
        .as_array()
        .expect("a module list")
        .iter()
        .map(|m| m.as_str().expect("a name"))
        .collect();
    reported.sort_unstable();

    let mut expected = erp_demo::modules();
    expected.sort_unstable();

    assert_eq!(
        reported, expected,
        "a module the demo does not enable is a module nothing demonstrates"
    );

    // Enabled is not the same as working, so every module is asked for
    // something only it can answer.
    let accounts = demo.get("/v1/ledger/accounts").await;
    assert!(
        accounts.as_array().expect("a list").len() > 10,
        "the ledger has a chart"
    );

    // **The till's sales are in here too**, and that is `pos`'s whole claim:
    // a counter transaction is a `sales` invoice, so there is one place that
    // answers "what did we sell" and the VAT return does not have to choose.
    let invoices = demo.get("/v1/sales/invoices").await["items"].clone();
    assert_eq!(
        invoices.as_array().expect("a list").len(),
        demo.seeded.invoices + demo.seeded.till_sales,
        "sales has its invoices, the counter's among them"
    );

    let diary = demo.get("/v1/booking/reservations").await["items"].clone();
    assert_eq!(
        diary.as_array().expect("a list").len(),
        demo.seeded.reservations,
        "booking has its diary"
    );
    // A rota that draws only one kind of thing demonstrates only one trade, and
    // the claim this module makes is that a stylist and a room type are the
    // same code. Both are in there, and one of the rooms has been given out.
    let rota = demo.get("/v1/booking/resources").await["items"].clone();
    assert_eq!(
        rota.as_array().expect("a list").len(),
        demo.seeded.bookables,
        "booking has its rota"
    );
    // Both recognition models, in one tenant, because the claim `prepaid` makes
    // is that they are not interchangeable.
    let held = demo.get("/v1/prepaid/entitlements").await["items"].clone();
    assert_eq!(
        held.as_array().expect("a list").len(),
        1,
        "prepaid has its packages"
    );
    let package = &held.as_array().expect("a list")[0];
    assert_eq!(package["uses_left"], 8, "two of ten sessions were used");
    assert_eq!(
        package["outstanding"]["minor"], 400_000,
        "the package earned two sessions"
    );

    // A loyalty card, whose liability is the only one here computed as a
    // fraction of a sale rather than the whole of one.
    let card = demo
        .get(&format!(
            "/v1/prepaid/cards/{}",
            erp_demo::demo_id("CARD-0001")
        ))
        .await;
    assert_eq!(card["counts"], 60, "a hundred earned, forty spent");
    assert_eq!(card["lifetime"], 100, "spending points cost a rank");
    assert_eq!(
        card["deferred"]["minor"], 549,
        "9.09 deferred against a hundred riyals, less the 3.60 honoured"
    );

    the_counter_counted(&demo).await;
    the_shelf_was_counted(&demo).await;

    let membership = demo
        .get(&format!(
            "/v1/prepaid/subscriptions/{}",
            erp_demo::demo_id("SUB-0001")
        ))
        .await;
    assert!(
        membership["recognised"]["minor"]
            .as_i64()
            .is_some_and(|earned| earned > 0),
        "the membership earned nothing"
    );
    assert!(
        membership["outstanding"]["minor"]
            .as_i64()
            .is_some_and(|owed| owed > 0),
        "the membership earned the whole year at once"
    );
    // The freeze pushed the term past its original end.
    assert_ne!(
        membership["ends_at"], "2027-01-01T00:00:00Z",
        "a freeze did not move the term"
    );

    let stay = demo
        .get(&format!(
            "/v1/booking/reservations/{}",
            erp_demo::demo_id("BK-0006")
        ))
        .await;
    assert_eq!(
        stay["lines"][0]["unit"], "room-201",
        "the pooled booking never had a unit assigned to it"
    );

    demo.cleanup().await;
}

/// The demo has something in every state a screen has to render.
#[tokio::test]
async fn the_demo_shows_a_business_rather_than_a_row() {
    let demo = Demo::build("demo-shape").await;

    let invoices = demo.get("/v1/sales/invoices").await["items"].clone();
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
        let detail = demo.get(&format!("/v1/sales/invoices/{id}")).await;
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

    // The other side of the return. A demo that shows output tax and calls it a
    // VAT return shows half a number, and the wrong half to somebody deciding
    // whether this can file for them.
    //
    // This also covers the demo's own projection list: a module the demo signs
    // up for and never advances has empty read models, and reads as "the demo is
    // broken" rather than "somebody forgot a line".
    let bills = demo.get("/v1/purchases/bills").await["items"].clone();
    let bills = bills.as_array().expect("a list");
    assert_eq!(
        bills.len(),
        demo.seeded.bills,
        "the demo signed up for purchases and its bills were never projected"
    );
    assert!(
        bills.iter().any(|b| b["outstanding"] == 0),
        "something is paid off"
    );
    assert!(
        bills.iter().any(|b| b["outstanding"] != 0),
        "and something is still owed, so a payables list has both"
    );

    // The whole return: charged, reclaimed, and the difference.
    let filed = demo
        .get(
            "/v1/tax_sa/vat-return\
             ?from=2026-01-01&until=2026-04-01&currency=SAR",
        )
        .await;
    let output = filed["output"]["tax"].as_i64().expect("output tax");
    let input = filed["input"]["tax"].as_i64().expect("input tax");
    assert!(output > 0, "the demo charged VAT");
    assert!(input > 0, "and paid some it can reclaim");
    assert_eq!(
        filed["payable"].as_i64(),
        Some(output - input),
        "the number that gets filed is the difference"
    );
    assert!(
        filed["input"]["bands"]
            .as_array()
            .expect("bands")
            .iter()
            .any(|b| b["vat"] == "exempt" && b["tax"] == 0),
        "the demo has an exempt purchase reclaiming nothing — the distinction \
         between zero-rated and exempt is invisible until a return has both"
    );

    // And the books are not made only of sales — an accounting demo in which
    // nothing was ever spent is not a demo of accounting.
    let accounts = demo.get("/v1/ledger/accounts").await;
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

/// **A quarter, declared.**
///
/// A tax module nobody has filed with demonstrates an arithmetic exercise rather
/// than the thing being bought. This is also what proves the module reached the
/// demo at all: filing goes through the public API and reads its own write back.
#[tokio::test]
async fn the_demo_has_filed_a_vat_return() {
    let demo = Demo::build("demo-tax").await;

    let returns = demo.get("/v1/tax_sa/returns").await;
    let returns = returns.as_array().expect("a list");
    assert_eq!(returns.len(), demo.seeded.filed, "the demo filed nothing");

    let filed = &returns[0];
    assert_eq!(filed["period"], "SAR.2026-01-01.2026-04-01");
    assert_eq!(
        filed["payable"].as_i64(),
        Some(
            filed["output_tax"].as_i64().unwrap_or_default()
                - filed["input_tax"].as_i64().unwrap_or_default()
        ),
        "what was filed does not add up"
    );
    assert!(
        filed["output_tax"].as_i64().is_some_and(|t| t > 0),
        "a filing with no output tax is not a demo of a business"
    );

    demo.cleanup().await;
}

/// Rebuilds one group into a shadow schema and diffs it against what is live.
///
/// The witness table is what makes the result mean something. `EXCEPT ALL`
/// between two **empty** tables is clean, so a group whose read models happen to
/// be empty is "reproducible" in the way that a blank page is correct. Each
/// group therefore names a table the demo must have filled, and this refuses to
/// report on a group that did no work.
macro_rules! replay {
    ($pool:expr, $module:ident, $group:ty, $witness:literal) => {{
        let schema = <$group as erp_projection::ProjectionGroup>::SCHEMA;
        // Both halves are compile-time constants — a group's `SCHEMA` const and
        // a literal at the call site below — so there is nothing here for a
        // caller to inject into.
        let counted = sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {schema}.{}", $witness));
        let rows: i64 = sqlx::query_scalar(counted)
            .fetch_one(&$pool)
            .await
            .unwrap_or_else(|e| panic!("{schema}.{} is not readable: {e}", $witness));
        assert!(
            rows > 0,
            "{schema}.{} is empty, so replaying {} proves nothing",
            $witness,
            stringify!($module),
        );

        let owned = $module::projections();
        let refs: Vec<&dyn Projection<Group = $group>> = owned.iter().map(AsRef::as_ref).collect();
        replay_shadow::<$group>(&$pool, &refs, $module::upcasters(), 500)
            .await
            .unwrap_or_else(|e| panic!("{} replays: {e}", stringify!($module)))
    }};
}

/// **Shadow replay.** Architecture L2 says projections are pure functions of the
/// event stream. This is where that stops being a claim about code nobody can
/// check by reading it.
///
/// # Every group, and the check that it is every group
///
/// This used to replay `ledger` and `sales` while claiming to cover everything.
/// `purchases` and `tax_sa` had projection groups and neither was ever rebuilt —
/// and `tax_sa` is the one where it matters most: its projection builds the
/// ZATCA hash chain, where each document carries the hash of the one before it.
/// A rebuild that produces a different document produces a different hash, and
/// breaks a chain **the tax authority validates**. The group nobody was watching
/// was the group with the most to lose.
///
/// So the list is no longer trusted to be complete. Every group every module
/// declares has to appear in it, and a module added to `erp_api::modules()`
/// without a line here fails this test rather than becoming the next one that
/// silently does not rebuild.
#[tokio::test]
async fn the_demo_replays_to_exactly_what_is_live() {
    let demo = Demo::build("demo-replay").await;
    let pool = demo.tenant_pool().await;

    let replays = vec![
        replay!(pool, booking, booking::Booking, "reservation"),
        replay!(pool, crm, crm::Crm, "customer"),
        replay!(pool, prepaid, prepaid::Prepaid, "entitlement"),
        replay!(pool, pos, pos::Pos, "shift"),
        replay!(pool, inventory, inventory::Inventory, "stock_item"),
        replay!(pool, branches, branches::Branches, "branch"),
        replay!(pool, hr, hr::Hr, "employee"),
        replay!(pool, payroll, payroll::Payroll, "run"),
        replay!(pool, ledger, ledger::Ledger, "account"),
        replay!(pool, sales, sales::Sales, "invoice"),
        replay!(pool, purchases, purchases::Purchases, "bill"),
        replay!(pool, tax_sa, tax_sa::TaxSa, "zatca_document"),
        replay!(pool, reports, reports::Reports, "revenue"),
        replay!(pool, files, files::Files, "file"),
        replay!(pool, payments, payments::Payments, "payment"),
        replay!(pool, notifications, notifications::Notifications, "inbox"),
        replay!(
            pool,
            conversations,
            conversations::Conversations,
            "conversation_message"
        ),
    ];

    pool.close().await;

    // **Nothing was left out.** The demo enables every module, so every group
    // every module owns exists in this tenant and must have been rebuilt.
    let replayed: std::collections::BTreeSet<&str> =
        replays.iter().map(|report| report.group).collect();
    let declared: std::collections::BTreeSet<&str> = erp_api::modules()
        .iter()
        .flat_map(|(_, setup)| setup.groups.iter().map(|(name, _, _)| *name))
        .collect();
    assert_eq!(
        replayed, declared,
        "a projection group is not covered by shadow replay"
    );

    // A report over an empty log is trivially reproducible, so the position they
    // were all compared at has to be somewhere.
    assert!(
        replays[0].position.get() > 20,
        "the demo should have written a substantial log, got {}",
        replays[0].position
    );

    for report in &replays {
        assert!(
            report.is_reproducible(),
            "{} does not rebuild to what is live: {:?}",
            report.group,
            report.differences()
        );
    }

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

    // **§10b, against a whole business rather than a fixture.** Every figure
    // the reports module offers is built from the same log the ledger is, and
    // this asserts they agree — over a demo that issues invoices, credits some,
    // rings up till sales, takes deposits and runs payroll.
    //
    // A discrepancy is a failure, not a coloured cell (L6), and this is where
    // that becomes a failing build rather than a claim in a doc comment.
    let discrepancies = reports::reconciles(&mut conn).await.expect("reads");
    assert!(
        discrepancies.is_empty(),
        "the demo's figures do not agree with its books: {:?}",
        discrepancies
            .iter()
            .map(reports::Discrepancy::describe)
            .collect::<Vec<_>>()
    );

    // **Decision R3's invariant**, which is `StockValueAgrees` in `bin/worker`
    // said here: what the shelves are worth against the account that says so.
    // `inventory` writes `1300` at both ends now — a receipt debits it, every
    // movement out credits it — so this catches a movement that did not post
    // or an entry somebody made against the stock account by hand, and it is
    // quiet across the delivery-then-bill window that used to make it shout.
    let held: i64 = inventory::value_on_hand(&mut conn)
        .await
        .expect("reads")
        .iter()
        .map(|money| money.minor())
        .sum();
    let account = inventory::PostingAccounts::conventional().inventory;
    let carried = ledger::account_balances(&mut conn)
        .await
        .expect("reads")
        .into_iter()
        .find(|a| a.code == account.as_str())
        .map_or(0, |a| a.balance.minor());
    assert_eq!(
        carried, held,
        "the demo's books and its shelves disagree about what the stock is worth"
    );

    let outbox = erp_eventlog::outbox_health(&mut conn).await.expect("reads");
    assert_eq!(outbox.dead, 0, "the demo dead-lettered an effect");

    // **Every document has a place in the ZATCA chain.** The demo registers
    // before it issues anything, so nothing should be `unregistered` — and a
    // gap in the chain is the failure that would be invisible until a tax
    // authority found it.
    let documents = tax_sa::documents(&mut conn, 500, None)
        .await
        .expect("reads")
        .items;
    assert_eq!(
        documents.len(),
        demo.seeded.invoices + demo.seeded.till_sales + demo.seeded.credited,
        "every invoice and every credit note is a ZATCA document"
    );
    let mut positions: Vec<i64> = documents.iter().filter_map(|d| d.icv).collect();
    positions.sort_unstable();
    assert_eq!(
        positions,
        (1..=i64::try_from(documents.len()).unwrap_or_default()).collect::<Vec<_>>(),
        "the chain has a gap or a repeat in it"
    );
    assert!(
        documents
            .iter()
            .all(|d| d.status == tax_sa::Status::Pending),
        "the demo registered before it issued, so nothing should be unregistered"
    );
    assert!(
        documents
            .iter()
            .all(|d| d.qr.as_deref().is_some_and(|qr| !qr.is_empty())),
        "a document with no QR is one that cannot be printed"
    );

    drop(conn);
    drop(db);
    demo.cleanup().await;
}

/// The demo is reachable the way a person would reach it: sign in with the
/// printed credentials and read the books.
#[tokio::test]
async fn the_demo_can_be_signed_into_afterwards() {
    let demo = Demo::build("demo-login").await;
    let app = erp_api::router(demo.state.clone());

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

    let trial_balance = erp_demo::get(&app, &demo.seeded.slug, "/v1/ledger/trial-balance", token)
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
    let control_db = erp_testkit::Template::get(&EMPTY)
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
        .with_url("primary", &erp_testkit::database_url())
        .expect("the test database URL parses");
    let control = Arc::new(ControlPlane::new(
        control_db.pool().clone(),
        TenantPools::new(clusters, PoolConfig::default()),
    ));

    erp_demo::bootstrap(&control, "primary", "ERP_CLUSTER_PRIMARY_URL", 100)
        .await
        .expect("bootstraps");
    // Twice, because a demo is often re-run against a live deployment.
    erp_demo::bootstrap(&control, "primary", "ERP_CLUSTER_PRIMARY_URL", 100)
        .await
        .expect("bootstrapping is idempotent");

    let state = erp_demo::with_deployment(AppState::new(control)).expect("deployment");
    let seeded = erp_demo::seed(&state, "demo-bootstrap", PASSWORD, None)
        .await
        .expect("the demo builds on a database it prepared itself");

    // Six invoices, one of them credited — the whole seed, not a truncated one.
    assert_eq!(seeded.invoices, 6);
    assert_eq!(seeded.credited, 1);

    let _ = erp_testkit::drop_named_database(&seeded.database).await;
}

/// **The demo starts no payment the gateway never issued.** It used to record a
/// Moyasar payment as already started under an invented id, which a worker
/// asked Moyasar about on every pass, for ever. What it records now is a charge
/// *requested* against a saved card — a state this system owns, that a worker
/// with no credentials leaves alone and one with credentials tries once.
#[tokio::test]
async fn the_demo_starts_no_payment_the_gateway_never_issued() {
    let demo = Demo::build("demo-requested").await;
    let pool = demo.tenant_pool().await;

    let stages: Vec<(String, String)> =
        sqlx::query_as("SELECT stage, provider FROM proj_payments.payment ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("reads");
    pool.close().await;

    assert!(!stages.is_empty(), "the demo asks for a gateway payment");
    assert!(
        stages.iter().all(|(stage, _)| stage == "requested"),
        "a payment claims a gateway state nobody at the gateway ever saw: {stages:?}"
    );

    demo.cleanup().await;
}

/// The demo has two people in it, with different jobs — which is the only way a
/// permissions model is visible at all.
#[tokio::test]
async fn the_demo_has_somebody_who_cannot_do_everything() {
    let demo = Demo::build("demo-people").await;
    let app = erp_api::router(demo.state.clone());

    // **Her password is her own**, minted for this demo: the owner's is the
    // one on the screen while the permissions model is being shown.
    assert_ne!(demo.seeded.colleague_password, PASSWORD);
    let sign_in = |handle: String, password: String| {
        axum::http::Request::post("/v1/sessions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "handle": handle, "password": password }).to_string(),
            ))
            .unwrap()
    };

    let refused = tower::ServiceExt::oneshot(
        app.clone(),
        sign_in(demo.seeded.colleague.clone(), PASSWORD.to_owned()),
    )
    .await
    .expect("responds");
    assert_eq!(
        refused.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "the owner's password does not open the colleague's account"
    );

    let response = tower::ServiceExt::oneshot(
        app.clone(),
        sign_in(
            demo.seeded.colleague.clone(),
            demo.seeded.colleague_password.clone(),
        ),
    )
    .await
    .expect("responds");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let sara = body["token"].as_str().expect("a token");

    // She can see the invoices she is responsible for...
    erp_demo::get(&app, &demo.seeded.slug, "/v1/sales/invoices", sara)
        .await
        .expect("sales is her job");

    // ...and cannot restructure the chart of accounts.
    let response = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::post("/v1/ledger/accounts")
            .header(
                axum::http::header::HOST,
                format!("{}.localhost", demo.seeded.slug),
            )
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

/// The counter: a shift that was counted, and came up short.
///
/// Split out because the test above was one line over the limit. It is also a
/// separate claim: the others are that a module answers, this is that the till
/// reconciled — and that the number it reconciled to is not zero, because a
/// demo where the drawer always balances shows nothing about the feature.
async fn the_counter_counted(demo: &Demo) {
    // Two places, and the till's takings attributable to one of them — which is
    // what a branch is for, and what one branch could not demonstrate.
    let places = demo.get("/v1/branches").await["items"].clone();
    assert_eq!(
        places.as_array().expect("a list").len(),
        demo.seeded.branches,
        "branches has its places"
    );

    let shift = demo
        .get(&format!(
            "/v1/pos/shifts/{}",
            erp_demo::demo_id("SHIFT-0001")
        ))
        .await;
    assert_eq!(shift["sales_count"], 4);
    assert_eq!(
        shift["variance"]["minor"], -50,
        "the demo's drawer is fifty halalas short, on purpose"
    );
    assert_eq!(shift["expected"]["minor"], 23_645);
}

/// **The shelf, and the number that says the quantity is honest.**
///
/// Four things the café keeps, in all three ways of keeping them: beans in two
/// dated roasts, a crate of milk that went off and was thrown out, a tray of
/// pastry three short of what the books say, and two grinders with numbers on
/// them of which one arrived damaged.
///
/// The canary is the last assertion, and it holds **lot by lot**: what is on a
/// batch is the sum of the movements against it. Nothing reconciles those two;
/// the projection writes a signed delta per portion and the lot's remainder is
/// those deltas added up, so a disagreement means the read model and the log
/// have parted company.
async fn the_shelf_was_counted(demo: &Demo) {
    let products = demo.get("/v1/inventory/products").await["items"].clone();
    assert_eq!(
        products.as_array().expect("a list").len(),
        demo.seeded.products,
        "inventory has its products"
    );
    assert_eq!(
        products
            .as_array()
            .expect("a list")
            .iter()
            .filter(|p| p["tracking"] == "lot")
            .count(),
        2,
        "the two that arrive in dated batches"
    );

    let shelves = demo.get("/v1/inventory/stock").await["items"].clone();
    let shelves = shelves.as_array().expect("a list");
    let on = |name: &str| {
        shelves
            .iter()
            .find(|row| row["product"] == erp_demo::demo_id(name))
            .expect("the product is on a shelf")
    };

    assert_eq!(on("PROD-BEANS")["on_hand"], 10_000, "five kilos twice");
    assert_eq!(
        on("PROD-BEANS")["value"]["minor"],
        70_000,
        "300.00 and 400.00, each roast carried at what it cost"
    );
    assert_eq!(
        on("PROD-BEANS")["branch"],
        erp_demo::demo_id("BRANCH-OLAYA"),
        "stock is held per branch"
    );
    assert_eq!(
        on("PROD-MILK")["on_hand"],
        0,
        "the crate went off and was thrown out"
    );
    assert_eq!(
        on("PROD-PASTRY")["on_hand"],
        57,
        "sixty came in and fifty-seven are there"
    );
    assert_eq!(on("PROD-GRINDER")["on_hand"], 1, "one of two was damaged");

    let lots = demo.get("/v1/inventory/lots").await["items"].clone();
    let lots = lots.as_array().expect("a list");
    the_batches_are_in_the_order_they_go_out(lots);

    let movements = demo.get("/v1/inventory/movements").await["items"].clone();
    let movements = movements.as_array().expect("a list");

    let count = movements
        .iter()
        .find(|row| row["kind"] == "counted")
        .expect("somebody counted a lot");
    assert_eq!(
        count["quantity"], demo.seeded.stock_short,
        "the demo's stocktake is three short, on purpose"
    );
    assert_eq!(count["expected"], 60);
    assert_eq!(count["declared"], 57);

    let expired = movements
        .iter()
        .find(|row| row["reason"] == "expired")
        .expect("the milk was thrown out");
    assert_eq!(expired["quantity"], -24);
    assert_eq!(expired["value"]["minor"], -12_000);

    what_moved_explains_what_is_there(lots, shelves, movements);

    // **The canary decision R3 makes load-bearing.** `1300 Inventory` was
    // debited by every delivery and has been credited by everything that left
    // the shelves — the crate of milk, the damaged grinder, the three
    // croissants nobody could find — and it has to come to exactly what the
    // shelves are worth.
    //
    // Two numbers built by two routes that share nothing but the log: one from
    // `proj_inventory`, one from `proj_ledger`. This is the same assertion
    // `StockValueAgrees` makes as a health finding on every tenant; here it is
    // made against a business with something on its shelves.
    let held: i64 = shelves
        .iter()
        .filter_map(|row| row["value"]["minor"].as_i64())
        .sum();
    let carried = demo.get("/v1/ledger/accounts").await;
    let carried = carried
        .as_array()
        .expect("a list")
        .iter()
        .find(|account| account["code"] == "1300")
        .and_then(|account| account["balance"].as_i64())
        .expect("the chart has an inventory account");
    assert_eq!(
        carried, held,
        "the books and the shelves disagree about what the stock is worth"
    );
    assert!(held > 0, "a demo whose shelves are empty proves nothing");

    // **And the other side of it: the café was billed for what it received.**
    // Every delivery credited `2010 Goods received, not invoiced` and the
    // supplier's bill debited it back, line for line, so the holding account is
    // flat. A non-zero balance here is goods received and never invoiced, or
    // invoiced and never received — both real, and both readable off one
    // account rather than out of a health finding.
    let holding = carried_on(demo, "2010").await;
    assert_eq!(
        holding, 0,
        "the deliveries and the bill that paid for them do not cancel"
    );
}

/// What one account's balance is, from the ledger's own route.
async fn carried_on(demo: &Demo, code: &str) -> i64 {
    demo.get("/v1/ledger/accounts")
        .await
        .as_array()
        .expect("a list")
        .iter()
        .find(|account| account["code"] == code)
        .and_then(|account| account["balance"].as_i64())
        .expect("the chart has the account")
}

/// **What is left, by batch, in the order it will go out.** The milk is not
/// here: a lot that empties closes, and its history is in the movements.
fn the_batches_are_in_the_order_they_go_out(lots: &[serde_json::Value]) {
    assert_eq!(
        lots.iter()
            .map(|l| (l["code"].clone(), l["expires_on"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                serde_json::json!("ROAST-2026-03-24"),
                serde_json::json!("2026-06-24")
            ),
            (
                serde_json::json!("ROAST-2026-03-31"),
                serde_json::json!("2026-07-01")
            ),
            // Undated batches sort after every dated one, oldest first.
            (serde_json::Value::Null, serde_json::Value::Null),
            (serde_json::Value::Null, serde_json::Value::Null),
        ],
        "the listing is not the order stock goes out in"
    );
    let grinder = lots
        .iter()
        .find(|l| l["product"] == erp_demo::demo_id("PROD-GRINDER"))
        .expect("the grinder that is left");
    assert_eq!(
        grinder["serials"],
        serde_json::json!(["EK43-77301"]),
        "the damaged one should have left its lot by name"
    );
}

/// **The canary.** What is on a batch is the sum of the movements against it,
/// and what is on a shelf is the sum of the movements against the product —
/// the same property one level up.
fn what_moved_explains_what_is_there(
    lots: &[serde_json::Value],
    shelves: &[serde_json::Value],
    movements: &[serde_json::Value],
) {
    for lot in lots {
        assert_eq!(
            movements
                .iter()
                .filter(|row| row["lot"] == lot["id"])
                .filter_map(|row| row["quantity"].as_i64())
                .sum::<i64>(),
            lot["remaining"].as_i64().expect("a quantity"),
            "lot {} holds a quantity its movements do not explain",
            lot["id"]
        );
    }
    for shelf in shelves {
        assert_eq!(
            movements
                .iter()
                .filter(|row| row["product"] == shelf["product"])
                .filter_map(|row| row["quantity"].as_i64())
                .sum::<i64>(),
            shelf["on_hand"].as_i64().expect("a quantity"),
            "{} holds a quantity its movements do not explain",
            shelf["product"]
        );
    }
}
