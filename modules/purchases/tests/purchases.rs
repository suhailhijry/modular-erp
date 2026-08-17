//! Purchases end to end, against a real tenant with the ledger installed.
//!
//! The test that carries this module is
//! [`what_the_supplier_charged_is_what_goes_in_the_books`]. Everything else
//! checks a rule; that one checks the claim the module was built on — that input
//! tax is *recorded* rather than computed, because a reclaim is evidenced by the
//! supplier's document and not by our arithmetic.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use ledger::{AccountKind, Ledger, VatCategory, account_balances, open_account, trial_balance};
use purchases::{
    BillLine, Draft, Payment, PurchaseError, Purchases, Supplier, pay_bill, record_bill,
};
use spa_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use spa_eventlog::{ExecuteError, Metadata};
use spa_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use spa_testkit::{Schema, TestDb};
use spa_types::{AggregateId, CurrencyCode, Money, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &spa_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("valid")
}
fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}
fn money(minor: i64) -> Money {
    Money::from_minor(minor, sar())
}
/// Whole riyals, so tests read in the units a person uses.
fn riyals(major: i64) -> Money {
    money(major * 100)
}
fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

fn line(account: &str, net: Money, category: VatCategory, tax: Money) -> BillLine {
    BillLine {
        description: format!("something from {account}"),
        account: code(account),
        net,
        category,
        rate_bp: ledger::Rates::saudi_arabia().of(category),
        tax,
    }
}

fn draft(lines: Vec<BillLine>) -> Draft {
    Draft {
        supplier: Supplier::new("Najd Supplies").with_vat_number("311234567800003"),
        supplier_reference: "NS-1".to_owned(),
        billed_on: on("2026-02-03"),
        due_on: None,
        currency: sar(),
        lines,
        note: String::new(),
    }
}

struct Fixture {
    db: TenantDb,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    tenant_database: String,
}

impl Fixture {
    /// A tenant with the ledger and purchases installed, and the accounts a bill
    /// touches already open.
    async fn new() -> Self {
        let fixture = Self::bare().await;
        for (account, kind) in [
            ("1010", AccountKind::Asset),     // Bank
            ("1200", AccountKind::Asset),     // Input VAT
            ("2000", AccountKind::Liability), // Accounts payable
            ("5000", AccountKind::Expense),   // Cost of sales
            ("5100", AccountKind::Expense),   // Rent
            ("5200", AccountKind::Expense),   // Other
        ] {
            fixture.open(account, kind).await;
        }
        fixture
    }

    async fn bare() -> Self {
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

        let tenant = control
            .register_tenant_on("najd", "Najd", "primary", Actor::system())
            .await
            .expect("tenant registers");
        spa_testkit::create_named_database(&tenant.database_name, &TENANT)
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
        ledger::install(&mut conn).await.expect("ledger schema");
        ensure_group_schema::<Ledger>(&mut conn)
            .await
            .expect("ledger checkpoint");
        purchases::install(&mut conn)
            .await
            .expect("purchases schema");
        ensure_group_schema::<Purchases>(&mut conn)
            .await
            .expect("purchases checkpoint");
        drop(conn);

        Self {
            db,
            _control: control,
            _control_db: control_db,
            tenant_database: tenant.database_name,
        }
    }

    async fn open(&self, account: &str, kind: AccountKind) {
        open_account(
            &self.db,
            &code(account),
            account,
            kind,
            sar(),
            &Metadata::default(),
        )
        .await
        .expect("opens");
    }

    /// Drives both groups to the head of the log.
    async fn project(&self) {
        let pool = self.tenant_pool().await;

        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("ledger projects");

        let owned = purchases::projections();
        let refs: Vec<&dyn Projection<Group = Purchases>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Purchases>(&pool, &refs, purchases::upcasters(), 200)
            .await
            .expect("purchases projects");

        pool.close().await;
    }

    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = spa_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    async fn balance(&self, account: &str) -> Money {
        let mut conn = self.db.acquire().await.expect("connection");
        let accounts = account_balances(&mut conn).await.expect("reads");
        accounts
            .into_iter()
            .find(|a| a.code == account)
            .map_or_else(|| money(0), |a| a.balance)
    }

    async fn balances(&self) -> Vec<ledger::TrialBalance> {
        let mut conn = self.db.acquire().await.expect("connection");
        trial_balance(&mut conn).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        let _ = spa_testkit::drop_named_database(&self.tenant_database).await;
    }
}

