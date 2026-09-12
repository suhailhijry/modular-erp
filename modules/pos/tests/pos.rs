//! The counter, against a real tenant.
//!
//! The test that carries this file is [`a_cafe_opens_sells_and_closes_level`] —
//! the phase's exit criterion, and the one that proves the whole composition:
//! the shift, the invoice `sales` issued, its payment and the ledger all agree
//! after forty sales, and the drawer counts level.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use pos::{
    Basket, Method, Opening, PayOut, PosError, Tender, close_shift, open_shift, pay_out, sell,
    take_back,
};

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
            .register_tenant_on("cafe", "Cafe", "primary", Actor::system())
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

        let mut conn = db.acquire().await.expect("connection");
        crm::install(&mut conn).await.expect("crm installs");
        ensure_group_schema::<crm::Crm>(&mut conn)
            .await
            .expect("crm checkpoint");
        ledger::install(&mut conn).await.expect("ledger installs");
        ensure_group_schema::<ledger::Ledger>(&mut conn)
            .await
            .expect("ledger checkpoint");
        sales::install(&mut conn).await.expect("sales installs");
        ensure_group_schema::<sales::Sales>(&mut conn)
            .await
            .expect("sales checkpoint");
        pos::install(&mut conn).await.expect("pos installs");
        ensure_group_schema::<pos::Pos>(&mut conn)
            .await
            .expect("pos checkpoint");
        drop(conn);

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

    async fn shift(&self, id: &str) -> Option<pos::ShiftSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        pos::shift(&mut conn, id).await.expect("reads")
    }

    async fn takings(&self, id: &str) -> Vec<pos::TakingRow> {
        let mut conn = self.pool.acquire().await.expect("connection");
        pos::takings(&mut conn, id).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

fn rejection(error: &CommandError<PosError>) -> Option<&PosError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

/// A basket of one coffee at 15.00 plus VAT, paid however the caller says.
fn coffee(tenders: Vec<Tender>) -> Basket {
    Basket {
        customer: sales::Customer::new("زبون"),
        lines: vec![sales::DraftLine {
            allowances: Vec::new(),
            description: "قهوة".to_owned(),
            net: money(1_500),
            category: ledger::VatCategory::Standard,
        }],
        discounts: Vec::new(),
        currency: sar(),
        tenders,
        note: String::new(),
        at: on("2026-04-01"),
    }
}

/// The gross of one coffee: 15.00 net at the shipped 15% rate.
fn gross() -> Money {
    money(1_725)
}

/// Two lines: the coffee, and a cake at 2 000 net. 4 025 gross.
fn coffee_and_cake(tenders: Vec<Tender>) -> Basket {
    let mut basket = coffee(tenders);
    basket.lines.push(sales::DraftLine {
        allowances: Vec::new(),
        description: "كيك".to_owned(),
        net: money(2_000),
        category: ledger::VatCategory::Standard,
    });
    basket
}

/// Hands back the cake and nothing else: 2 000 net, 2 300 gross.
fn cake_back(tendered: i64) -> pos::Return {
    pos::Return {
        reference: "RET-1".to_owned(),
        tenders: vec![Tender::new(Method::Cash, money(tendered))],
        lines: vec![sales::CreditLine {
            against: 1,
            net: money(2_000),
        }],
        why: "أعاد الكيك".to_owned(),
        at: on("2026-04-01"),
    }
}

async fn opened(fixture: &Fixture, id: &str, float: i64) {
    open_shift(
        &fixture.db,
        &code(id),
        &Opening {
            till: "١".to_owned(),
            operator: "staff-1".to_owned(),
            float: money(float),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("the till opens");
}

/// **The phase's exit criterion.**
///
/// A café opens a shift, sells forty coffees, and closes with a variance of
/// zero — and every one of those forty is a `sales` invoice with a statutory
/// number, posted to the ledger, which is what makes them reportable to ZATCA.
/// If this passes, the composition works; there is no second document model to
/// keep in step because there is no second document model.
#[tokio::test]
async fn a_cafe_opens_sells_and_closes_level() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 50_000).await;

    let mut numbers = Vec::new();
    for n in 1..=40 {
        let rung = sell(
            &fixture.db,
            &code("SHIFT-1"),
            &code(&format!("SALE-{n}")),
            &coffee(vec![Tender::new(Method::Cash, gross())]),
            &Metadata::default(),
            sales::Authority::Member { owner: false },
        )
        .await
        .expect("the sale rings");
        assert_eq!(rung.total, gross());
        numbers.push(rung.number);
    }

    // Forty statutory numbers, all different: the series is `sales`', and a
    // till that reused one would be a ZATCA finding.
    numbers.sort();
    numbers.dedup();
    assert_eq!(numbers.len(), 40, "the invoice series repeated itself");

    // 500.00 float plus forty at 17.25.
    let expected = money(50_000 + 40 * 1_725);
    close_shift(
        &fixture.db,
        &code("SHIFT-1"),
        expected,
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("the till closes");

    fixture.project().await;
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(shift.sales_count, 40);
    assert_eq!(shift.expected, expected);
    assert_eq!(shift.declared, Some(expected));
    assert_eq!(shift.variance, Some(money(0)), "a level drawer");

    // And the books agree, without `pos` ever having posted a sale itself.
    //
    // **The ledger holds the takings and not the float.** Opening a shift moves
    // cash from a safe to a drawer, and both are `1000 Cash on hand` — the
    // business is no richer, so there is no entry. What the drawer should hold
    // is therefore a larger number than what this shift added to the books, and
    // the two are different questions on purpose.
    assert_eq!(
        fixture.balance("1000").await,
        money(40 * 1_725),
        "the takings, as the ledger sees them"
    );
    assert_eq!(
        fixture.balance("4000").await,
        money(-40 * 1_500),
        "revenue, net of tax"
    );
    assert_eq!(
        fixture.balance("2100").await,
        money(-40 * 225),
        "VAT payable"
    );

    fixture.cleanup().await;
}

/// **A drawer that is short books the loss, and one that is over books the
/// gain.**
///
/// This is the accounting reason the variance exists. A till that records a
/// shortage and does not post it leaves the ledger saying the drawer holds what
/// it does not, for ever, and the next reconciliation inherits the lie.
#[tokio::test]
async fn a_variance_is_booked_and_not_just_recorded() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHORT", 0).await;
    opened(&fixture, "OVER", 0).await;

    for (shift, n) in [("SHORT", 1), ("OVER", 2)] {
        sell(
            &fixture.db,
            &code(shift),
            &code(&format!("SALE-{n}")),
            &coffee(vec![Tender::new(Method::Cash, gross())]),
            &Metadata::default(),
            sales::Authority::Member { owner: false },
        )
        .await
        .expect("the sale rings");
    }

    // **Fifty halalas missing.** Asserted on its own before the overage, because
    // a short and an over of the same size net to nothing in the expense
    // account — and so does posting neither, which is the bug this is for.
    close_shift(
        &fixture.db,
        &code("SHORT"),
        money(1_675),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("closes short");

    fixture.project().await;
    assert_eq!(
        fixture.shift("SHORT").await.expect("there").variance,
        Some(money(-50)),
        "negative is short"
    );
    assert_eq!(
        fixture.balance("5910").await,
        money(50),
        "the shortage is an expense"
    );
    assert_eq!(
        fixture.balance("1000").await,
        money(2 * 1_725 - 50),
        "and cash came down by it"
    );

    // Fifty over, the other way.
    close_shift(
        &fixture.db,
        &code("OVER"),
        money(1_775),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("closes over");

    fixture.project().await;
    assert_eq!(
        fixture.shift("OVER").await.expect("there").variance,
        Some(money(50))
    );
    assert_eq!(
        fixture.balance("5910").await,
        money(0),
        "the overage offsets the shortage"
    );
    assert_eq!(fixture.balance("1000").await, money(2 * 1_725));

    fixture.cleanup().await;
}

/// **The tenders must come to exactly the sale.**
///
/// Less would leave a balance owing, which is an invoice on credit and not a
/// till sale. More is refused by `sales` as an overpayment, and it is right to:
/// change handed back is a counter concern, not a record.
#[tokio::test]
async fn a_sale_that_is_not_paid_in_full_is_refused_and_rings_nothing() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;

    for short in [money(1_000), money(2_000)] {
        let refused = sell(
            &fixture.db,
            &code("SHIFT-1"),
            &code("SALE-1"),
            &coffee(vec![Tender::new(Method::Cash, short)]),
            &Metadata::default(),
            sales::Authority::Member { owner: false },
        )
        .await
        .expect_err("the tenders do not come to the sale");
        assert!(matches!(
            rejection(&refused),
            Some(PosError::TendersDoNotMatch { .. })
        ));
    }

    fixture.project().await;
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(shift.sales_count, 0, "a refused sale rang up");
    assert_eq!(shift.expected, money(0));
    // **And no invoice was left behind.** The whole thing is one transaction,
    // so a refused tender takes the document with it.
    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "a stray receivable"
    );
    assert_eq!(fixture.balance("4000").await, money(0), "stray revenue");

    fixture.cleanup().await;
}

/// **A split payment reaches the drawer only for its cash half.**
#[tokio::test]
async fn a_split_payment_lands_in_two_accounts() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;

    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![
            Tender::new(Method::Cash, money(725)),
            Tender::new(Method::Card, money(1_000)),
        ]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");

    fixture.project().await;
    assert_eq!(
        fixture.shift("SHIFT-1").await.expect("there").expected,
        money(725),
        "the card half moved the drawer"
    );
    assert_eq!(fixture.balance("1000").await, money(725), "cash on hand");
    assert_eq!(fixture.balance("1010").await, money(1_000), "the bank");

    let takings = fixture.takings("SHIFT-1").await;
    assert_eq!(takings.len(), 2);
    assert_eq!(
        takings.iter().find(|t| t.method == "card").map(|t| t.taken),
        Some(money(1_000))
    );

    fixture.cleanup().await;
}

