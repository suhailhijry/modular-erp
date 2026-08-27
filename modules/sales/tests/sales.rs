//! Sales end to end, against a real tenant with both modules installed.
//!
//! The test that carries this module is
//! [`a_failed_posting_leaves_no_invoice_behind`]. Everything else checks a rule;
//! that one checks the claim the module was built to make — that an invoice and
//! its journal entry commit together — and it is the test that fails if anyone
//! ever splits them across two transactions or moves the posting to the outbox.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use ledger::{AccountKind, Ledger, account_balances, open_account, trial_balance};
use sales::{
    Customer, Draft, DraftLine, Receipt, Sales, SalesError, VatCategory, issue_invoice,
    record_payment,
};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

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

fn line(description: &str, net: Money, category: VatCategory) -> DraftLine {
    DraftLine {
        description: description.to_owned(),
        net,
        category,
    }
}

fn draft(lines: Vec<DraftLine>) -> Draft {
    Draft {
        customer: Customer::new("Rawabi Trading").with_vat_number("310000000000003"),
        issued_on: when(),
        due_on: None,
        currency: sar(),
        lines,
        discounts: Vec::new(),
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
            .register_tenant_on("rawabi", "Rawabi", "primary", Actor::system())
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
        let url = erp_testkit::database_url();
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
        erp_eventlog::load::<sales::Invoice>(&mut conn, &code(id), sales::upcasters())
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
        let _ = erp_testkit::drop_named_database(&self.tenant_database).await;
    }
}

async fn issue(fixture: &Fixture, id: &str, lines: Vec<DraftLine>) -> Outcome {
    issue_numbered(fixture, id, lines)
        .await
        .map(|numbered| numbered.committed)
}

/// The same, keeping the allocated number. See `numbering.rs`.
async fn issue_numbered(
    fixture: &Fixture,
    id: &str,
    lines: Vec<DraftLine>,
) -> Result<sales::Numbered, CommandError<SalesError>> {
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
    credit_numbered(fixture, invoice, note)
        .await
        .map(|numbered| numbered.committed)
}