async fn record(fixture: &Fixture, id: &str, lines: Vec<BillLine>) -> Outcome {
    record_bill(&fixture.db, &code(id), &draft(lines), &Metadata::default()).await
}

async fn pay(fixture: &Fixture, id: &str, reference: &str, amount: Money) -> Outcome {
    pay_bill(
        &fixture.db,
        &code(id),
        &Payment {
            reference: reference.to_owned(),
            amount,
            paid_on: on("2026-03-04"),
            from: code("1010"),
        },
        &Metadata::default(),
    )
    .await
}

type Outcome = Result<spa_eventlog::Committed<purchases::BillEvent>, CommandError<PurchaseError>>;

fn rejection(error: &CommandError<PurchaseError>) -> Option<&PurchaseError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

/// **The claim this module is built on.**
///
/// A supplier whose rounding lands a halala away from 15% is recorded as they
/// stated it, and the ledger carries their figure. Recomputing would produce a
/// reclaim that does not match the document evidencing it — and the document is
/// what an inspector asks to see.
#[tokio::test]
async fn what_the_supplier_charged_is_what_goes_in_the_books() {
    let fixture = Fixture::new().await;

    // 15% of 1,000.00 is 150.00. This supplier charged 149.99.
    record(
        &fixture,
        "BILL-1",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            money(14_999),
        )],
    )
    .await
    .expect("records");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1200").await,
        money(14_999),
        "the ledger carries the supplier's figure, not ours"
    );
    assert_eq!(fixture.balance("5000").await, riyals(1_000));
    assert_eq!(
        fixture.balance("2000").await,
        money(-114_999),
        "and what is owed is the two together"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let bill = purchases::bill(&mut conn, "BILL-1")
        .await
        .expect("reads")
        .expect("is there");
    drop(conn);
    assert_eq!(bill.summary.tax, money(14_999));
    assert_eq!(bill.summary.gross, money(114_999));

    assert!(
        fixture
            .balances()
            .await
            .iter()
            .all(ledger::TrialBalance::balances)
    );
    fixture.cleanup().await;
}

/// A bill splits across the accounts its lines name, and only reclaimable tax
/// reaches input VAT.
#[tokio::test]
async fn a_bill_lands_in_the_accounts_its_lines_name() {
    let fixture = Fixture::new().await;

    record(
        &fixture,
        "BILL-MIXED",
        vec![
            line("5000", riyals(1_000), VatCategory::Standard, riyals(150)),
            line("5100", riyals(400), VatCategory::Zero, money(0)),
            line("5200", riyals(100), VatCategory::Exempt, money(0)),
        ],
    )
    .await
    .expect("records");
    fixture.project().await;

    assert_eq!(fixture.balance("5000").await, riyals(1_000));
    assert_eq!(fixture.balance("5100").await, riyals(400));
    assert_eq!(fixture.balance("5200").await, riyals(100));
    assert_eq!(fixture.balance("1200").await, riyals(150));
    assert_eq!(fixture.balance("2000").await, riyals(-1_650));

    fixture.cleanup().await;
}

/// Recording the same bill twice is a no-op, which is what makes a retry safe.
#[tokio::test]
async fn re_recording_a_bill_changes_nothing() {
    let fixture = Fixture::new().await;
    let lines = vec![line(
        "5000",
        riyals(1_000),
        VatCategory::Standard,
        riyals(150),
    )];

    record(&fixture, "BILL-1", lines.clone())
        .await
        .expect("records");
    let again = record(&fixture, "BILL-1", lines).await.expect("is a no-op");
    assert!(again.did_nothing());
    fixture.project().await;

    assert_eq!(
        fixture.balance("2000").await,
        riyals(-1_150),
        "the second attempt posted a second time"
    );

    fixture.cleanup().await;
}