/// **A retried till rings nothing twice** (L8), and answers with the same
/// receipt — which is what a till whose request timed out needs.
#[tokio::test]
async fn a_retried_sale_is_harmless_and_gives_back_the_same_number() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;

    let first = sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![Tender::new(Method::Cash, gross())]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");

    for _ in 0..2 {
        let again = sell(
            &fixture.db,
            &code("SHIFT-1"),
            &code("SALE-1"),
            &coffee(vec![Tender::new(Method::Cash, gross())]),
            &Metadata::default(),
            sales::Authority::Member { owner: false },
        )
        .await
        .expect("a retry is not an error");
        assert_eq!(again.number, first.number, "a retry got a second number");
    }

    fixture.project().await;
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(shift.sales_count, 1, "the same sale rang up twice");
    assert_eq!(shift.expected, gross());
    assert_eq!(fixture.balance("1000").await, gross());

    fixture.cleanup().await;
}

/// **A shut till takes no more money**, which is what makes the count mean
/// anything: a sale rung after the drawer was counted would make the variance a
/// number about a moment that has passed.
#[tokio::test]
async fn a_closed_till_refuses_a_sale() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    close_shift(
        &fixture.db,
        &code("SHIFT-1"),
        money(0),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("closes");

    let refused = sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![Tender::new(Method::Cash, gross())]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect_err("the till is shut");
    assert!(matches!(rejection(&refused), Some(PosError::Closed(_))));

    // Closing again is a no-op rather than an error, so a manager whose request
    // timed out can send it again.
    close_shift(
        &fixture.db,
        &code("SHIFT-1"),
        money(9_999),
        on("2026-04-03"),
        &Metadata::default(),
    )
    .await
    .expect("closing a shut till is harmless");

    fixture.project().await;
    assert_eq!(
        fixture.shift("SHIFT-1").await.expect("there").declared,
        Some(money(0)),
        "the second close overwrote the count"
    );

    fixture.cleanup().await;
}

