//! Branches, against a real tenant.
//!
//! The test that carries this file is
//! [`two_branches_report_separately_and_sum_to_one_trial_balance`] — the
//! phase's exit criterion, and the one that says what a branch actually is: a
//! dimension the ledger can be read by, not a row in a settings screen.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use branches::{Address, BranchError, Details, close_branch, open_branch};
use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use pos::{Basket, Method, Opening, Tender, close_shift, open_shift, sell};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("a real code")
}

fn money(minor: i64) -> Money {
    Money::from_minor(minor, sar())
}

fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

struct Fixture {
    db: TenantDb,
    pool: sqlx::PgPool,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

/// Every module this fixture drives, installed into one tenant.
///
/// Split out because `Fixture::new` was over the line limit, and because the
/// list is the interesting part: `branches` is a leaf, and the four beneath it
/// are what the exit criterion actually exercises.
async fn install_modules(db: &TenantDb) {
    let mut conn = db.acquire().await.expect("connection");
    crm::install(&mut conn).await.expect("crm installs");
    ensure_group_schema::<crm::Crm>(&mut conn)
        .await
        .expect("crm checkpoint");
    ledger::install(&mut conn).await.expect("ledger installs");
    ensure_group_schema::<ledger::Ledger>(&mut conn)
        .await
        .expect("ledger checkpoint");
    branches::install(&mut conn)
        .await
        .expect("branches installs");
    ensure_group_schema::<branches::Branches>(&mut conn)
        .await
        .expect("branches checkpoint");
    booking::install(&mut conn).await.expect("booking installs");
    ensure_group_schema::<booking::Booking>(&mut conn)
        .await
        .expect("booking checkpoint");
    sales::install(&mut conn).await.expect("sales installs");
    ensure_group_schema::<sales::Sales>(&mut conn)
        .await
        .expect("sales checkpoint");
    pos::install(&mut conn).await.expect("pos installs");
    ensure_group_schema::<pos::Pos>(&mut conn)
        .await
        .expect("pos checkpoint");
}

impl Fixture {
    async fn new() -> Self {
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

        let tenant = control
            .register_tenant_on("salon", "Salon", "primary", Actor::system())
            .await
            .expect("tenant registers");
        erp_testkit::create_named_database(&tenant.database_name, &TENANT)
            .await
            .expect("tenant database is created");
        control
            .activate_tenant(tenant.id, Actor::system())
            .await
            .expect("tenant activates");

        let db = control
            .enter_for_maintenance(tenant.id)
            .await
            .expect("maintenance entry");

        install_modules(&db).await;

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        let fixture = Self {
            db,
            pool,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        };

        // A chart to post into, and somebody to sell to.
        ledger::install_chart(
            &fixture.db,
            ledger::chart("services").expect("the services chart ships"),
            sar(),
            erp_i18n::Locale::English,
            &Metadata::default(),
        )
        .await
        .expect("the chart installs");

        crm::register_customer(
            &fixture.db,
            &code("CUST-1"),
            &crm::Details {
                name: "سارة".to_owned(),
                name_latin: None,
                kind: crm::CustomerKind::Person,
                contact: crm::Contact {
                    phone: Some("+966511111111".to_owned()),
                    email: None,
                },
                address: None,
                tax: None,
            },
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("the customer is on file");

        fixture
    }

    async fn project(&self) {
        let owned = branches::projections();
        let refs: Vec<&dyn Projection<Group = branches::Branches>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<branches::Branches>(&self.pool, &refs, branches::upcasters(), 200)
            .await
            .expect("branches projects");

        self.project_booking().await;

        let owned = pos::projections();
        let refs: Vec<&dyn Projection<Group = pos::Pos>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<pos::Pos>(&self.pool, &refs, pos::upcasters(), 200)
            .await
            .expect("pos projects");

        let owned = sales::projections();
        let refs: Vec<&dyn Projection<Group = sales::Sales>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<sales::Sales>(&self.pool, &refs, sales::upcasters(), 200)
            .await
            .expect("sales projects");

        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<ledger::Ledger>(&self.pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("ledger projects");
    }

    /// What the books say an account holds.
    async fn balance(&self, code: &str) -> Money {
        let mut conn = self.pool.acquire().await.expect("connection");
        ledger::account_balances(&mut conn)
            .await
            .expect("the ledger answers")
            .iter()
            .find(|b| b.code == code)
            .map_or_else(|| money(0), |b| b.balance)
    }

    async fn project_booking(&self) {
        let owned = booking::projections();
        let refs: Vec<&dyn Projection<Group = booking::Booking>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<booking::Booking>(&self.pool, &refs, booking::upcasters(), 200)
            .await
            .expect("booking projects");
    }

    /// What one branch did, per account.
    async fn per_branch(&self, branch: Option<&str>) -> Vec<ledger::BranchBalance> {
        let mut conn = self.pool.acquire().await.expect("connection");
        ledger::branch_balances(&mut conn, branch)
            .await
            .expect("the ledger answers")
    }

    /// The whole-tenant invariant: debits equal credits, per currency.
    async fn balances(&self) -> bool {
        let mut conn = self.pool.acquire().await.expect("connection");
        ledger::imbalances(&mut conn)
            .await
            .expect("the ledger answers")
            .is_empty()
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

fn rejection(error: &CommandError<BranchError>) -> Option<&BranchError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

fn details(name: &str) -> Details {
    Details {
        name: name.to_owned(),
        name_latin: None,
        address: Address {
            street: "طريق الملك فهد".to_owned(),
            building: None,
            district: None,
            city: "الرياض".to_owned(),
            postal_code: None,
            country: "SA".to_owned(),
        },
    }
}

/// **Where a request happened**, which is how a branch reaches the ledger.
///
/// The HTTP layer folds this in from `X-Branch` on the authorization extractor;
/// a command-level test says it directly, which is the same thing one layer
/// down.
fn at(branch: &str) -> Metadata {
    Metadata::default().at_branch(branch)
}

/// A basket of one service at 100.00 plus VAT, paid in cash.
fn service(net: i64) -> Basket {
    Basket {
        customer: sales::Customer::new("زبون"),
        lines: vec![sales::DraftLine {
            description: "قص".to_owned(),
            net: money(net),
            category: ledger::VatCategory::Standard,
        }],
        discounts: Vec::new(),
        currency: sar(),
        tenders: vec![Tender::new(Method::Cash, money(net + net * 15 / 100))],
        note: String::new(),
        at: on("2026-05-01"),
    }
}

/// **The phase's exit criterion.**
///
/// A two-branch salon reports each separately, and both reconcile to one trial
/// balance. Every number here reaches the ledger without `sales` or `pos`
/// carrying a branch field: the dimension travels in the request's metadata, and
/// `post_entry_in` is where it lands.
#[tokio::test]
async fn two_branches_report_separately_and_sum_to_one_trial_balance() {
    let fixture = Fixture::new().await;

    for (id, name) in [("OLAYA", "فرع العليا"), ("MALAZ", "فرع الملز")] {
        open_branch(
            &fixture.db,
            &code(id),
            &details(name),
            on("2026-04-01"),
            &Metadata::default(),
        )
        .await
        .expect("the branch opens");
    }

    // A till at each, and a different day's takings at each.
    for (branch, shift, sales) in [
        ("OLAYA", "SHIFT-O", [10_000, 20_000]),
        ("MALAZ", "SHIFT-M", [30_000, 40_000]),
    ] {
        open_shift(
            &fixture.db,
            &code(shift),
            &Opening {
                till: branch.to_owned(),
                operator: "staff-1".to_owned(),
                float: money(0),
                at: on("2026-05-01"),
            },
            &at(branch),
        )
        .await
        .expect("the till opens");

        for (n, net) in sales.into_iter().enumerate() {
            sell(
                &fixture.db,
                &code(shift),
                &code(&format!("{branch}-SALE-{n}")),
                &service(net),
                &at(branch),
            )
            .await
            .expect("the sale rings");
        }

        close_shift(
            &fixture.db,
            &code(shift),
            money(sales.iter().map(|n| n + n * 15 / 100).sum::<i64>()),
            on("2026-05-02"),
            &at(branch),
        )
        .await
        .expect("the till closes");
    }

    fixture.project().await;

    // **Each branch reports on its own.** Olaya sold 300, Malaz sold 700.
    let olaya = fixture.per_branch(Some("OLAYA")).await;
    let malaz = fixture.per_branch(Some("MALAZ")).await;
    let revenue = |rows: &[ledger::BranchBalance]| {
        rows.iter()
            .find(|r| r.code == "4000")
            .map_or_else(|| money(0), |r| r.balance)
    };
    assert_eq!(revenue(&olaya), money(-30_000), "Olaya's revenue");
    assert_eq!(revenue(&malaz), money(-70_000), "Malaz's revenue");
    assert!(
        olaya.iter().all(|r| r.branch.as_deref() == Some("OLAYA")),
        "a branch's report carried another branch's rows"
    );

    // **And the branches are a partition of the whole**, checked against the
    // unsplit view rather than against a number typed here: splitting a report
    // by branch must change what is shown and never what was posted.
    let split: i64 = fixture
        .per_branch(None)
        .await
        .iter()
        .filter(|r| r.code == "4000")
        .map(|r| r.balance.minor())
        .sum();
    assert_eq!(
        Money::from_minor(split, sar()),
        fixture.balance("4000").await,
        "the branches do not sum to the chart"
    );
    assert_eq!(split, -100_000, "and the whole business sold a thousand");

    // **One trial balance, and it balances.** This is the invariant a branch
    // dimension must not break: splitting a report by branch changes what is
    // shown and never what was posted.
    assert!(
        fixture.balances().await,
        "the trial balance stopped balancing once postings carried a branch"
    );

    fixture.cleanup().await;
}

/// **A document cannot be dated to a branch that is not there.**
///
/// Checked once, in `ledger::post_entry_in`, because every posting in the
/// system arrives there — so `sales`, `purchases`, `prepaid` and `pos` all
/// inherit it without any of them repeating the check.
#[tokio::test]
async fn a_posting_to_an_unknown_branch_is_refused_everywhere() {
    let fixture = Fixture::new().await;
    open_shift(
        &fixture.db,
        &code("SHIFT-1"),
        &Opening {
            till: "١".to_owned(),
            operator: "staff-1".to_owned(),
            float: money(0),
            at: on("2026-05-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("the till opens");

    let refused = sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &service(10_000),
        &at("NOWHERE"),
    )
    .await
    .expect_err("there is no such branch");
    assert!(
        format!("{refused}").contains("NOWHERE"),
        "the refusal did not name the branch: {refused}"
    );

    fixture.project().await;
    // **And nothing was left behind.** The sale, its payment and the shift's
    // event are one transaction, so a refused branch takes the document too.
    assert_eq!(fixture.balance("4000").await, money(0), "stray revenue");
    assert!(fixture.per_branch(None).await.is_empty(), "a stray posting");

    fixture.cleanup().await;
}

/// **A closed branch keeps its history and takes nothing new.**
///
/// The distinction `crm` draws about an archived customer, and for the same
/// reason: a dimension that vanished would take the meaning of its own history
/// with it.
#[tokio::test]
async fn a_closed_branch_keeps_what_it_traded() {
    let fixture = Fixture::new().await;
    open_branch(
        &fixture.db,
        &code("OLAYA"),
        &details("فرع العليا"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("opens");
    open_shift(
        &fixture.db,
        &code("SHIFT-1"),
        &Opening {
            till: "١".to_owned(),
            operator: "staff-1".to_owned(),
            float: money(0),
            at: on("2026-05-01"),
        },
        &at("OLAYA"),
    )
    .await
    .expect("the till opens");
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &service(10_000),
        &at("OLAYA"),
    )
    .await
    .expect("the sale rings");

    close_branch(
        &fixture.db,
        &code("OLAYA"),
        "انتقل".to_owned().as_str(),
        on("2026-06-01"),
        &Metadata::default(),
    )
    .await
    .expect("closes");

    // Nothing new.
    let refused = sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-2"),
        &service(10_000),
        &at("OLAYA"),
    )
    .await
    .expect_err("the branch is closed");
    assert!(format!("{refused}").contains("OLAYA"));

    fixture.project().await;

    // And everything it traded is still there and still attributed to it.
    let olaya = fixture.per_branch(Some("OLAYA")).await;
    assert_eq!(
        olaya.iter().find(|r| r.code == "4000").map(|r| r.balance),
        Some(money(-10_000)),
        "closing the branch took its year with it"
    );

    fixture.cleanup().await;
}

/// A branch refuses what it cannot be: no name, no address, and a country code
/// that is not two letters — the field a caller gets wrong, and the one ZATCA
/// prints on every document the branch issues.
#[tokio::test]
async fn a_branch_refuses_what_it_cannot_be() {
    let fixture = Fixture::new().await;

    let mut nameless = details("  ");
    nameless.name_latin = None;
    let refused = open_branch(
        &fixture.db,
        &code("B-1"),
        &nameless,
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("a branch needs a name");
    assert!(matches!(
        rejection(&refused),
        Some(BranchError::Details(branches::BadBranch::NoName))
    ));

    let mut bad_country = details("فرع");
    bad_country.address.country = "SAU".to_owned();
    let refused = open_branch(
        &fixture.db,
        &code("B-2"),
        &bad_country,
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("a country is two letters");
    assert!(matches!(
        rejection(&refused),
        Some(BranchError::Details(branches::BadBranch::NotACountry(_)))
    ));

    fixture.cleanup().await;
}

/// **Everything here is a pure function of the log.**
#[tokio::test]
async fn a_rebuild_reproduces_the_branches() {
    let fixture = Fixture::new().await;
    open_branch(
        &fixture.db,
        &code("OLAYA"),
        &details("فرع العليا"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("opens");
    branches::amend_branch(
        &fixture.db,
        &code("OLAYA"),
        &details("فرع العليا الجديد"),
        on("2026-04-05"),
        &Metadata::default(),
    )
    .await
    .expect("amended");
    close_branch(
        &fixture.db,
        &code("OLAYA"),
        "انتقل",
        on("2026-06-01"),
        &Metadata::default(),
    )
    .await
    .expect("closes");
    branches::reopen_branch(
        &fixture.db,
        &code("OLAYA"),
        on("2026-07-01"),
        &Metadata::default(),
    )
    .await
    .expect("reopens");

    fixture.project().await;
    let mut conn = fixture.pool.acquire().await.expect("connection");
    let found = branches::branch(&mut conn, "OLAYA")
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(found.name, "فرع العليا الجديد");
    assert!(found.closed_at.is_none(), "reopening left it closed");
    drop(conn);

    let owned = branches::projections();
    let refs: Vec<&dyn Projection<Group = branches::Branches>> =
        owned.iter().map(AsRef::as_ref).collect();
    let report =
        replay_shadow::<branches::Branches>(&fixture.pool, &refs, branches::upcasters(), 200)
            .await
            .expect("the shadow replays");
    assert!(
        report.is_reproducible(),
        "a rebuild must reproduce the branches exactly: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every message this module can produce has a translation in every locale.
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&branches::CATALOG);
}

/// **"Book at Olaya."**
///
/// A resource says where it is, the list narrows to a branch, and a resource
/// placed at a branch that is not open is refused — checked at declaration
/// rather than inherited from `post_entry_in`, because declaring a chair posts
/// nothing and so has no journal entry to carry the check.
#[tokio::test]
async fn a_resource_belongs_to_a_branch_and_the_rota_narrows_to_it() {
    let fixture = Fixture::new().await;
    for (id, name) in [("OLAYA", "فرع العليا"), ("MALAZ", "فرع الملز")] {
        open_branch(
            &fixture.db,
            &code(id),
            &details(name),
            on("2026-04-01"),
            &Metadata::default(),
        )
        .await
        .expect("the branch opens");
    }

    let chair = |branch: &str| booking::Details {
        name: "كرسي".to_owned(),
        name_latin: None,
        kind: booking::Kind::Place,
        capacity: 1,
        branch: Some(code(branch)),
        employee: None,
    };

    for (id, branch) in [("CHAIR-O", "OLAYA"), ("CHAIR-M", "MALAZ")] {
        booking::declare_resource(
            &fixture.db,
            &code(id),
            &chair(branch),
            on("2026-04-02"),
            &Metadata::default(),
        )
        .await
        .expect("the chair is declared");
    }

    // A chair at a branch that is not there.
    let refused = booking::declare_resource(
        &fixture.db,
        &code("CHAIR-X"),
        &chair("NOWHERE"),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect_err("there is no such branch");
    assert!(format!("{refused}").contains("NOWHERE"));

    // And one at a branch that has closed.
    close_branch(
        &fixture.db,
        &code("MALAZ"),
        "انتقل",
        on("2026-04-03"),
        &Metadata::default(),
    )
    .await
    .expect("closes");
    let refused = booking::declare_resource(
        &fixture.db,
        &code("CHAIR-M2"),
        &chair("MALAZ"),
        on("2026-04-04"),
        &Metadata::default(),
    )
    .await
    .expect_err("the branch is closed");
    assert!(format!("{refused}").contains("MALAZ"));

    fixture.project_booking().await;
    let mut conn = fixture.pool.acquire().await.expect("connection");

    // The rota for one place.
    let olaya = booking::resources(&mut conn, Some("OLAYA"), false, 50, None)
        .await
        .expect("reads");
    assert_eq!(olaya.items.len(), 1, "Olaya's rota carried another branch");
    assert_eq!(olaya.items[0].id, "CHAIR-O");
    assert_eq!(olaya.items[0].branch.as_deref(), Some("OLAYA"));

    // And the whole business, which is what no branch header means.
    let everywhere = booking::resources(&mut conn, None, false, 50, None)
        .await
        .expect("reads");
    assert_eq!(
        everywhere.items.len(),
        2,
        "the unfiltered rota is not every branch's"
    );

    drop(conn);
    fixture.cleanup().await;
}