/// The same supplier invoice number recorded twice is a duplicate reclaim, and
/// the read model refuses it.
#[tokio::test]
async fn the_same_supplier_invoice_cannot_be_recorded_twice() {
    let fixture = Fixture::new().await;
    let lines = vec![line(
        "5000",
        riyals(1_000),
        VatCategory::Standard,
        riyals(150),
    )];

    record(&fixture, "BILL-1", lines.clone())
        .await
        .expect("records");
    // A different key of ours, the same document of theirs.
    record(&fixture, "BILL-2", lines.clone())
        .await
        .expect("the command does not know about the other one");

    // The projection is where it is caught, by a constraint that holds against a
    // rebuild too. ponytail: this surfaces as a stopped projection rather than a
    // 409, which is loud and unhelpful — the check belongs in the command, and
    // needs a read model the command may consult (`ledger::accepts_postings` is
    // the pattern). Worth building; the constraint is what stops the money being
    // wrong in the meantime.
    let pool = fixture.tenant_pool().await;
    let owned = purchases::projections();
    let refs: Vec<&dyn Projection<Group = Purchases>> = owned.iter().map(AsRef::as_ref).collect();
    let stopped = run_to_head::<Purchases>(&pool, &refs, purchases::upcasters(), 200).await;
    pool.close().await;

    assert!(
        stopped.is_err(),
        "a duplicate supplier invoice was projected as if it were a second bill"
    );

    fixture.cleanup().await;
}

/// Paying a supplier moves the debt into the bank.
#[tokio::test]
async fn paying_a_bill_settles_it() {
    let fixture = Fixture::new().await;

    record(
        &fixture,
        "BILL-1",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            riyals(150),
        )],
    )
    .await
    .expect("records");
    pay(&fixture, "BILL-1", "TRF-1", riyals(1_150))
        .await
        .expect("pays");
    fixture.project().await;

    assert_eq!(fixture.balance("2000").await, money(0), "nothing owed");
    assert_eq!(fixture.balance("1010").await, riyals(-1_150), "cash gone");

    let mut conn = fixture.db.acquire().await.expect("connection");
    let bill = purchases::bill(&mut conn, "BILL-1")
        .await
        .expect("reads")
        .expect("is there");
    assert_eq!(bill.summary.outstanding, money(0));
    assert_eq!(bill.payments.len(), 1);
    assert!(
        purchases::overpaid(&mut conn)
            .await
            .expect("checks")
            .is_empty(),
        "the invariant this module contributes"
    );
    drop(conn);

    fixture.cleanup().await;
}

/// The same payment reference twice is a retry, not a second payment.
#[tokio::test]
async fn the_same_payment_reference_is_recorded_once() {
    let fixture = Fixture::new().await;
    record(
        &fixture,
        "BILL-1",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            riyals(150),
        )],
    )
    .await
    .expect("records");

    pay(&fixture, "BILL-1", "TRF-1", riyals(500))
        .await
        .expect("pays");
    let again = pay(&fixture, "BILL-1", "TRF-1", riyals(500))
        .await
        .expect("is a no-op");
    assert!(again.did_nothing());
    fixture.project().await;

    assert_eq!(
        fixture.balance("1010").await,
        riyals(-500),
        "the money left twice"
    );

    fixture.cleanup().await;
}

/// Paying more than is owed is refused rather than parked as a negative payable.
#[tokio::test]
async fn an_overpayment_is_refused() {
    let fixture = Fixture::new().await;
    record(
        &fixture,
        "BILL-1",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            riyals(150),
        )],
    )
    .await
    .expect("records");

    let refused = pay(&fixture, "BILL-1", "TRF-1", riyals(2_000)).await;
    assert!(matches!(
        rejection(&refused.expect_err("is refused")),
        Some(PurchaseError::Overpayment { .. })
    ));

    fixture.cleanup().await;
}

/// A bill that cannot post leaves nothing behind — the same guarantee sales
/// makes, inherited from the same transaction boundary.
#[tokio::test]
async fn a_failed_posting_leaves_no_bill_behind() {
    let fixture = Fixture::bare().await;

    let refused = record(
        &fixture,
        "BILL-1",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            riyals(150),
        )],
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(PurchaseError::Ledger(ledger::LedgerError::NoSuchAccount(_)))
        ),
        "a tenant with no chart cannot post"
    );

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    assert!(
        purchases::bills(&mut conn, 100, None)
            .await
            .expect("reads")
            .items
            .is_empty(),
        "a bill exists that has no accounting behind it"
    );
    drop(conn);

    fixture.cleanup().await;
}

/// **The closed-period check reaches here without this module mentioning it.**
///
/// `purchases` has no idea a fiscal period exists. Every posting goes through
/// `ledger::post_entry_in`, which is where the one check lives — so a bill with
/// a back-dated tax point is refused by a rule written in another module before
/// this one existed.
#[tokio::test]
async fn a_bill_cannot_be_dated_into_a_closed_period() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-04-01")), Some("the-accountant"))
        .await
        .expect("closes the first quarter");
    drop(conn);

    let refused = record(
        &fixture,
        "BILL-BACKDATED",
        vec![line(
            "5000",
            riyals(1_000),
            VatCategory::Standard,
            riyals(150),
        )],
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(PurchaseError::Ledger(
                ledger::LedgerError::PeriodClosed { .. }
            ))
        ),
        "a bill was dated into a quarter whose return has been filed"
    );

    fixture.cleanup().await;
}

