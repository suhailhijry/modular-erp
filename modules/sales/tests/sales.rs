//! Sales end to end, against a real tenant with both modules installed.
//!
//! The test that carries this module is
//! [`a_failed_posting_leaves_no_invoice_behind`]. Everything else checks a rule;
//! that one checks the claim the module was built to make — that an invoice and
//! its journal entry commit together — and it is the test that fails if anyone
//! ever splits them across two transactions or moves the posting to the outbox.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use ledger::{AccountKind, Ledger, account_balances, open_account, trial_balance};
use sales::{
    Customer, Draft, InvoiceLine, Receipt, Sales, SalesError, Vat, VatCategory, issue_invoice,
    record_payment,
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
fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("valid")
}
fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}
fn when() -> Timestamp {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid")
}
fn money(minor: i64) -> Money {
    Money::from_minor(minor, sar())
}
/// Whole riyals, so tests read in the units a person uses.
fn riyals(major: i64) -> Money {
    money(major * 100)
}

fn line(description: &str, net: Money, category: VatCategory) -> InvoiceLine {
    InvoiceLine {
        description: description.to_owned(),
        net,
        vat: Vat::current(category),
    }
}

fn draft(lines: Vec<InvoiceLine>) -> Draft {
    Draft {
        customer: Customer::new("Rawabi Trading").with_vat_number("310000000000003"),
        issued_on: when(),
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
    /// A tenant with both modules installed and the conventional accounts open.
    async fn new() -> Self {
        let fixture = Self::bare().await;
        for (account, kind) in [
            ("1010", AccountKind::Asset),     // Bank
            ("1100", AccountKind::Asset),     // Accounts receivable
            ("2100", AccountKind::Liability), // VAT payable
            ("4000", AccountKind::Revenue),   // Revenue
        ] {
            fixture.open(account, kind, sar()).await;
        }
        fixture
    }

    /// The same tenant with no accounts at all, for the tests about what happens
    /// when the ledger refuses.
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
            .register_tenant_on("rawabi", "Rawabi", "primary", Actor::system())
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
        sales::install(&mut conn).await.expect("sales schema");
        ensure_group_schema::<Sales>(&mut conn)
            .await
            .expect("sales checkpoint");
        drop(conn);

        Self {
            db,
            _control: control,
            _control_db: control_db,
            tenant_database: tenant.database_name,
        }
    }

    async fn open(&self, account: &str, kind: AccountKind, currency: CurrencyCode) {
        open_account(
            &self.db,
            &code(account),
            account,
            kind,
            currency,
            &Metadata::default(),
        )
        .await
        .expect("opens");
    }

    /// Drives **both** groups to the head of the log.
    ///
    /// They are separate groups over one log, which is the arrangement the whole
    /// module rests on: they never read each other's tables and each keeps its
    /// own checkpoint.
    async fn project(&self) {
        let pool = self.tenant_pool().await;

        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("ledger projects");

        let owned = sales::projections();
        let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<Sales>(&pool, &refs, sales::upcasters(), 200)
            .await
            .expect("sales projects");

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

    async fn invoice(&self, id: &str) -> Option<sales::InvoiceDetail> {
        let mut conn = self.db.acquire().await.expect("connection");
        sales::invoice(&mut conn, id).await.expect("reads")
    }

    /// Whether the *event log* has an issued invoice under this id — the
    /// question the read models cannot answer, because a projection that has not
    /// run yet also produces no row.
    async fn is_issued(&self, id: &str) -> bool {
        let mut conn = self.db.acquire().await.expect("connection");
        spa_eventlog::load::<sales::Invoice>(&mut conn, &code(id), sales::upcasters())
            .await
            .expect("loads")
            .aggregate
            .issued
    }

    async fn imbalances(&self) -> Vec<ledger::TrialBalance> {
        let mut conn = self.db.acquire().await.expect("connection");
        ledger::imbalances(&mut conn).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        let _ = spa_testkit::drop_named_database(&self.tenant_database).await;
    }
}

async fn issue(fixture: &Fixture, id: &str, lines: Vec<InvoiceLine>) -> Outcome {
    issue_invoice(&fixture.db, &code(id), &draft(lines), &Metadata::default()).await
}

async fn pay(fixture: &Fixture, id: &str, reference: &str, amount: Money) -> Outcome {
    record_payment(
        &fixture.db,
        &code(id),
        &Receipt {
            reference: reference.to_owned(),
            amount,
            received_on: when(),
            into: code("1010"),
        },
        &Metadata::default(),
    )
    .await
}

async fn credit(fixture: &Fixture, invoice: &str, note: &str) -> Outcome {
    sales::cancel_invoice(
        &fixture.db,
        &code(invoice),
        note,
        "issued in error",
        when(),
        &Metadata::default(),
    )
    .await
}

type Outcome = Result<spa_eventlog::Committed<sales::InvoiceEvent>, CommandError<SalesError>>;

fn rejection(error: &CommandError<SalesError>) -> Option<&SalesError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn issuing_an_invoice_posts_it_to_the_ledger() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-1001",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    // The sales side.
    let invoice = fixture.invoice("INV-1001").await.expect("is there");
    assert_eq!(invoice.summary.net, riyals(1_000));
    assert_eq!(invoice.summary.tax, riyals(150), "15% of 1,000");
    assert_eq!(invoice.summary.gross, riyals(1_150));
    assert_eq!(invoice.summary.outstanding, riyals(1_150));
    assert_eq!(invoice.summary.customer, "Rawabi Trading");
    assert_eq!(invoice.lines.len(), 1);

    // The accounting side — the same sale, in the books, without sales having
    // touched a single ledger table.
    assert_eq!(fixture.balance("1100").await, riyals(1_150), "receivable");
    assert_eq!(fixture.balance("4000").await, riyals(-1_000), "revenue");
    assert_eq!(fixture.balance("2100").await, riyals(-150), "VAT payable");
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_failed_posting_leaves_no_invoice_behind() {
    // The module's whole claim, as an experiment. `bare` has no accounts, so the
    // ledger refuses — and the invoice event must not survive that.
    let fixture = Fixture::bare().await;

    let error = issue(
        &fixture,
        "INV-2001",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect_err("the ledger has no account 1100");

    assert!(
        matches!(
            rejection(&error),
            Some(SalesError::Ledger(ledger::LedgerError::NoSuchAccount(_)))
        ),
        "expected the ledger's own rejection, got {error:?}"
    );

    assert!(
        !fixture.is_issued("INV-2001").await,
        "the invoice rolled back with the posting"
    );

    // Not vacuous: the same request succeeds once the accounts exist, so the
    // assertion above is about the rollback and not about the command never
    // working.
    for (account, kind) in [
        ("1100", AccountKind::Asset),
        ("2100", AccountKind::Liability),
        ("4000", AccountKind::Revenue),
    ] {
        fixture.open(account, kind, sar()).await;
    }
    issue(
        &fixture,
        "INV-2001",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues now");
    assert!(fixture.is_issued("INV-2001").await);

    fixture.cleanup().await;
}

#[tokio::test]
async fn re_issuing_the_same_invoice_changes_nothing() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-1002",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    // A retried request, with different lines — a client that got a timeout and
    // rebuilt its payload badly.
    let second = issue(
        &fixture,
        "INV-1002",
        vec![line("Consulting", riyals(9_999), VatCategory::Standard)],
    )
    .await
    .expect("is not an error");

    assert!(
        second.events.is_empty(),
        "a re-issue writes nothing; the stored invoice wins"
    );

    fixture.project().await;
    let invoice = fixture.invoice("INV-1002").await.expect("is there");
    assert_eq!(invoice.summary.net, riyals(1_000), "the first one stands");
    assert_eq!(
        fixture.balance("1100").await,
        riyals(1_150),
        "and it was posted exactly once"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_payment_clears_the_receivable_without_touching_revenue() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-1003",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    pay(&fixture, "INV-1003", "wire-88", riyals(1_150))
        .await
        .expect("records");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "receivable cleared"
    );
    assert_eq!(fixture.balance("1010").await, riyals(1_150), "bank took it");
    assert_eq!(
        fixture.balance("4000").await,
        riyals(-1_000),
        "revenue was recognised once, at issue"
    );

    let invoice = fixture.invoice("INV-1003").await.expect("is there");
    assert_eq!(invoice.summary.paid, riyals(1_150));
    assert_eq!(invoice.summary.outstanding, money(0));
    assert_eq!(invoice.payments.len(), 1);
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_part_payment_leaves_the_rest_outstanding() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-1004",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    pay(&fixture, "INV-1004", "wire-1", riyals(500))
        .await
        .expect("records");
    pay(&fixture, "INV-1004", "wire-2", riyals(650))
        .await
        .expect("records");
    fixture.project().await;

    let invoice = fixture.invoice("INV-1004").await.expect("is there");
    assert_eq!(invoice.summary.outstanding, money(0));
    assert_eq!(invoice.payments.len(), 2);
    assert_eq!(fixture.balance("1100").await, money(0));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_same_payment_reference_is_recorded_once() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-1005",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    pay(&fixture, "INV-1005", "wire-7", riyals(500))
        .await
        .expect("records");
    let again = pay(&fixture, "INV-1005", "wire-7", riyals(500))
        .await
        .expect("is not an error");

    assert!(again.events.is_empty(), "the second is a no-op");
    fixture.project().await;

    let invoice = fixture.invoice("INV-1005").await.expect("is there");
    assert_eq!(invoice.summary.paid, riyals(500), "counted once");
    assert_eq!(
        fixture.balance("1010").await,
        riyals(500),
        "and posted to the bank once"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_overpayment_is_refused_rather_than_parked() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-1006",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let error = pay(&fixture, "INV-1006", "wire-9", riyals(200))
        .await
        .expect_err("115.00 is outstanding");

    assert!(matches!(
        rejection(&error),
        Some(SalesError::Overpayment { .. })
    ));

    fixture.project().await;
    assert_eq!(
        fixture.balance("1010").await,
        money(0),
        "nothing reached the bank account either"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_payment_against_an_invoice_that_does_not_exist_is_refused() {
    let fixture = Fixture::new().await;

    let error = pay(&fixture, "INV-NOPE", "wire-1", riyals(100))
        .await
        .expect_err("there is no such invoice");

    assert!(matches!(rejection(&error), Some(SalesError::NotIssued(_))));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_payment_in_another_currency_is_refused() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-1007",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let error = pay(
        &fixture,
        "INV-1007",
        "wire-usd",
        Money::from_minor(10_000, usd()),
    )
    .await
    .expect_err("the invoice is in SAR");

    assert!(matches!(
        rejection(&error),
        Some(SalesError::PaymentCurrency { .. })
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_invoice_with_nothing_on_it_is_refused() {
    let fixture = Fixture::new().await;

    let empty = issue(&fixture, "INV-1008", vec![]).await;
    assert!(matches!(
        rejection(&empty.expect_err("no lines")),
        Some(SalesError::NothingToInvoice)
    ));

    // Lines that cancel out exactly are the same thing wearing a disguise.
    let cancels = issue(
        &fixture,
        "INV-1009",
        vec![
            line("Work", riyals(100), VatCategory::Exempt),
            line("Discount", riyals(-100), VatCategory::Exempt),
        ],
    )
    .await;
    assert!(matches!(
        rejection(&cancels.expect_err("comes to nothing")),
        Some(SalesError::NothingToInvoice)
    ));

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_mixed_rate_invoice_prints_a_band_per_rate() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-1010",
        vec![
            line("Consulting", riyals(1_000), VatCategory::Standard),
            line("Export", riyals(500), VatCategory::Zero),
            line("Residential rent", riyals(300), VatCategory::Exempt),
            line("More consulting", riyals(200), VatCategory::Standard),
        ],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let invoice = fixture.invoice("INV-1010").await.expect("is there");
    assert_eq!(invoice.lines.len(), 4);
    assert_eq!(invoice.tax.len(), 3, "one band per category present");

    let standard = invoice
        .tax
        .iter()
        .find(|b| b.category == VatCategory::Standard)
        .expect("a standard band");
    assert_eq!(standard.net, riyals(1_200), "both standard lines, summed");
    assert_eq!(standard.tax, riyals(180));

    assert_eq!(invoice.summary.net, riyals(2_000));
    assert_eq!(invoice.summary.tax, riyals(180));
    assert_eq!(fixture.balance("2100").await, riyals(-180));

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_vat_account_holds_exactly_what_the_invoices_charged() {
    // The cross-module invariant a business actually cares about: what ZATCA is
    // owed, according to sales, equals what the ledger says it is owed. Two
    // independent read models, computed from the same log by different code.
    let fixture = Fixture::new().await;

    for (n, net) in [333_i64, 1_777, 10].into_iter().enumerate() {
        issue(
            &fixture,
            &format!("INV-20{n}"),
            vec![
                line("Consulting", money(net), VatCategory::Standard),
                line("Export", money(net * 3), VatCategory::Zero),
            ],
        )
        .await
        .expect("issues");
    }
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let charged: i64 = sales::invoices(&mut conn, 100)
        .await
        .expect("reads")
        .iter()
        .map(|i| i.tax.minor())
        .sum();
    drop(conn);

    assert!(charged > 0, "the test would pass vacuously at zero");
    assert_eq!(
        fixture.balance("2100").await,
        money(-charged),
        "VAT payable is a credit balance of exactly what was charged"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_trial_balance_stays_zero_across_a_run_of_sales() {
    // The pipeline canary, borrowed from the ledger's own suite. It can only be
    // zero if commands, events, both projections and the read models are all
    // right — and sales is now one of the things writing to it.
    let fixture = Fixture::new().await;

    for n in 0..12_i64 {
        let id = format!("INV-30{n}");
        issue(
            &fixture,
            &id,
            vec![
                line("Consulting", money(n * 977 + 13), VatCategory::Standard),
                line("Export", money(n * 31 + 7), VatCategory::Zero),
            ],
        )
        .await
        .expect("issues");

        if n % 3 == 0 {
            pay(&fixture, &id, "wire", money(n * 7 + 1))
                .await
                .expect("records");
        }
    }
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let balance = trial_balance(&mut conn).await.expect("reads");
    let overpaid = sales::overpaid(&mut conn).await.expect("reads");
    drop(conn);

    assert!(!balance.is_empty(), "there should be something to balance");
    assert!(
        balance.iter().all(ledger::TrialBalance::balances),
        "the books do not balance: {balance:?}"
    );
    assert!(overpaid.is_empty(), "nothing is overpaid: {overpaid:?}");

    fixture.cleanup().await;
}

#[tokio::test]
async fn each_group_replays_to_exactly_what_is_live() {
    let fixture = Fixture::new().await;

    for n in 0..6_i64 {
        let id = format!("INV-40{n}");
        issue(
            &fixture,
            &id,
            vec![
                line("Consulting", money(n * 811 + 101), VatCategory::Standard),
                line("Rent", money(n * 53 + 11), VatCategory::Exempt),
            ],
        )
        .await
        .expect("issues");
        pay(&fixture, &id, &format!("wire-{n}"), money(n + 1))
            .await
            .expect("records");
    }
    fixture.project().await;

    let pool = fixture.tenant_pool().await;

    let owned = sales::projections();
    let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
    let sales_report = replay_shadow::<Sales>(&pool, &refs, sales::upcasters(), 200)
        .await
        .expect("replays");

    // The ledger too, because sales is now writing events into the same log and
    // a rebuild has to reproduce *both* sides of it.
    let owned = ledger::projections();
    let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
    let ledger_report = replay_shadow::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
        .await
        .expect("replays");

    pool.close().await;

    assert!(
        sales_report.is_reproducible(),
        "sales does not rebuild to what is live: {:?}",
        sales_report.differences()
    );
    assert!(
        ledger_report.is_reproducible(),
        "the ledger does not rebuild to what is live: {:?}",
        ledger_report.differences()
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn a_sales_posting_cannot_collide_with_a_hand_written_entry() {
    // Sales derives its journal entry ids from the invoice number. If it used
    // the number unprefixed, an entry someone had already posted by hand under
    // that id would absorb the sale silently — posting an existing entry id is a
    // no-op by design.
    let fixture = Fixture::new().await;

    let lines = ledger::BalancedLines::new(vec![
        ledger::Line::new(code("1010"), riyals(500)),
        ledger::Line::new(code("4000"), riyals(-500)),
    ])
    .expect("balances");

    ledger::post_entry(
        &fixture.db,
        &code("INV-5001"),
        when(),
        "posted by hand",
        lines,
        &Metadata::default(),
    )
    .await
    .expect("posts");

    issue(
        &fixture,
        "INV-5001",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues despite the name clash");
    fixture.project().await;

    assert_eq!(
        fixture.balance("4000").await,
        riyals(-1_500),
        "both the manual entry and the sale reached revenue"
    );
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

#[tokio::test]
async fn an_invoice_records_the_customer_as_they_were() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-6001",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let invoice = fixture.invoice("INV-6001").await.expect("is there");
    assert_eq!(invoice.summary.customer, "Rawabi Trading");
    assert_eq!(
        invoice.summary.customer_vat.as_deref(),
        Some("310000000000003"),
        "the buyer's VAT number is on the document, not behind a join"
    );

    fixture.cleanup().await;
}

#[tokio::test]
async fn the_rate_on_a_line_is_the_rate_that_applied() {
    // The reason `Vat` carries basis points at all: a future rate change must
    // leave old invoices alone. This asserts the rate reaches storage, which is
    // what makes that possible.
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-6002",
        vec![
            line("Consulting", riyals(100), VatCategory::Standard),
            line("Export", riyals(100), VatCategory::Zero),
        ],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let invoice = fixture.invoice("INV-6002").await.expect("is there");
    let standard = &invoice.lines[0];
    assert_eq!(standard.category, VatCategory::Standard);
    assert_eq!(standard.basis_points, 1_500, "15%, stored on the line");
    assert_eq!(invoice.lines[1].basis_points, 0);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// **The requirement.** A tenant whose chart does not use the conventional
/// codes tells sales where to post, and it posts there.
#[tokio::test]
async fn a_tenant_can_choose_which_accounts_a_sale_posts_to() {
    let fixture = Fixture::new().await;

    // A chart of their own, alongside the conventional one.
    for (account, kind) in [
        ("AR", AccountKind::Asset),
        ("SALES", AccountKind::Revenue),
        ("VAT-OUT", AccountKind::Liability),
    ] {
        fixture.open(account, kind, sar()).await;
    }

    let mut conn = fixture.db.acquire().await.expect("connection");
    spa_eventlog::configuration::set(
        &mut conn,
        sales::PostingAccounts::KEY,
        &sales::PostingAccounts {
            receivable: code("AR"),
            revenue: code("SALES"),
            output_vat: code("VAT-OUT"),
        },
        Some("owner"),
    )
    .await
    .expect("configures");
    drop(conn);

    issue(
        &fixture,
        "INV-CFG-1",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    assert_eq!(fixture.balance("AR").await, riyals(1_150));
    assert_eq!(fixture.balance("SALES").await, riyals(-1_000));
    assert_eq!(fixture.balance("VAT-OUT").await, riyals(-150));

    // And the conventional accounts were left alone, which is what makes this
    // about configuration rather than about there being two charts.
    assert_eq!(fixture.balance("1100").await, money(0));
    assert_eq!(fixture.balance("4000").await, money(0));

    fixture.cleanup().await;
}

/// **Changing configuration does not restate history.**
///
/// Architecture L5: the event carries the resolved accounts, not a reference to
/// the configuration. An invoice issued before the change stays where it was
/// posted, and a replay reproduces it — which is the property that would break
/// if a projection resolved config at read time.
#[tokio::test]
async fn changing_where_sales_post_leaves_earlier_invoices_alone() {
    let fixture = Fixture::new().await;
    for (account, kind) in [("AR", AccountKind::Asset), ("SALES", AccountKind::Revenue)] {
        fixture.open(account, kind, sar()).await;
    }

    // Issued against the shipped defaults.
    issue(
        &fixture,
        "INV-CFG-BEFORE",
        vec![line("Consulting", riyals(100), VatCategory::Zero)],
    )
    .await
    .expect("issues");

    let mut conn = fixture.db.acquire().await.expect("connection");
    spa_eventlog::configuration::set(
        &mut conn,
        sales::PostingAccounts::KEY,
        &sales::PostingAccounts {
            receivable: code("AR"),
            revenue: code("SALES"),
            output_vat: code("2100"),
        },
        Some("owner"),
    )
    .await
    .expect("configures");
    drop(conn);

    issue(
        &fixture,
        "INV-CFG-AFTER",
        vec![line("Consulting", riyals(200), VatCategory::Zero)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    assert_eq!(
        fixture.balance("1100").await,
        riyals(100),
        "the earlier invoice is exactly where it was posted"
    );
    assert_eq!(fixture.balance("AR").await, riyals(200));
    assert!(fixture.imbalances().await.is_empty());

    // The books still rebuild from the log, which they could not if the
    // accounts were resolved at read time.
    let pool = fixture.tenant_pool().await;
    let owned = ledger::projections();
    let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
        .await
        .expect("replays");
    pool.close().await;
    assert!(report.is_reproducible(), "{:?}", report.differences());

    fixture.cleanup().await;
}

/// A command records which generation of configuration it decided against.
#[tokio::test]
async fn a_command_stamps_the_configuration_it_resolved_against() {
    let fixture = Fixture::new().await;

    let committed = issue(
        &fixture,
        "INV-CFG-2",
        vec![line("Consulting", riyals(100), VatCategory::Zero)],
    )
    .await
    .expect("issues");

    let position = committed.at.expect("wrote an event");
    let mut conn = fixture.db.acquire().await.expect("connection");

    let unconfigured: Option<i64> = sqlx::query_scalar(
        "SELECT (metadata->>'config_version')::BIGINT FROM event WHERE position = $1",
    )
    .bind(position.get())
    .fetch_one(&mut *conn)
    .await
    .expect("reads");
    assert_eq!(
        unconfigured,
        Some(0),
        "nothing configured is a real answer, not a missing one"
    );

    spa_eventlog::configuration::set(
        &mut conn,
        sales::PostingAccounts::KEY,
        &sales::PostingAccounts::conventional(),
        Some("owner"),
    )
    .await
    .expect("configures");
    drop(conn);

    let committed = issue(
        &fixture,
        "INV-CFG-3",
        vec![line("Consulting", riyals(100), VatCategory::Zero)],
    )
    .await
    .expect("issues");

    let mut conn = fixture.db.acquire().await.expect("connection");
    let after: Option<i64> = sqlx::query_scalar(
        "SELECT (metadata->>'config_version')::BIGINT FROM event WHERE position = $1",
    )
    .bind(committed.at.expect("wrote an event").get())
    .fetch_one(&mut *conn)
    .await
    .expect("reads");
    drop(conn);

    assert!(
        after > unconfigured,
        "the generation moved: {after:?} should be later than {unconfigured:?}"
    );

    fixture.cleanup().await;
}

/// A stored value that no longer fits its type stops the command rather than
/// falling back to the shipped default.
#[tokio::test]
async fn unusable_configuration_refuses_rather_than_pretending() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    sqlx::query(
        "INSERT INTO configuration (key, value, version)
         VALUES ($1, '{\"receivable\": \"1100\"}'::jsonb, nextval('configuration_version'))",
    )
    .bind(sales::PostingAccounts::KEY)
    .execute(&mut *conn)
    .await
    .expect("writes something unusable");
    drop(conn);

    let error = issue(
        &fixture,
        "INV-CFG-4",
        vec![line("Consulting", riyals(100), VatCategory::Zero)],
    )
    .await
    .expect_err("cannot post against configuration it cannot read");

    assert!(
        matches!(rejection(&error), Some(SalesError::Config(_))),
        "expected a configuration failure, got {error:?}"
    );

    // Nothing was written — a half-configured tenant does not get a half-issued
    // invoice.
    assert!(!fixture.is_issued("INV-CFG-4").await);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Credit notes
// ---------------------------------------------------------------------------

/// **An invoice issued in error can be credited**, and the books show both.
#[tokio::test]
async fn a_credit_note_cancels_an_invoice_and_reverses_its_posting() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-CN-1",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, riyals(1_150));

    sales::cancel_invoice(
        &fixture.db,
        &code("INV-CN-1"),
        "CN-1",
        "wrong customer",
        when(),
        &Metadata::default(),
    )
    .await
    .expect("credits");
    fixture.project().await;

    assert_eq!(fixture.balance("1100").await, money(0), "nothing owed");
    assert_eq!(fixture.balance("4000").await, money(0), "revenue undone");
    assert_eq!(
        fixture.balance("2100").await,
        money(0),
        "and the VAT with it"
    );

    // The invoice is still there — it was issued, and somebody may hold a copy.
    let invoice = fixture.invoice("INV-CN-1").await.expect("is still there");
    assert_eq!(invoice.summary.gross, riyals(1_150), "as issued");
    assert_eq!(
        invoice.summary.outstanding,
        money(0),
        "but nobody owes it, or a receivables list would keep chasing it"
    );
    assert_eq!(invoice.summary.credit_note.as_deref(), Some("CN-1"));
    assert!(invoice.summary.cancelled_on.is_some());

    assert!(fixture.imbalances().await.is_empty());
    fixture.cleanup().await;
}

/// Crediting twice would swing the balance the other way; the same credit note
/// again is a retry.
#[tokio::test]
async fn an_invoice_can_only_be_credited_once() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-CN-2",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    credit(&fixture, "INV-CN-2", "CN-2").await.expect("credits");

    let retry = credit(&fixture, "INV-CN-2", "CN-2")
        .await
        .expect("is not an error");
    assert!(retry.events.is_empty(), "a retry writes nothing");

    let error = credit(&fixture, "INV-CN-2", "CN-2b")
        .await
        .expect_err("already cancelled");
    assert!(
        matches!(
            rejection(&error),
            Some(SalesError::AlreadyCancelled { by, .. }) if by == "CN-2"
        ),
        "{error:?}"
    );

    fixture.project().await;
    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "credited exactly once"
    );
    fixture.cleanup().await;
}

/// An invoice that has been paid cannot simply be cancelled — the money is
/// somewhere, and this system has no way to model the refund.
#[tokio::test]
async fn an_invoice_with_payments_is_refused_rather_than_left_inconsistent() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-CN-3",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-CN-3", "wire-1", riyals(50))
        .await
        .expect("records");

    let error = credit(&fixture, "INV-CN-3", "CN-3")
        .await
        .expect_err("it has been paid");
    assert!(matches!(
        rejection(&error),
        Some(SalesError::HasPayments(_))
    ));

    fixture.project().await;
    let invoice = fixture.invoice("INV-CN-3").await.expect("is there");
    assert!(invoice.summary.cancelled_on.is_none(), "still live");
    assert_eq!(invoice.summary.paid, riyals(50), "and still paid");

    fixture.cleanup().await;
}

/// Crediting an invoice nobody issued does nothing at all.
#[tokio::test]
async fn crediting_an_invoice_that_does_not_exist_leaves_no_trace() {
    let fixture = Fixture::new().await;

    let error = credit(&fixture, "INV-NOPE", "CN-X")
        .await
        .expect_err("there is no such invoice");
    assert!(matches!(rejection(&error), Some(SalesError::NotIssued(_))));

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0));
    fixture.cleanup().await;
}

/// Both sides still rebuild from the log, which is what a new event type is
/// most likely to break.
#[tokio::test]
async fn credited_invoices_replay_to_exactly_what_is_live() {
    let fixture = Fixture::new().await;

    for n in 0..4_i64 {
        let id = format!("INV-CN-R{n}");
        issue(
            &fixture,
            &id,
            vec![line(
                "Consulting",
                money(n * 733 + 41),
                VatCategory::Standard,
            )],
        )
        .await
        .expect("issues");
        if n % 2 == 0 {
            credit(&fixture, &id, &format!("CN-R{n}"))
                .await
                .expect("credits");
        }
    }
    fixture.project().await;

    let pool = fixture.tenant_pool().await;

    let owned = sales::projections();
    let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
    let sales_report = replay_shadow::<Sales>(&pool, &refs, sales::upcasters(), 200)
        .await
        .expect("replays");

    let owned = ledger::projections();
    let refs: Vec<&dyn Projection<Group = Ledger>> = owned.iter().map(AsRef::as_ref).collect();
    let ledger_report = replay_shadow::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
        .await
        .expect("replays");

    pool.close().await;

    assert!(
        sales_report.is_reproducible(),
        "{:?}",
        sales_report.differences()
    );
    assert!(
        ledger_report.is_reproducible(),
        "{:?}",
        ledger_report.differences()
    );
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The VAT return
// ---------------------------------------------------------------------------

fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

/// Issues an invoice on a given date, so a return has periods to separate.
async fn issue_on(fixture: &Fixture, id: &str, day: &str, lines: Vec<InvoiceLine>) -> Outcome {
    issue_invoice(
        &fixture.db,
        &code(id),
        &Draft {
            customer: Customer::new("Rawabi Trading"),
            issued_on: on(day),
            due_on: None,
            currency: sar(),
            lines,
            note: String::new(),
        },
        &Metadata::default(),
    )
    .await
}

/// **What a Saudi business files.** Output tax by rate, for a period.
#[tokio::test]
async fn a_vat_return_reports_what_was_charged_by_rate() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "Q1-A",
        "2026-01-15",
        vec![
            line("Consulting", riyals(1_000), VatCategory::Standard),
            line("Export", riyals(500), VatCategory::Zero),
        ],
    )
    .await
    .expect("issues");
    issue_on(
        &fixture,
        "Q1-B",
        "2026-03-31",
        vec![line("Consulting", riyals(400), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    // The next quarter, which must not appear.
    issue_on(
        &fixture,
        "Q2-A",
        "2026-04-01",
        vec![line("Consulting", riyals(9_999), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(filed.bands.len(), 2, "standard and zero-rated");

    let standard = filed
        .bands
        .iter()
        .find(|b| b.category == VatCategory::Standard)
        .expect("a standard band");
    assert_eq!(standard.net, riyals(1_400), "both quarter-one invoices");
    assert_eq!(standard.tax, riyals(210), "15% of 1,400");
    assert_eq!(standard.invoices, 2);

    assert_eq!(filed.net, riyals(1_900));
    assert_eq!(filed.tax, riyals(210), "the number that goes on the return");

    // The boundary is exclusive, so consecutive returns neither double-count a
    // day nor drop one. Stated as the property rather than as arithmetic: the
    // two quarters together are exactly the whole span.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let q2 = sales::vat_return(&mut conn, sar(), on("2026-04-01"), on("2026-07-01"))
        .await
        .expect("reads");
    let whole = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-07-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(
        q2.net,
        riyals(9_999),
        "the invoice on the boundary day is Q2's"
    );
    assert_eq!(
        filed.tax.minor() + q2.tax.minor(),
        whole.tax.minor(),
        "every riyal charged appears in exactly one of the two"
    );
    assert_eq!(filed.net.minor() + q2.net.minor(), whole.net.minor());

    fixture.cleanup().await;
}

/// A credited invoice is not a supply, so it leaves the return.
#[tokio::test]
async fn a_credited_invoice_drops_out_of_the_return() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "VR-KEEP",
        "2026-02-01",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    issue_on(
        &fixture,
        "VR-DROP",
        "2026-02-02",
        vec![line("Consulting", riyals(3_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let read = async |fixture: &Fixture| {
        let mut conn = fixture.db.acquire().await.expect("connection");
        let filed = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
            .await
            .expect("reads");
        drop(conn);
        filed
    };

    let before = read(&fixture).await;
    assert_eq!(before.tax, riyals(600), "15% of 4,000");

    credit(&fixture, "VR-DROP", "CN-VR").await.expect("credits");
    fixture.project().await;

    let after = read(&fixture).await;
    assert_eq!(
        after.tax,
        riyals(150),
        "only the invoice that still stands is declared"
    );
    assert_eq!(after.bands[0].invoices, 1);

    // And the ledger agrees: the VAT account holds exactly what the return says.
    assert_eq!(fixture.balance("2100").await, riyals(-150));

    fixture.cleanup().await;
}

/// A business with nothing to declare still files, so an empty period is a
/// return with no bands rather than an error.
#[tokio::test]
async fn a_quiet_period_is_an_empty_return_not_a_failure() {
    let fixture = Fixture::new().await;
    issue_on(
        &fixture,
        "VR-OLD",
        "2026-01-05",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = sales::vat_return(&mut conn, sar(), on("2026-07-01"), on("2026-10-01"))
        .await
        .expect("reads");
    drop(conn);

    assert!(filed.bands.is_empty());
    assert_eq!(filed.tax, money(0));
    assert_eq!(filed.net, money(0));

    fixture.cleanup().await;
}

/// The return is per currency: a business invoicing in two does not add them up.
#[tokio::test]
async fn a_return_covers_one_currency() {
    let fixture = Fixture::new().await;
    fixture.open("1101", AccountKind::Asset, usd()).await;

    issue_on(
        &fixture,
        "VR-SAR",
        "2026-02-01",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let sar_return = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    let usd_return = sales::vat_return(&mut conn, usd(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(sar_return.tax, riyals(150));
    assert_eq!(usd_return.currency, usd());
    assert!(
        usd_return.bands.is_empty(),
        "SAR supplies are not USD supplies"
    );

    fixture.cleanup().await;
}