async fn credit_numbered(
    fixture: &Fixture,
    invoice: &str,
    note: &str,
) -> Result<sales::Numbered, CommandError<SalesError>> {
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

type Outcome = Result<erp_eventlog::Committed<sales::InvoiceEvent>, CommandError<SalesError>>;

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
    let charged: i64 = sales::invoices(&mut conn, 100, None)
        .await
        .expect("reads")
        .items
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
    erp_eventlog::configuration::set(
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
    erp_eventlog::configuration::set(
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

    erp_eventlog::configuration::set(
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
    // `CN-1` was the client's key for the cancellation; the credit note's own
    // number comes from the tenant's gapless series.
    assert_eq!(invoice.summary.credit_note.as_deref(), Some("CN-00001"));
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
async fn issue_on(fixture: &Fixture, id: &str, day: &str, lines: Vec<DraftLine>) -> Outcome {
    let issued = issue_invoice(
        &fixture.db,
        &code(id),
        &Draft {
            customer: Customer::new("Rawabi Trading"),
            issued_on: on(day),
            due_on: None,
            currency: sar(),
            lines,
            discounts: Vec::new(),
            note: String::new(),
        },
        &Metadata::default(),
    )
    .await;
    issued.map(|numbered| numbered.committed)
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
async fn a_credit_note_in_the_same_period_nets_the_supply_out() {
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

    // Dated inside the same quarter. The old view ignored a credit note's date
    // entirely, so this line did not used to matter; it is the whole question
    // now.
    sales::cancel_invoice(
        &fixture.db,
        &code("VR-DROP"),
        "CN-VR",
        "issued in error",
        on("2026-02-20"),
        &Metadata::default(),
    )
    .await
    .expect("credits");
    fixture.project().await;

    let after = read(&fixture).await;
    assert_eq!(
        after.tax,
        riyals(150),
        "the credit lands in the same period, so it nets the supply out"
    );
    // Both documents are still counted. A return that showed one invoice would
    // be hiding that a supply happened and was credited, which is exactly what
    // an auditor is looking for.
    assert_eq!(after.bands[0].invoices, 2);
    assert_eq!(after.bands[0].credit_notes, 1);

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

// ---------------------------------------------------------------------------
// Gapless statutory numbering
//
// Saudi law requires a tax invoice to carry "a sequential number which uniquely
// identifies the invoice" (VAT Implementing Regulations, Article 53). Not
// unique. Not mostly ordered. **Gapless** — an auditor counts them, and a
// missing 4,108 is a question the business has to answer.
//
// Every test below is about a way a number could go missing or repeat.
// ---------------------------------------------------------------------------

/// Numbers come out one after another, from one.
#[tokio::test]
async fn invoices_are_numbered_in_an_unbroken_sequence() {
    let fixture = Fixture::new().await;

    let mut numbers = Vec::new();
    for i in 1..=5 {
        let issued = issue_numbered(
            &fixture,
            &format!("KEY-{i}"),
            vec![line("Consulting", riyals(100), VatCategory::Standard)],
        )
        .await
        .expect("issues");
        numbers.push(issued.number);
    }

    assert_eq!(
        numbers,
        [
            "INV-00001",
            "INV-00002",
            "INV-00003",
            "INV-00004",
            "INV-00005"
        ],
        "the series has a hole or a repeat in it"
    );

    // And the read model agrees, which is the copy anybody actually looks at.
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let mut projected: Vec<String> = sales::invoices(&mut conn, 100, None)
        .await
        .expect("reads")
        .items
        .into_iter()
        .map(|i| i.number)
        .collect();
    drop(conn);
    projected.sort();
    assert_eq!(projected, numbers);

    fixture.cleanup().await;
}

/// **A retried request does not burn a number.**
///
/// This is the pairing `erp_eventlog::numbering` cannot enforce from inside
/// itself: reserve, decide nothing, and do *not* consume. A client whose request
/// timed out and repeated it is the normal case, not an edge one, and putting a
/// gap in a business's invoice sequence because their network blinked would be
/// this feature failing at the one thing it exists to do.
#[tokio::test]
async fn re_issuing_does_not_move_the_series() {
    let fixture = Fixture::new().await;
    let lines = vec![line("Consulting", riyals(100), VatCategory::Standard)];

    let first = issue_numbered(&fixture, "KEY-1", lines.clone())
        .await
        .expect("issues");
    assert_eq!(first.number, "INV-00001");

    // The same key, three more times.
    for _ in 0..3 {
        let again = issue_numbered(&fixture, "KEY-1", lines.clone())
            .await
            .expect("is a no-op");
        assert!(again.committed.did_nothing(), "a retry wrote something");
        assert_eq!(
            again.number, "INV-00001",
            "a retry must be told the number the invoice already has"
        );
    }

    // So the next real invoice is 2, not 5.
    let second = issue_numbered(&fixture, "KEY-2", lines)
        .await
        .expect("issues");
    assert_eq!(second.number, "INV-00002");

    let mut conn = fixture.db.acquire().await.expect("connection");
    assert_eq!(
        erp_eventlog::numbering::peek(&mut conn, sales::INVOICE_SERIES)
            .await
            .expect("reads"),
        3,
        "the counter moved for something that was not issued"
    );
    drop(conn);

    fixture.cleanup().await;
}

/// **A refused invoice does not burn a number either.**
///
/// The reason a Postgres sequence cannot do this job: `nextval` survives a
/// rollback by design. Here the reservation is an ordinary row read `FOR
/// UPDATE`, so a transaction that fails takes the number down with it.
#[tokio::test]
async fn a_refused_invoice_leaves_the_series_where_it_was() {
    let fixture = Fixture::new().await;
    let good = vec![line("Consulting", riyals(100), VatCategory::Standard)];

    issue(&fixture, "KEY-1", good.clone())
        .await
        .expect("issues");

    // Refused for three different reasons, at three different depths: before
    // the transaction opens, inside the tax calculation, and inside the ledger.
    assert!(issue(&fixture, "KEY-EMPTY", vec![]).await.is_err());
    assert!(
        issue(
            &fixture,
            "KEY-MIXED",
            vec![
                line("SAR", riyals(100), VatCategory::Standard),
                line("USD", Money::from_minor(100, usd()), VatCategory::Standard),
            ],
        )
        .await
        .is_err()
    );

    let closed = Fixture::bare().await;
    assert!(
        issue(&closed, "KEY-NO-ACCOUNTS", good.clone())
            .await
            .is_err(),
        "a tenant with no chart cannot post"
    );
    let mut conn = closed.db.acquire().await.expect("connection");
    assert_eq!(
        erp_eventlog::numbering::peek(&mut conn, sales::INVOICE_SERIES)
            .await
            .expect("reads"),
        1,
        "a refusal at the ledger burned a number"
    );
    drop(conn);
    closed.cleanup().await;

    let next = issue_numbered(&fixture, "KEY-2", good)
        .await
        .expect("issues");
    assert_eq!(next.number, "INV-00002", "a refusal burned a number");

    fixture.cleanup().await;
}

/// Concurrent issues get consecutive numbers, and never the same one twice.
///
/// Gaplessness and concurrency are the same contradiction whatever holds the
/// counter, so the reservation serializes. This is the test that the
/// serialization is real rather than assumed — without the row lock, two
/// transactions read the same `next` and both write it.
#[tokio::test]
async fn concurrent_issues_never_share_a_number() {
    let fixture = Arc::new(Fixture::new().await);

    let issues = (1..=8).map(|i| {
        let fixture = Arc::clone(&fixture);
        tokio::spawn(async move {
            issue_numbered(
                &fixture,
                &format!("KEY-{i}"),
                vec![line("Consulting", riyals(100), VatCategory::Standard)],
            )
            .await
            .map(|numbered| numbered.number)
        })
    });

    let mut numbers: Vec<String> = Vec::new();
    for issue in issues {
        numbers.push(issue.await.expect("the task finishes").expect("issues"));
    }
    numbers.sort();

    assert_eq!(
        numbers,
        (1..=8).map(|i| format!("INV-{i:05}")).collect::<Vec<_>>(),
        "eight concurrent issues did not produce one to eight exactly once"
    );

    Arc::try_unwrap(fixture)
        .unwrap_or_else(|_| unreachable!("every task has finished"))
        .cleanup()
        .await;
}

/// Credit notes have their own series, and ZATCA wants it that way.
#[tokio::test]
async fn credit_notes_are_numbered_apart_from_invoices() {
    let fixture = Fixture::new().await;
    let lines = vec![line("Consulting", riyals(100), VatCategory::Standard)];

    let first = issue_numbered(&fixture, "KEY-1", lines.clone())
        .await
        .expect("issues");
    let second = issue_numbered(&fixture, "KEY-2", lines)
        .await
        .expect("issues");
    assert_eq!(
        (first.number.as_str(), second.number.as_str()),
        ("INV-00001", "INV-00002")
    );

    let credited = credit_numbered(&fixture, "KEY-1", "CANCEL-1")
        .await
        .expect("credits");
    assert_eq!(
        credited.number, "CN-00001",
        "a credit note takes the next credit-note number, not the next invoice number"
    );

    // Repeating the cancellation is a no-op and reports the same credit note.
    let again = credit_numbered(&fixture, "KEY-1", "CANCEL-1")
        .await
        .expect("is a no-op");
    assert!(again.committed.did_nothing());
    assert_eq!(again.number, "CN-00001");

    let next = credit_numbered(&fixture, "KEY-2", "CANCEL-2")
        .await
        .expect("credits");
    assert_eq!(
        next.number, "CN-00002",
        "the credit note series has a hole in it"
    );

    // And issuing carries on from where it was: the two series are independent.
    let third = issue_numbered(
        &fixture,
        "KEY-3",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    assert_eq!(third.number, "INV-00003");

    fixture.cleanup().await;
}

/// **A rebuild reproduces the numbers rather than re-allocating them.**
///
/// The number is in the event, not derived on read (architecture L5). If it were
/// derived, replaying a tenant's log would renumber every document they have
/// ever issued — including the ones customers hold copies of.
#[tokio::test]
async fn a_replay_reproduces_the_numbers_it_issued_under() {
    let fixture = Fixture::new().await;
    let lines = vec![line("Consulting", riyals(100), VatCategory::Standard)];

    for i in 1..=3 {
        issue(&fixture, &format!("KEY-{i}"), lines.clone())
            .await
            .expect("issues");
    }
    credit(&fixture, "KEY-2", "CANCEL-2")
        .await
        .expect("credits");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let before: Vec<(String, String)> = sales::invoices(&mut conn, 100, None)
        .await
        .expect("reads")
        .items
        .into_iter()
        .map(|i| (i.id, i.number))
        .collect();
    drop(conn);
    assert_eq!(before.len(), 3);

    let pool = fixture.tenant_pool().await;
    let owned = sales::projections();
    let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Sales>(&pool, &refs, sales::upcasters(), 100)
        .await
        .expect("replays");

    assert!(
        report.is_reproducible(),
        "a rebuild does not reproduce the live tables: {:?}",
        report.differences()
    );

    // The counter is *not* consulted by a replay, so it has not moved either.
    let mut conn = fixture.db.acquire().await.expect("connection");
    assert_eq!(
        erp_eventlog::numbering::peek(&mut conn, sales::INVOICE_SERIES)
            .await
            .expect("reads"),
        4,
        "a replay moved the counter"
    );
    drop(conn);

    fixture.cleanup().await;
}

/// A business arriving from another system starts where they left off.
#[tokio::test]
async fn a_series_can_start_somewhere_other_than_one() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    erp_eventlog::numbering::start_at(&mut conn, sales::INVOICE_SERIES, 4108)
        .await
        .expect("sets");
    drop(conn);

    let issued = issue_numbered(
        &fixture,
        "KEY-1",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    assert_eq!(issued.number, "INV-04108");

    // And it refuses to go backwards, which would reissue numbers that are
    // already printed on documents somebody holds.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let settled = erp_eventlog::numbering::start_at(&mut conn, sales::INVOICE_SERIES, 7)
        .await
        .expect("sets");
    drop(conn);
    assert_eq!(settled, 4109, "a series was allowed to move backwards");

    fixture.cleanup().await;
}

/// **A credit note in a later period does not reach back into a filed return.**
///
/// The bug this replaces: `taxable_supply` excluded cancelled invoices outright,
/// so crediting in April changed what a re-run of the January–March return said.
/// The Q1 return had already been filed and the tax already paid — and nothing
/// anywhere recorded why the number moved.
///
/// Each document is now reported on its own tax point. Q1 keeps the supply; the
/// credit is an adjustment in Q2, which is where ZATCA wants it and where
/// anybody reconciling the books to a filed return will look for it.
#[tokio::test]
async fn a_credit_note_is_declared_in_its_own_period_not_the_invoices() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "VR-Q1",
        "2026-02-10",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let quarter = async |fixture: &Fixture, from: &str, until: &str| {
        let mut conn = fixture.db.acquire().await.expect("connection");
        let filed = sales::vat_return(&mut conn, sar(), on(from), on(until))
            .await
            .expect("reads");
        drop(conn);
        filed
    };

    // Q1 is filed: 150 riyals of output tax, and the money has gone to ZATCA.
    let q1_as_filed = quarter(&fixture, "2026-01-01", "2026-04-01").await;
    assert_eq!(q1_as_filed.tax, riyals(150));

    // In April the invoice is credited.
    sales::cancel_invoice(
        &fixture.db,
        &code("VR-Q1"),
        "CANCEL-Q1",
        "supply never happened",
        on("2026-04-20"),
        &Metadata::default(),
    )
    .await
    .expect("credits");
    fixture.project().await;

    let q1_again = quarter(&fixture, "2026-01-01", "2026-04-01").await;
    assert_eq!(
        q1_again.tax, q1_as_filed.tax,
        "re-running a filed return gave a different answer — the credit note \
         reached back into a period that was already declared and paid"
    );
    assert_eq!(q1_again.bands[0].invoices, 1);
    assert_eq!(
        q1_again.bands[0].credit_notes, 0,
        "the credit note has an April tax point and does not belong to Q1"
    );

    // And Q2 carries the adjustment, which is the whole point of not deleting it.
    let q2 = quarter(&fixture, "2026-04-01", "2026-07-01").await;
    assert_eq!(
        q2.tax,
        riyals(-150),
        "the credit is declared in the period it happened"
    );
    assert_eq!(q2.net, riyals(-1_000));
    assert_eq!(q2.bands[0].invoices, 0);
    assert_eq!(q2.bands[0].credit_notes, 1);

    // Over both quarters together the two cancel, which is the arithmetic that
    // makes this a restatement rather than a loss.
    let half = quarter(&fixture, "2026-01-01", "2026-07-01").await;
    assert_eq!(half.tax, money(0));
    assert_eq!(half.net, money(0));

    // The ledger agrees: the VAT account is back to nothing.
    assert_eq!(fixture.balance("2100").await, money(0));

    fixture.cleanup().await;
}

/// A credit note reverses every band the invoice declared, not just one.
#[tokio::test]
async fn a_credit_note_adjusts_each_rate_the_invoice_carried() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "VR-MIXED",
        "2026-02-10",
        vec![
            line("Consulting", riyals(1_000), VatCategory::Standard),
            line("Export", riyals(400), VatCategory::Zero),
            line("Rent", riyals(200), VatCategory::Exempt),
        ],
    )
    .await
    .expect("issues");
    sales::cancel_invoice(
        &fixture.db,
        &code("VR-MIXED"),
        "CANCEL-MIXED",
        "cancelled",
        on("2026-05-02"),
        &Metadata::default(),
    )
    .await
    .expect("credits");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let q2 = sales::vat_return(&mut conn, sar(), on("2026-04-01"), on("2026-07-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(
        q2.bands.len(),
        3,
        "one adjustment per band, not one per invoice"
    );
    assert_eq!(q2.net, riyals(-1_600), "every band's net is reversed");
    assert_eq!(
        q2.tax,
        riyals(-150),
        "and only the standard-rated one carried tax"
    );
    for band in &q2.bands {
        assert_eq!(band.invoices, 0);
        assert_eq!(band.credit_notes, 1);
    }

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Closed periods, from the other side of the seam
//
// Sales never mentions a fiscal period. It inherits the refusal because an
// invoice and its journal entry commit together, so every sales write arrives at
// `ledger::post_entry_in` — which is where the one check lives. These are the
// tests that the seam actually carries it, rather than that it was supposed to.
// ---------------------------------------------------------------------------

/// **An invoice with a back-dated tax point cannot reopen a filed quarter.**
#[tokio::test]
async fn an_invoice_cannot_be_dated_into_a_closed_period() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-04-01")), Some("the-accountant"))
        .await
        .expect("closes the first quarter");
    drop(conn);

    let refused = issue_on(
        &fixture,
        "KEY-BACKDATED",
        "2026-02-14",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(SalesError::Ledger(ledger::LedgerError::PeriodClosed { .. }))
        ),
        "an invoice was dated into a quarter whose return has been filed"
    );

    // And nothing was left behind: not the invoice, and not a number out of the
    // series. The whole transaction went, which is the same guarantee
    // `a_failed_posting_leaves_no_invoice_behind` makes about the ledger.
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    assert!(
        sales::invoices(&mut conn, 100, None)
            .await
            .expect("reads")
            .items
            .is_empty(),
        "a refused invoice left a row behind"
    );
    assert_eq!(
        erp_eventlog::numbering::peek(&mut conn, sales::INVOICE_SERIES)
            .await
            .expect("reads"),
        1,
        "a refused invoice burned a number"
    );
    drop(conn);

    // The open quarter still works, and takes the first number.
    let issued = issue_on(
        &fixture,
        "KEY-OK",
        "2026-04-14",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues into the open period");
    assert!(issued.at.is_some());

    fixture.cleanup().await;
}

/// **A credit note cannot be dated back into a filed quarter either.**
///
/// This is the case the VAT return's period rule depends on. An adjustment
/// belongs in the period it happened; letting somebody date one into a quarter
/// that has been declared would put the return back exactly where it was before
/// `vat_entry` — able to restate itself after filing.
#[tokio::test]
async fn a_credit_note_cannot_be_dated_into_a_closed_period() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "KEY-Q1",
        "2026-02-10",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-04-01")), Some("the-accountant"))
        .await
        .expect("closes the first quarter");
    drop(conn);

    let refused = sales::cancel_invoice(
        &fixture.db,
        &code("KEY-Q1"),
        "CANCEL-BACKDATED",
        "cancelled",
        on("2026-03-01"),
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(SalesError::Ledger(ledger::LedgerError::PeriodClosed { .. }))
        ),
        "a credit note was dated into a quarter that had already been declared"
    );

    // The filed quarter still says what it said.
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let q1 = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    drop(conn);
    assert_eq!(q1.tax, riyals(150), "the filed return moved");

    // Dated into the open quarter, the credit goes through — and lands there.
    sales::cancel_invoice(
        &fixture.db,
        &code("KEY-Q1"),
        "CANCEL-Q2",
        "cancelled",
        on("2026-04-20"),
        &Metadata::default(),
    )
    .await
    .expect("credits into the open period");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let q1_again = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    let q2 = sales::vat_return(&mut conn, sar(), on("2026-04-01"), on("2026-07-01"))
        .await
        .expect("reads");
    drop(conn);
    assert_eq!(q1_again.tax, riyals(150), "and it still says it");
    assert_eq!(
        q2.tax,
        riyals(-150),
        "the adjustment is in the open quarter"
    );

    fixture.cleanup().await;
}

/// A payment is dated too, and a receipt back-dated into a closed period moves
/// cash that has already been reconciled.
#[tokio::test]
async fn a_payment_cannot_be_dated_into_a_closed_period() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "KEY-1",
        "2026-02-10",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let mut conn = fixture.db.acquire().await.expect("connection");
    ledger::period::close(&mut conn, Some(on("2026-04-01")), Some("the-accountant"))
        .await
        .expect("closes");
    drop(conn);

    let refused = record_payment(
        &fixture.db,
        &code("KEY-1"),
        &Receipt {
            reference: "wire-1".to_owned(),
            amount: riyals(100),
            received_on: on("2026-03-05"),
            into: code("1010"),
        },
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(SalesError::Ledger(ledger::LedgerError::PeriodClosed { .. }))
        ),
        "cash moved in a period that had already been reconciled"
    );

    pay(&fixture, "KEY-1", "wire-2", riyals(100))
        .await
        .expect_err("`when()` is 2023, which is also closed");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The rate is the tenant's, not the build's
// ---------------------------------------------------------------------------

/// **A business outside Saudi Arabia can issue a correct invoice.**
///
/// The rate used to be `VatCategory::rate_now()` returning 1500 from the
/// accounting kernel, so a tenant in the UAE — 5% — could not. It is
/// configuration now, resolved in the command's own transaction.
#[tokio::test]
async fn an_invoice_carries_the_rate_the_tenant_configured() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        ledger::Rates::KEY,
        &ledger::Rates { standard: 500 },
        Some("the-accountant"),
    )
    .await
    .expect("sets");
    drop(conn);

    issue(
        &fixture,
        "KEY-1",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let invoice = sales::invoice(&mut conn, "KEY-1")
        .await
        .expect("reads")
        .expect("is there");
    drop(conn);

    assert_eq!(invoice.summary.tax, riyals(50), "5% of 1,000, not 15%");
    assert_eq!(invoice.summary.gross, riyals(1_050));
    assert_eq!(
        invoice.lines[0].basis_points, 500,
        "and the line carries the rate it was issued under"
    );

    // The ledger agrees, which is what makes the invoice and the books one
    // document rather than two numbers that happen to match.
    assert_eq!(fixture.balance("2100").await, riyals(-50));

    fixture.cleanup().await;
}

/// **Changing the rate does not restate what was already issued.**
///
/// The rate goes into the event as a value (L5). If it were read back at
/// projection time, raising the rate would silently change every invoice a
/// business has ever filed a return against.
#[tokio::test]
async fn changing_the_rate_leaves_earlier_invoices_alone() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "KEY-15",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues at the shipped 15%");

    let mut conn = fixture.db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        ledger::Rates::KEY,
        &ledger::Rates { standard: 500 },
        Some("the-accountant"),
    )
    .await
    .expect("sets");
    drop(conn);

    issue(
        &fixture,
        "KEY-5",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues at 5%");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let old = sales::invoice(&mut conn, "KEY-15")
        .await
        .expect("reads")
        .expect("is there");
    let new = sales::invoice(&mut conn, "KEY-5")
        .await
        .expect("reads")
        .expect("is there");
    drop(conn);

    assert_eq!(
        old.summary.tax,
        riyals(150),
        "the earlier invoice was restated"
    );
    assert_eq!(new.summary.tax, riyals(50));

    // And a rebuild reproduces both, because both rates are in the log.
    let pool = fixture.tenant_pool().await;
    let owned = sales::projections();
    let refs: Vec<&dyn Projection<Group = Sales>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<Sales>(&pool, &refs, sales::upcasters(), 100)
        .await
        .expect("replays");
    pool.close().await;
    assert!(
        report.is_reproducible(),
        "a rebuild renumbered the rates: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}