/// **Cash out of the drawer that is not a refund** — a banking run — comes off
/// what the drawer should hold and posts where it went.
#[tokio::test]
async fn cash_paid_out_leaves_the_drawer_and_the_books_together() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 100_000).await;

    for _ in 0..2 {
        pay_out(
            &fixture.db,
            &code("SHIFT-1"),
            &PayOut {
                reference: "BANK-1".to_owned(),
                amount: money(60_000),
                to: code("1010"),
                why: "إيداع بنكي".to_owned(),
                at: on("2026-04-01"),
            },
            &Metadata::default(),
        )
        .await
        .expect("the run is recorded");
    }

    fixture.project().await;
    assert_eq!(
        fixture.shift("SHIFT-1").await.expect("there").expected,
        money(40_000),
        "the same reference paid out twice"
    );
    assert_eq!(fixture.balance("1000").await, money(-60_000));
    assert_eq!(fixture.balance("1010").await, money(60_000));

    fixture.cleanup().await;
}

/// **Everything here is a pure function of the log.**
#[tokio::test]
async fn a_rebuild_reproduces_the_till() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 20_000).await;

    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![
            Tender::new(Method::Cash, money(725)),
            Tender::new(Method::Transfer, money(1_000)),
        ]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("rings");
    pay_out(
        &fixture.db,
        &code("SHIFT-1"),
        &PayOut {
            reference: "BANK-1".to_owned(),
            amount: money(5_000),
            to: code("1010"),
            why: "إيداع".to_owned(),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("paid out");
    close_shift(
        &fixture.db,
        &code("SHIFT-1"),
        money(15_000),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("closes");

    fixture.project().await;
    assert!(fixture.shift("SHIFT-1").await.is_some());

    let owned = pos::projections();
    let refs: Vec<&dyn Projection<Group = pos::Pos>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<pos::Pos>(&fixture.pool, &refs, pos::upcasters(), 200)
        .await
        .expect("the shadow replays");
    assert!(
        report.is_reproducible(),
        "a rebuild must reproduce the till exactly: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every message this module can produce has a translation in every locale.
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&pos::CATALOG);
}

/// **A return: the money back, the sale credited, and the drawer down.**
///
/// The gap Phase 15 could not close. `cancel_invoice` refused any invoice that
/// had ever been paid, and every till sale is paid the instant it happens — so
/// no till sale could be credited through any route. `sales` gained a refund,
/// its rule became *"nothing is still held"*, and this is the whole thing in one
/// transaction.
#[tokio::test]
async fn a_return_hands_the_money_back_and_credits_the_sale() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![Tender::new(Method::Cash, gross())]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");

    take_back(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &pos::Return {
            reference: "RET-1".to_owned(),
            tenders: vec![Tender::new(Method::Cash, gross())],
            lines: Vec::new(),
            why: "أعاد المنتج".to_owned(),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the return is taken");

    fixture.project().await;

    // The drawer is back where it started, and so are the books.
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(
        shift.expected,
        money(0),
        "the cash did not leave the drawer"
    );
    assert_eq!(fixture.balance("1000").await, money(0), "cash on hand");
    assert_eq!(fixture.balance("4000").await, money(0), "revenue reversed");
    assert_eq!(fixture.balance("2100").await, money(0), "VAT reversed");
    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "the receivable is square"
    );

    // The takings still say what happened: a sale was made and given back.
    let takings = fixture.takings("SHIFT-1").await;
    let cash = takings
        .iter()
        .find(|t| t.method == "cash")
        .expect("cash was taken");
    assert_eq!(cash.taken, gross());
    assert_eq!(cash.refunded, gross());

    fixture.cleanup().await;
}

/// A retried return hands the money back once.
#[tokio::test]
async fn a_retried_return_is_harmless() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![Tender::new(Method::Cash, gross())]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("rings");

    for _ in 0..3 {
        take_back(
            &fixture.db,
            &code("SHIFT-1"),
            &code("SALE-1"),
            &pos::Return {
                reference: "RET-1".to_owned(),
                tenders: vec![Tender::new(Method::Cash, gross())],
                lines: Vec::new(),
                why: "أعاد".to_owned(),
                at: on("2026-04-01"),
            },
            &Metadata::default(),
            sales::Authority::Member { owner: false },
        )
        .await
        .expect("a retry is not an error");
    }

    fixture.project().await;

    // The ledger is the half `sales` already protects, and asserting only this
    // is what let a real bug through: the credit note and the money deduplicate
    // by reference inside `sales`, so these stayed at zero while the drawer
    // came down once per retry.
    assert_eq!(fixture.balance("1000").await, money(0), "refunded twice");
    assert_eq!(fixture.balance("4000").await, money(0));

    // The drawer is the half this module owns, and it is the one that broke.
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(
        shift.expected,
        money(0),
        "the drawer came down once per retry"
    );
    let takings = fixture.takings("SHIFT-1").await;
    let cash = takings
        .iter()
        .find(|t| t.method == "cash")
        .expect("cash was taken");
    assert_eq!(cash.refunded, gross(), "the takings counted it three times");

    fixture.cleanup().await;
}

/// **One item of two comes back, and only that item is credited.** The first
/// version could only cancel the whole sale, and a partial return either rolled
/// everything back or was refused with an error about payments.
#[tokio::test]
async fn a_return_of_one_line_credits_that_line_and_leaves_the_rest() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee_and_cake(vec![Tender::new(Method::Cash, money(4_025))]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");

    take_back(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &cake_back(2_300),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the cake comes back");
    fixture.project().await;

    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(
        shift.expected,
        money(1_725),
        "the coffee's cash stays in the drawer"
    );
    assert_eq!(fixture.balance("1000").await, money(1_725), "cash on hand");
    assert_eq!(
        fixture.balance("4000").await,
        money(-1_500),
        "the coffee's revenue stands"
    );
    assert_eq!(fixture.balance("2100").await, money(-225), "and its VAT");
    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "the receivable is square"
    );
    let takings = fixture.takings("SHIFT-1").await;
    let cash = takings.iter().find(|t| t.method == "cash").expect("cash");
    assert_eq!(cash.taken, money(4_025));
    assert_eq!(cash.refunded, money(2_300));

    fixture.cleanup().await;
}

/// **The tenders must come to what the lines credit.** Handing back more leaves
/// a receivable; handing back less leaves the customer owed money the till has
/// no record of. Either is refused whole, and the sale is untouched.
#[tokio::test]
async fn a_partial_return_whose_tenders_do_not_match_its_lines_is_refused() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee_and_cake(vec![Tender::new(Method::Cash, money(4_025))]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");

    let refused = take_back(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &cake_back(2_000),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect_err("2 000 is the cake's net, not its gross");
    assert!(
        matches!(
            rejection(&refused),
            Some(PosError::TendersDoNotMatch { .. })
        ),
        "{refused:?}"
    );
    fixture.project().await;

    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(shift.expected, money(4_025), "nothing left the drawer");
    assert_eq!(
        fixture.balance("4000").await,
        money(-3_500),
        "nothing was credited"
    );
    assert_eq!(
        fixture.balance("1000").await,
        money(4_025),
        "nothing was refunded"
    );

    fixture.cleanup().await;
}

/// **A closed till refuses a return**, the way it refuses a sale and a pay-out.
/// A refund after the count would change what the drawer should hold after the
/// variance has been posted, and the variance is never revisited.
#[tokio::test]
async fn a_closed_till_refuses_a_return() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &coffee(vec![Tender::new(Method::Cash, gross())]),
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect("the sale rings");
    close_shift(
        &fixture.db,
        &code("SHIFT-1"),
        gross(),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("closes level");

    let refused = take_back(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-1"),
        &pos::Return {
            reference: "RET-1".to_owned(),
            tenders: vec![Tender::new(Method::Cash, gross())],
            lines: Vec::new(),
            why: "أعاد المنتج".to_owned(),
            at: on("2026-04-02"),
        },
        &Metadata::default(),
        sales::Authority::Member { owner: false },
    )
    .await
    .expect_err("the till is shut");
    assert!(
        matches!(rejection(&refused), Some(PosError::Closed(_))),
        "{refused:?}"
    );
    fixture.project().await;
    assert_eq!(
        fixture.balance("4000").await,
        money(-1_500),
        "the sale was not credited behind a closed till"
    );

    fixture.cleanup().await;
}

/// **A till is a member at a counter, and held to the document limit.**
///
/// This tenant has no `hr`, so nobody can hold the claim that lifts it: a
/// clerk's sale over it is refused and rings nothing, the owner's is not. A
/// return is judged twice — each tender's refund, and the credit note — so
/// splitting the money across two tenders under the limit does not carry a
/// credit note over it.
#[tokio::test]
async fn a_till_holds_a_clerk_to_the_document_limit() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    {
        let mut conn = fixture.db.acquire().await.expect("a connection");
        erp_eventlog::configuration::set(
            &mut conn,
            sales::DocumentLimit::KEY,
            &Some(
                sales::DocumentLimit::new(money(1_000_000), sales::Basis::AfterVat)
                    .expect("a limit"),
            ),
            Some("the-owner"),
            None,
        )
        .await
        .expect("the limit is set");
    }
    // 10,000 net is 11,500 after VAT.
    let big = |tenders| Basket {
        lines: vec![sales::DraftLine {
            allowances: Vec::new(),
            description: "آلة قهوة".to_owned(),
            net: money(1_000_000),
            category: ledger::VatCategory::Standard,
        }],
        ..coffee(tenders)
    };
    let clerk = Metadata {
        actor: Some("clerk".to_owned()),
        ..Metadata::default()
    };

    let refused = sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-BIG"),
        &big(vec![Tender::new(Method::Cash, money(1_150_000))]),
        &clerk,
        sales::Authority::Member { owner: false },
    )
    .await;
    assert!(
        matches!(
            refused.as_ref().err().and_then(rejection),
            Some(PosError::Sale(sales::SalesError::OverDocumentLimit { .. }))
        ),
        "{refused:?}"
    );

    sell(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-OWNER"),
        &big(vec![
            Tender::new(Method::Cash, money(575_000)),
            Tender::new(Method::Card, money(575_000)),
        ]),
        &Metadata::default(),
        sales::Authority::Member { owner: true },
    )
    .await
    .expect("the owner is never limited");

    let refused = take_back(
        &fixture.db,
        &code("SHIFT-1"),
        &code("SALE-OWNER"),
        &pos::Return {
            reference: "RET-BIG".to_owned(),
            tenders: vec![
                Tender::new(Method::Cash, money(575_000)),
                Tender::new(Method::Card, money(575_000)),
            ],
            lines: Vec::new(),
            why: "أعادها".to_owned(),
            at: on("2026-04-01"),
        },
        &clerk,
        sales::Authority::Member { owner: false },
    )
    .await;
    assert!(
        matches!(
            refused.as_ref().err().and_then(rejection),
            Some(PosError::Sale(sales::SalesError::OverDocumentLimit { amount, .. }))
                if *amount == money(1_150_000)
        ),
        "the credit note is 11,500 though each tender is under 10,000: {refused:?}"
    );

    fixture.project().await;
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(
        shift.expected,
        money(575_000),
        "the refused sale and the refused return moved no cash"
    );

    fixture.cleanup().await;
}