/// The input side of a return, by rate and by period.
#[tokio::test]
async fn input_tax_is_reported_on_the_suppliers_tax_point() {
    let fixture = Fixture::new().await;

    let mut february = draft(vec![line(
        "5000",
        riyals(1_000),
        VatCategory::Standard,
        riyals(150),
    )]);
    february.supplier_reference = "NS-FEB".to_owned();
    february.billed_on = on("2026-02-10");
    record_bill(
        &fixture.db,
        &code("BILL-FEB"),
        &february,
        &Metadata::default(),
    )
    .await
    .expect("records");

    let mut april = draft(vec![line(
        "5100",
        riyals(400),
        VatCategory::Standard,
        riyals(60),
    )]);
    april.supplier_reference = "NS-APR".to_owned();
    april.billed_on = on("2026-04-10");
    record_bill(&fixture.db, &code("BILL-APR"), &april, &Metadata::default())
        .await
        .expect("records");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let q1 = purchases::input_tax(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    let q2 = purchases::input_tax(&mut conn, sar(), on("2026-04-01"), on("2026-07-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(q1.tax, riyals(150), "the February bill is a Q1 reclaim");
    assert_eq!(q1.net, riyals(1_000));
    assert_eq!(q2.tax, riyals(60), "and the April one is a Q2 reclaim");

    fixture.cleanup().await;
}

/// **Exempt purchases are reported and their tax is not reclaimed.**
///
/// The distinction `VatCategory::Zero` and `VatCategory::Exempt` exist to carry,
/// on the side of the return where it costs money to get wrong.
#[tokio::test]
async fn exempt_input_tax_is_never_reclaimed() {
    let fixture = Fixture::new().await;

    // A supplier who charged tax on an exempt supply is refused outright, so
    // the only way an exempt line carries tax is a rebuild of an older event.
    // What the return must never do is *claim* it — checked here through the
    // view, which is what `input_tax` reads.
    record(
        &fixture,
        "BILL-EXEMPT",
        vec![
            line("5000", riyals(1_000), VatCategory::Standard, riyals(150)),
            line("5200", riyals(500), VatCategory::Exempt, money(0)),
        ],
    )
    .await
    .expect("records");
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    // Force the case the command refuses: an exempt line with tax on it.
    sqlx::query("UPDATE proj_purchases.bill_line SET tax = 7500 WHERE vat_category = 'exempt'")
        .execute(&pool)
        .await
        .expect("writes");
    pool.close().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = purchases::input_tax(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(
        filed.tax,
        riyals(150),
        "exempt input tax was reclaimed; it is a cost, not a debt ZATCA owes back"
    );
    assert_eq!(
        filed.net,
        riyals(1_500),
        "but the exempt purchase itself is still reported"
    );

    fixture.cleanup().await;
}

/// A rebuild reproduces the read models exactly.
#[tokio::test]
async fn bills_replay_to_exactly_what_is_live() {
    let fixture = Fixture::new().await;

    for n in 1..=5 {
        let mut bill = draft(vec![line(
            "5000",
            riyals(100 * n),
            VatCategory::Standard,
            riyals(15 * n),
        )]);
        bill.supplier_reference = format!("NS-{n}");
        record_bill(
            &fixture.db,
            &code(&format!("BILL-{n}")),
            &bill,
            &Metadata::default(),
        )
        .await
        .expect("records");
        if n % 2 == 0 {
            pay(
                &fixture,
                &format!("BILL-{n}"),
                &format!("TRF-{n}"),
                riyals(50),
            )
            .await
            .expect("pays");
        }
    }
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let owned = purchases::projections();
    let refs: Vec<&dyn Projection<Group = Purchases>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Purchases>(&pool, &refs, purchases::upcasters(), 200)
        .await
        .expect("replays");
    pool.close().await;

    assert!(
        report.is_reproducible(),
        "a rebuild does not reproduce the live tables: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every event name this module writes is a valid one.
#[test]
fn names_are_valid() {
    for name in purchases::BillEvent::NAMES {
        assert!(
            spa_types::EventName::new(name).is_ok(),
            "{name} is not a usable event name"
        );
    }
}