/// An org chart in this café — boss ← supervisor ← clerk — and the `hr` it
/// needs. **Installing `hr` is part of the point**: until this runs there is no
/// `hr` here at all, which is the ordinary shape of a till.
async fn a_chart_with_three(fixture: &Fixture) {
    {
        let mut conn = fixture.db.acquire().await.expect("a connection");
        hr::install(&mut conn).await.expect("hr installs");
        ensure_group_schema::<hr::Hr>(&mut conn)
            .await
            .expect("hr checkpoint");
    }
    for (id, login, reports_to) in [
        ("EMP-BOSS", "boss", None),
        ("EMP-SUPERVISOR", "supervisor", Some("EMP-BOSS")),
        ("EMP-CLERK", "clerk", Some("EMP-SUPERVISOR")),
    ] {
        hr::hire(
            &fixture.db,
            &code(id),
            &hr::Hire {
                details: hr::Details {
                    name: id.to_owned(),
                    name_latin: None,
                    national_id: None,
                    email: Some(format!("{login}@cafe.test")),
                    phone: None,
                },
                reports_to: reports_to.map(code),
                branch: None,
                at: on("2026-01-01"),
            },
            &Metadata::default(),
        )
        .await
        .expect("hires");
        hr::link_login(
            &fixture.db,
            &code(id),
            login,
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("links a login");
    }
    let owned = hr::projections();
    let refs: Vec<&dyn Projection<Group = hr::Hr>> = owned.iter().map(AsRef::as_ref).collect();
    run_to_head::<hr::Hr>(&fixture.pool, &refs, hr::upcasters(), 200)
        .await
        .expect("hr projects");
}

/// Grants it company-wide, **asking for propagation** the way the granting
/// screen does — which `hr` refuses to honour, because this claim is one of
/// `SEGREGATED`.
async fn grant_the_credit_claim(fixture: &Fixture, employee: &str) {
    hr::grant_claim(
        &fixture.db,
        &code(employee),
        &hr::Claim {
            name: sales::APPROVE_CREDIT_NOTE.to_owned(),
            branch: None,
        },
        true,
    )
    .await
    .expect("grants the claim");
}

/// **A till return asks for `sales:approve_credit_note`.**
///
/// Until §70 the claim was asked for only in the two `sales` wrappers the sales
/// screens call, and `take_back` calls the roots — so a clerk refused at
/// `/v1/sales` could take the same return at the counter. That is one rule now,
/// and the change at the till is deliberate. What must **not** change is the
/// tenant that has granted nothing: this café has no `hr` at all until the
/// chart below, and its returns go through exactly as they did.
#[tokio::test]
async fn a_till_return_needs_the_credit_note_claim() {
    let fixture = Fixture::new().await;
    opened(&fixture, "SHIFT-1", 0).await;
    let member = sales::Authority::Member { owner: false };
    let at_the_till = |actor: &str| Metadata {
        actor: Some(actor.to_owned()),
        ..Metadata::default()
    };
    let back = |reference: &str| pos::Return {
        reference: reference.to_owned(),
        tenders: vec![Tender::new(Method::Cash, gross())],
        lines: Vec::new(),
        why: "أعادها".to_owned(),
        at: on("2026-04-01"),
    };
    let returned = async |sale: &str, reference: &str, actor: &str| {
        take_back(
            &fixture.db,
            &code("SHIFT-1"),
            &code(sale),
            &back(reference),
            &at_the_till(actor),
            member,
        )
        .await
    };
    let not_approved = |outcome: &Result<_, CommandError<PosError>>| {
        matches!(
            outcome.as_ref().err().and_then(rejection),
            Some(PosError::Sale(sales::SalesError::NotApproved(_)))
        )
    };

    for sale in ["SALE-1", "SALE-2", "SALE-3"] {
        sell(
            &fixture.db,
            &code("SHIFT-1"),
            &code(sale),
            &coffee(vec![Tender::new(Method::Cash, gross())]),
            &at_the_till("clerk"),
            member,
        )
        .await
        .expect("rings");
    }

    // **A tenant that has granted nothing is asked nothing.** No `hr`, no
    // grants, no change.
    returned("SALE-1", "RET-1", "clerk")
        .await
        .expect("the door that opened yesterday opens today");

    a_chart_with_three(&fixture).await;
    grant_the_credit_claim(&fixture, "EMP-SUPERVISOR").await;

    let refused = returned("SALE-2", "RET-2", "clerk").await;
    assert!(not_approved(&refused), "{refused:?}");

    // **The same clerk, the same refusal, at the sales screen.** One rule.
    let screen = sales::cancel_invoice(
        &fixture.db,
        &code("SALE-2"),
        "CN-1",
        "أعادها",
        on("2026-04-01"),
        &at_the_till("clerk"),
        member,
    )
    .await;
    assert!(
        matches!(
            screen,
            Err(CommandError::Execute(ExecuteError::Rejected(
                sales::SalesError::NotApproved(_)
            )))
        ),
        "the till and the sales screen must refuse alike: {screen:?}"
    );

    // **This claim travels nowhere.** It is one of `hr::SEGREGATED`, so the
    // boss does not hold what the supervisor beneath them was granted — unlike
    // §68's `sales:exceed_document_limit`, which does.
    let refused = returned("SALE-3", "RET-3", "boss").await;
    assert!(
        not_approved(&refused),
        "a segregated claim does not travel up: {refused:?}"
    );
    returned("SALE-3", "RET-4", "supervisor")
        .await
        .expect("the supervisor holds what they were granted");

    grant_the_credit_claim(&fixture, "EMP-CLERK").await;
    returned("SALE-2", "RET-5", "clerk")
        .await
        .expect("granted to the clerk now");

    fixture.project().await;
    let shift = fixture.shift("SHIFT-1").await.expect("there");
    assert_eq!(
        shift.expected,
        money(0),
        "three sales rung and three handed back; the refusals moved nothing"
    );

    fixture.cleanup().await;
}
