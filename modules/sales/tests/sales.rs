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
        allowances: Vec::new(),
        description: description.to_owned(),
        net,
        category,
        product: None,
        quantity: None,
        serials: Vec::new(),
        lot: None,
    }
}

fn draft(lines: Vec<DraftLine>) -> Draft {
    Draft {
        prepayment: false,
        prepaid: None,
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
        fixture.configure_exemption_reasons().await;
        fixture
    }

    /// A tenant with accounts but **nothing configured at all**, for the tests
    /// about what a command records when there is no configuration to resolve
    /// against. Only standard-rated lines can be issued here, which is the
    /// point: a line carrying no tax needs an article, and an article is
    /// configuration.
    async fn unconfigured() -> Self {
        let fixture = Self::bare().await;
        for (account, kind) in [
            ("1010", AccountKind::Asset),
            ("1100", AccountKind::Asset),
            ("2100", AccountKind::Liability),
            ("4000", AccountKind::Revenue),
        ] {
            fixture.open(account, kind, sar()).await;
        }
        fixture
    }

    /// **Why a fixture has to say this at all.** A line that carries no tax
    /// must name the article it is exempt or zero-rated under, and this build
    /// refuses to issue one that cannot. The codes below are the ones this
    /// file's own test data implies: it invoices "Export" at zero rate and
    /// "Residential rent" as exempt.
    async fn configure_exemption_reasons(&self) {
        let mut conn = self.db.acquire().await.expect("a connection");
        erp_eventlog::configuration::set(
            &mut conn,
            ledger::Rates::KEY,
            &ledger::Rates {
                standard: 1_500,
                zero_reason: Some("VATEX-SA-32".to_owned()),
                exempt_reason: Some("VATEX-SA-30".to_owned()),
            },
            Some("the-accountant"),
            None,
        )
        .await
        .expect("rates configure");
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
        // **`hr` too, because issuing a credit note is a claim.** Resolving a
        // caller to an employee reads `hr`'s read model.
        hr::install(&mut conn).await.expect("hr schema");
        ensure_group_schema::<hr::Hr>(&mut conn)
            .await
            .expect("hr checkpoint");
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
    issue_invoice(
        &fixture.db,
        &code(id),
        &draft(lines),
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
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
        sales::Authority::System,
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
        None,
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
        None,
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

/// **A line that carries no tax must name the article it is untaxed under.**
///
/// Refused at issue rather than defaulted downstream. Until 2026-09-09 an
/// exempt line was rendered to ZATCA as `VATEX-SA-29`, financial services,
/// whatever the business actually did — right for a bank and a false statement
/// to a tax authority for a landlord.
#[tokio::test]
async fn a_line_that_carries_no_tax_must_say_why() {
    let fixture = Fixture::unconfigured().await;

    for category in [VatCategory::Zero, VatCategory::Exempt] {
        let refused = issue(
            &fixture,
            "INV-NOREASON",
            vec![line("Rent", riyals(100), category)],
        )
        .await;
        assert!(
            matches!(
                refused,
                Err(CommandError::Execute(ExecuteError::Rejected(
                    SalesError::NoExemptionReason { category: refused_category }
                ))) if refused_category == category
            ),
            "a {category:?} line with no configured article must be refused, got {refused:?}"
        );
    }

    // And a standard-rated line is unaffected: it is taxed, so it has nothing
    // to explain.
    issue(
        &fixture,
        "INV-STANDARD",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("a taxed line needs no article");

    fixture.cleanup().await;
}

/// A command records which generation of configuration it decided against.
#[tokio::test]
async fn a_command_stamps_the_configuration_it_resolved_against() {
    let fixture = Fixture::unconfigured().await;

    // **Standard-rated on purpose.** A zero-rated line needs the article it is
    // zero-rated under, and that is configuration — which this test exists to
    // observe the *absence* of.
    let committed = issue(
        &fixture,
        "INV-CFG-2",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
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
        None,
    )
    .await
    .expect("configures");
    drop(conn);

    let committed = issue(
        &fixture,
        "INV-CFG-3",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
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
        sales::Authority::System,
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

/// An invoice the business is **still holding money against** cannot be
/// cancelled. Not "has ever been paid": the money has to go back first, and
/// `refund_invoice` is how — see the test below.
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

/// **The final invoice after a deposit charges and declares only the rest.**
///
/// A deposit is taxed when it is received, on its prepayment invoice. When the
/// service is delivered the final invoice shows the whole supply, names the
/// prepayment invoice, and deducts what it declared band by band — so the
/// customer owes the remainder, the ledger posts the remainder, and the return
/// declares the deposit's tax once. A deposit that does not fit the supply is
/// refused rather than declared as a negative.
#[expect(
    clippy::too_many_lines,
    reason = "a deposit, its final invoice and the two refusals in one story; splitting it \
              would mean re-issuing the deposit in every half"
)]
#[tokio::test]
async fn the_final_invoice_after_a_deposit_charges_and_declares_only_the_rest() {
    let fixture = Fixture::new().await;

    // The deposit: a fifth of the service, billed when it was paid.
    let deposit = Draft {
        prepayment: true,
        issued_on: on("2026-01-05"),
        ..draft(vec![line("Deposit", riyals(200), VatCategory::Standard)])
    };
    let numbered = issue_invoice(
        &fixture.db,
        &code("dep-1"),
        &deposit,
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("the deposit is billed");
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let bands = sales::bands_of(&mut conn, "dep-1").await.expect("reads");
    assert_eq!(bands.len(), 1);
    assert_eq!(bands[0].net, riyals(200));
    assert_eq!(bands[0].tax, riyals(30));
    drop(conn);

    let prepaid = sales::Prepaid {
        invoice: code("dep-1"),
        number: numbered.number.clone(),
        issued_on: on("2026-01-05"),
        bands,
    };

    // The service, delivered a month later, and billed with the deposit off.
    let final_invoice = Draft {
        issued_on: on("2026-02-10"),
        prepaid: Some(prepaid.clone()),
        ..draft(vec![line(
            "Colour and cut",
            riyals(1_000),
            VatCategory::Standard,
        )])
    };
    let numbered = issue_invoice(
        &fixture.db,
        &code("bk-1"),
        &final_invoice,
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("the final invoice is issued");
    let Some(sales::InvoiceEvent::Issued {
        totals,
        lines,
        prepaid: deducted,
        ..
    }) = numbered.committed.events.first()
    else {
        panic!("an invoice was issued");
    };
    assert_eq!(
        lines[0].net,
        riyals(1_000),
        "the lines are the whole supply"
    );
    assert_eq!(totals.net, riyals(800), "the totals are what is left");
    assert_eq!(totals.tax, riyals(120));
    assert_eq!(totals.gross, riyals(920));
    assert_eq!(
        deducted.as_ref().map(|p| p.number.as_str()),
        Some(prepaid.number.as_str())
    );

    fixture.project().await;
    // The customer owes the rest, and the read model says which document
    // took the deposit.
    let read = fixture.invoice("bk-1").await.expect("read back");
    assert_eq!(read.summary.gross, riyals(920));
    assert_eq!(
        read.summary.prepaid_number.as_deref(),
        Some(prepaid.number.as_str())
    );
    // The books: the receivable is the deposit's 230 plus the remainder's
    // 920, and revenue is the whole 1,000 recognised once.
    assert_eq!(fixture.balance("1100").await, riyals(1_150));
    assert_eq!(fixture.balance("4000").await, riyals(-1_000));
    // The return declares the deposit's tax in January and the rest in
    // February, and never the deposit's twice.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let january = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-02-01"))
        .await
        .expect("reads");
    assert_eq!(january.tax, riyals(30));
    let february = sales::vat_return(&mut conn, sar(), on("2026-02-01"), on("2026-03-01"))
        .await
        .expect("reads");
    assert_eq!(february.tax, riyals(120));
    drop(conn);

    // **A deposit that does not fit is refused, not declared negative.**
    let too_much = Draft {
        prepaid: Some(sales::Prepaid {
            bands: vec![sales::TaxBand {
                net: riyals(500),
                tax: riyals(75),
                ..prepaid.bands[0]
            }],
            ..prepaid.clone()
        }),
        ..draft(vec![line("Trim", riyals(100), VatCategory::Standard)])
    };
    let refused = issue_invoice(
        &fixture.db,
        &code("bk-2"),
        &too_much,
        &Metadata::default(),
        sales::Authority::System,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Prepaid(sales::PrepaidError::MoreThanTheSupply { .. })
            )))
        ),
        "{refused:?}"
    );
    let other_band = Draft {
        prepaid: Some(prepaid.clone()),
        ..draft(vec![line("Export", riyals(1_000), VatCategory::Zero)])
    };
    let refused = issue_invoice(
        &fixture.db,
        &code("bk-3"),
        &other_band,
        &Metadata::default(),
        sales::Authority::System,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Prepaid(sales::PrepaidError::NoSuchBand { .. })
            )))
        ),
        "{refused:?}"
    );
}

/// **A partial refund of a single-band invoice gets a credit note for the
/// part**; one of a multi-band invoice still gets none, because how the refund
/// divides across bands is not this system's to guess.
#[tokio::test]
async fn a_partial_refund_credits_the_part_when_the_invoice_has_one_band() {
    let fixture = Fixture::new().await;

    issue(
        &fixture,
        "INV-1",
        vec![line("Cut", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-1", "card", riyals(115))
        .await
        .expect("paid in full");
    refund(&fixture, "INV-1", "back-1", money(5_750))
        .await
        .expect("half back");
    // And the same refund again, which is a retry.
    refund(&fixture, "INV-1", "back-1", money(5_750))
        .await
        .expect("a retry is quiet");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let notes = sales::credit_notes(&mut conn, "INV-1")
        .await
        .expect("reads");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].reference, "back-1");
    assert_eq!(notes[0].net, riyals(50));
    assert_eq!(notes[0].tax, money(750));
    assert_eq!(notes[0].gross, money(5_750));
    drop(conn);

    // Two bands: the refund could be either's, so no document.
    issue(
        &fixture,
        "INV-2",
        vec![
            line("Cut", riyals(100), VatCategory::Standard),
            line("Export", riyals(100), VatCategory::Zero),
        ],
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-2", "card", riyals(215))
        .await
        .expect("paid in full");
    refund(&fixture, "INV-2", "back-2", riyals(50))
        .await
        .expect("some back");
    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let notes = sales::credit_notes(&mut conn, "INV-2")
        .await
        .expect("reads");
    assert!(notes.is_empty(), "a band was guessed: {notes:?}");

    // A gross no net lands on at this rate — 10.00 at 15% — gets none either,
    // rather than a document that is a halala off.
    refund(&fixture, "INV-1", "back-3", riyals(10))
        .await
        .expect("some back");
    fixture.project().await;
    let notes = sales::credit_notes(&mut conn, "INV-1")
        .await
        .expect("reads");
    assert_eq!(notes.len(), 1, "{notes:?}");
}

fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

/// Issues an invoice on a given date, so a return has periods to separate.
async fn issue_on(fixture: &Fixture, id: &str, day: &str, lines: Vec<DraftLine>) -> Outcome {
    let issued = issue_invoice(
        &fixture.db,
        &code(id),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: Customer::new("Rawabi Trading"),
            issued_on: on(day),
            due_on: None,
            currency: sar(),
            lines,
            discounts: Vec::new(),
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
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

/// **The whole point of crediting on refund**, in the one place a tax authority
/// looks. A refund that moved money and issued no document left a VAT return
/// declaring output tax on a supply the business had already unwound — which is
/// tax paid on a sale that did not happen, quarter after quarter, with nothing
/// in the system to say so.
#[tokio::test]
async fn a_refund_takes_its_supply_out_of_the_vat_return() {
    let fixture = Fixture::new().await;

    issue_on(
        &fixture,
        "VR-REFUND",
        "2026-02-01",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    pay(&fixture, "VR-REFUND", "wire-1", riyals(1_150))
        .await
        .expect("records");
    fixture.project().await;

    let read = async |fixture: &Fixture| {
        let mut conn = fixture.db.acquire().await.expect("connection");
        let filed = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
            .await
            .expect("reads");
        drop(conn);
        filed
    };
    assert_eq!(read(&fixture).await.tax, riyals(150), "15% of 1,000");

    sales::refund_invoice(
        &fixture.db,
        &code("VR-REFUND"),
        &Receipt {
            reference: "refund-1".to_owned(),
            amount: riyals(1_150),
            received_on: on("2026-02-20"),
            into: code("1010"),
        },
        "the engagement was cancelled",
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("refunds");
    fixture.project().await;

    let after = read(&fixture).await;
    assert_eq!(
        after.tax,
        riyals(0),
        "output tax on a supply that was unwound"
    );
    assert_eq!(after.net, riyals(0));

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
        sales::Authority::System,
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
        sales::Authority::System,
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
        sales::Authority::System,
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
        sales::Authority::System,
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
        sales::Authority::System,
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
        &ledger::Rates {
            standard: 500,
            zero_reason: None,
            exempt_reason: None,
        },
        Some("the-accountant"),
        None,
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
        &ledger::Rates {
            standard: 500,
            zero_reason: None,
            exempt_reason: None,
        },
        Some("the-accountant"),
        None,
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

// ---------------------------------------------------------------------------
// Receivables
// ---------------------------------------------------------------------------
//
// The question an accounts-receivable clerk asks every morning, and one this
// system could not answer until there was a report for it: invoices could be
// listed and paid, but not summed by the person who owed them.
//
// What these are really about is the bucket boundaries. Ageing is arithmetic on
// dates, off-by-one is the standard defect, and the direction it fails matters —
// a debt that shows one bucket too young is a debt nobody escalates.

/// Issues an invoice to a named customer, due on a given day.
async fn owe(
    fixture: &Fixture,
    id: &str,
    customer: &str,
    issued: &str,
    due: Option<&str>,
    amount: Money,
) -> Outcome {
    issue_invoice(
        &fixture.db,
        &code(id),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: Customer::new(customer),
            issued_on: on(issued),
            due_on: due.map(on),
            currency: amount.currency(),
            lines: vec![line("Consulting", amount, VatCategory::Zero)],
            discounts: Vec::new(),
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .map(|numbered| numbered.committed)
}

/// The report, as at a day.
async fn aged(fixture: &Fixture, as_of: &str) -> Vec<sales::AgedCustomer> {
    let mut conn = fixture.db.acquire().await.expect("connection");
    sales::receivables(&mut conn, on(as_of), 100, None)
        .await
        .expect("receivables")
        .items
}

/// **Each bucket takes the days it says it does.**
///
/// One invoice per boundary, aged from a fixed date. An off-by-one at any edge
/// moves money into an adjacent column, and the one that matters is 90/91: over
/// ninety days is the column a business writes off against.
#[tokio::test]
async fn debt_lands_in_the_bucket_its_age_says() {
    let fixture = Fixture::new().await;

    // As at 2026-04-01. Due dates chosen to sit on each edge.
    owe(
        &fixture,
        "FUT",
        "Rawabi",
        "2026-03-01",
        Some("2026-04-10"),
        riyals(10),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D0",
        "Rawabi",
        "2026-03-01",
        Some("2026-04-01"),
        riyals(20),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D1",
        "Rawabi",
        "2026-03-01",
        Some("2026-03-31"),
        riyals(30),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D30",
        "Rawabi",
        "2026-02-01",
        Some("2026-03-02"),
        riyals(40),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D31",
        "Rawabi",
        "2026-02-01",
        Some("2026-03-01"),
        riyals(50),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D90",
        "Rawabi",
        "2026-01-01",
        Some("2026-01-01"),
        riyals(60),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "D91",
        "Rawabi",
        "2025-12-01",
        Some("2025-12-31"),
        riyals(70),
    )
    .await
    .expect("issues");
    fixture.project().await;

    let rows = aged(&fixture, "2026-04-01").await;
    assert_eq!(rows.len(), 1, "one customer, one currency");
    let row = &rows[0];

    assert_eq!(row.not_yet_due, riyals(30), "due later, plus due today");
    assert_eq!(
        row.days_1_30,
        riyals(70),
        "one day late, and thirty days late"
    );
    assert_eq!(row.days_31_60, riyals(50), "thirty-one days late");
    assert_eq!(
        row.days_61_90,
        riyals(60),
        "ninety days late is not yet over ninety"
    );
    assert_eq!(row.over_90, riyals(70), "ninety-one days late");
    assert_eq!(row.total, riyals(280), "every bucket together");
    assert_eq!(row.invoices, 7);
    assert_eq!(row.oldest_due, on("2025-12-31"));
}

/// **An invoice with no terms was due when it was issued.**
///
/// `due_on` is optional, and treating an absent one as "not yet due" for ever is
/// how a ledger fills with debts nobody chases — the invoice never ages, so it
/// never reaches a column anyone escalates.
#[tokio::test]
async fn an_invoice_with_no_due_date_ages_from_when_it_was_issued() {
    let fixture = Fixture::new().await;

    owe(
        &fixture,
        "NOTERMS",
        "Rawabi",
        "2025-11-01",
        None,
        riyals(100),
    )
    .await
    .expect("issues");
    fixture.project().await;

    let rows = aged(&fixture, "2026-04-01").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].over_90,
        riyals(100),
        "five months after issue, with no terms to say otherwise"
    );
    assert_eq!(
        rows[0].not_yet_due,
        Money::from_minor(0, sar()),
        "an absent due date must not park the debt in `not_yet_due` for ever"
    );
}

/// **A credited invoice owes nothing.**
///
/// `invoice_status` already excludes it, and this is the assertion that keeps it
/// excluded: the view's own comment says what goes wrong otherwise — somebody
/// chases a customer for money that was credited back to them.
#[tokio::test]
async fn a_cancelled_invoice_is_not_owed() {
    let fixture = Fixture::new().await;

    owe(
        &fixture,
        "GONE",
        "Rawabi",
        "2026-01-01",
        Some("2026-01-15"),
        riyals(500),
    )
    .await
    .expect("issues");
    credit(&fixture, "GONE", "CN-GONE").await.expect("credits");
    fixture.project().await;

    assert!(
        aged(&fixture, "2026-04-01").await.is_empty(),
        "a credited invoice must not appear in receivables"
    );
}

/// **A tenant cannot yet owe in two currencies, and the ledger is what says so.**
///
/// The report groups by `(customer, currency)` because `Money` has no `Add`
/// (D10) — a total mixing SAR and USD would be true in neither. That grouping is
/// currently free, because an invoice in a currency other than the ledger's is
/// refused before it is ever issued: multi-currency entries with FX are unbuilt,
/// and are on the list as such.
///
/// This is here rather than a test of the two-row case because the two-row case
/// is unreachable, and a test that reached it would have to write projection
/// rows the domain cannot produce — which proves the SQL and pretends about the
/// system. When multi-currency lands, this test is the one that fails, and the
/// grouping it documents is already right.
#[tokio::test]
async fn owing_in_a_second_currency_is_refused_by_the_ledger() {
    let fixture = Fixture::new().await;

    owe(
        &fixture,
        "SAR1",
        "Rawabi",
        "2026-03-01",
        Some("2026-03-15"),
        riyals(100),
    )
    .await
    .expect("the ledger's own currency issues");

    let refused = owe(
        &fixture,
        "USD1",
        "Rawabi",
        "2026-03-01",
        Some("2026-03-15"),
        Money::from_minor(5_000, usd()),
    )
    .await;

    assert!(
        refused.is_err(),
        "an invoice in a currency the ledger does not keep must be refused, got {refused:?}"
    );

    fixture.project().await;
    let rows = aged(&fixture, "2026-04-01").await;
    assert_eq!(rows.len(), 1, "one currency, so one row");
    assert_eq!(rows[0].currency, sar());
    assert_eq!(rows[0].total, riyals(100));
}

/// **A settled invoice leaves the report.**
#[tokio::test]
async fn a_paid_invoice_is_no_longer_owed() {
    let fixture = Fixture::new().await;

    owe(
        &fixture,
        "PAID",
        "Rawabi",
        "2026-01-01",
        Some("2026-01-15"),
        riyals(200),
    )
    .await
    .expect("issues");
    pay(&fixture, "PAID", "BANK-1", riyals(200))
        .await
        .expect("pays");
    fixture.project().await;

    assert!(
        aged(&fixture, "2026-04-01").await.is_empty(),
        "nothing is owed once it is paid"
    );
}

/// **Biggest debtor first**, because that is the order the list is worked in.
#[tokio::test]
async fn the_largest_debt_comes_first() {
    let fixture = Fixture::new().await;

    owe(
        &fixture,
        "SMALL",
        "Small Co",
        "2026-01-01",
        Some("2026-01-15"),
        riyals(50),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "BIG",
        "Big Co",
        "2026-01-01",
        Some("2026-01-15"),
        riyals(900),
    )
    .await
    .expect("issues");
    owe(
        &fixture,
        "MID",
        "Mid Co",
        "2026-01-01",
        Some("2026-01-15"),
        riyals(300),
    )
    .await
    .expect("issues");
    fixture.project().await;

    let rows = aged(&fixture, "2026-04-01").await;
    let order: Vec<&str> = rows.iter().map(|r| r.customer.as_str()).collect();
    assert_eq!(order, vec!["Big Co", "Mid Co", "Small Co"]);
}

// ---------------------------------------------------------------------------
// The customer reference
// ---------------------------------------------------------------------------
//
// An invoice references a `crm` record **and** freezes what it printed. Both,
// never either. The reference is what makes "everything for this customer"
// answerable when they are spelled two ways; the frozen copy is what the law
// requires the document to say, and it does not move when the record does.

/// Records a customer, so an invoice has somebody real to name.
async fn customer_record(fixture: &Fixture, id: &str, name: &str) {
    crm::register_customer(
        &fixture.db,
        &code(id),
        &crm::Details {
            name: name.to_owned(),
            name_latin: None,
            kind: crm::CustomerKind::Person,
            contact: crm::Contact {
                phone: Some("+966500000000".to_owned()),
                email: None,
            },
            address: None,
            tax: None,
        },
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("registers");
}

/// Issues to a `crm` record, freezing a name that may differ from the record's.
async fn owe_customer(
    fixture: &Fixture,
    id: &str,
    reference: &str,
    printed: &str,
    issued: &str,
    amount: Money,
) -> Outcome {
    issue_invoice(
        &fixture.db,
        &code(id),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: Customer::new(printed).of(code(reference)),
            issued_on: on(issued),
            due_on: Some(on(issued)),
            currency: amount.currency(),
            lines: vec![line("Consulting", amount, VatCategory::Zero)],
            discounts: Vec::new(),
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .map(|numbered| numbered.committed)
}

/// **The reference and the copy, both.**
#[tokio::test]
async fn an_invoice_names_a_customer_and_still_freezes_what_it_printed() {
    let fixture = Fixture::new().await;
    customer_record(&fixture, "CUST-1", "Najd Consulting").await;

    // The document prints something the record does not say — a trading name,
    // a branch, a spelling. That is what a document does, and the reference is
    // what still ties it to the record.
    owe_customer(
        &fixture,
        "INV-1",
        "CUST-1",
        "Najd Consulting · Riyadh branch",
        "2026-03-01",
        riyals(1_000),
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let stored: (String, Option<String>) =
        sqlx::query_as("SELECT customer, customer_id FROM proj_sales.invoice WHERE id = 'INV-1'")
            .fetch_one(&mut *conn)
            .await
            .expect("reads");

    assert_eq!(
        stored.0, "Najd Consulting · Riyadh branch",
        "the document still says what it printed"
    );
    assert_eq!(
        stored.1.as_deref(),
        Some("CUST-1"),
        "and still points at the record"
    );

    drop(conn);
    fixture.cleanup().await;
}

/// **A reference to nobody is refused, and costs no invoice number.**
///
/// The number matters as much as the refusal. `reserve` runs before the
/// aggregate is touched, so a rejection after it must roll the whole
/// transaction back — otherwise a typo'd customer id puts a permanent gap in a
/// series ZATCA requires to be gapless.
#[tokio::test]
async fn an_invoice_to_a_customer_who_is_not_there_is_refused() {
    let fixture = Fixture::new().await;
    customer_record(&fixture, "CUST-1", "Najd Consulting").await;

    let refused = owe_customer(
        &fixture,
        "INV-1",
        "CUST-TYPO",
        "Najd Consulting",
        "2026-03-01",
        riyals(1_000),
    )
    .await
    .expect_err("there is no such customer");
    assert!(matches!(
        rejection(&refused),
        Some(SalesError::NoSuchCustomer(_))
    ));

    // The next real invoice takes the first number, so nothing was burned.
    let issued = owe_customer(
        &fixture,
        "INV-2",
        "CUST-1",
        "Najd Consulting",
        "2026-03-02",
        riyals(1_000),
    )
    .await
    .expect("issues");
    assert!(!issued.did_nothing());

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let number: String =
        sqlx::query_scalar("SELECT number FROM proj_sales.invoice WHERE id = 'INV-2'")
            .fetch_one(&mut *conn)
            .await
            .expect("reads");
    assert!(
        number.ends_with("00001"),
        "the refused invoice must not have consumed a number, got {number}"
    );

    drop(conn);
    fixture.cleanup().await;
}

/// An archived customer takes no new invoices, and keeps every old one.
#[tokio::test]
async fn an_archived_customer_takes_no_new_invoice() {
    let fixture = Fixture::new().await;
    customer_record(&fixture, "CUST-1", "Najd Consulting").await;
    owe_customer(
        &fixture,
        "INV-1",
        "CUST-1",
        "Najd",
        "2026-03-01",
        riyals(500),
    )
    .await
    .expect("issues");

    crm::archive_customer(&fixture.db, &code("CUST-1"), None, &Metadata::default())
        .await
        .expect("archives");

    let refused = owe_customer(
        &fixture,
        "INV-2",
        "CUST-1",
        "Najd",
        "2026-03-05",
        riyals(500),
    )
    .await
    .expect_err("archived customers take no new work");
    assert!(matches!(
        rejection(&refused),
        Some(SalesError::NoSuchCustomer(_))
    ));

    fixture.project().await;
    let rows = aged(&fixture, "2026-04-01").await;
    assert_eq!(rows.len(), 1, "the invoice they already have is untouched");

    fixture.cleanup().await;
}

/// **The payoff: a reference merges what a name cannot.**
///
/// Two invoices under two spellings of one buyer are two rows when all the
/// report has is the frozen name, and one row when they name the same record.
/// Both behaviours in one test, because the second is only interesting beside
/// the first.
#[tokio::test]
async fn receivables_merge_by_reference_and_split_by_name() {
    let fixture = Fixture::new().await;
    customer_record(&fixture, "CUST-1", "Najd Consulting").await;

    // Two spellings, one record.
    owe_customer(
        &fixture,
        "INV-1",
        "CUST-1",
        "Najd Consulting",
        "2026-03-01",
        riyals(1_000),
    )
    .await
    .expect("issues");
    owe_customer(
        &fixture,
        "INV-2",
        "CUST-1",
        "NAJD CONSULTING LLC",
        "2026-03-05",
        riyals(500),
    )
    .await
    .expect("issues");

    // Two spellings, no record. The old behaviour, still visible.
    owe(&fixture, "INV-3", "Rawabi", "2026-03-01", None, riyals(300))
        .await
        .expect("issues");
    owe(&fixture, "INV-4", "RAWABI", "2026-03-02", None, riyals(200))
        .await
        .expect("issues");

    fixture.project().await;
    let rows = aged(&fixture, "2026-04-01").await;

    let merged: Vec<_> = rows.iter().filter(|r| r.identified).collect();
    assert_eq!(
        merged.len(),
        1,
        "one record, one row, whatever it was called"
    );
    assert_eq!(merged[0].key, "CUST-1");
    assert_eq!(merged[0].total, riyals(1_500), "both invoices in one total");
    assert_eq!(merged[0].invoices, 2);
    assert_eq!(
        merged[0].customer, "NAJD CONSULTING LLC",
        "shown under the name the most recent invoice froze"
    );

    let unmerged: Vec<_> = rows.iter().filter(|r| !r.identified).collect();
    assert_eq!(
        unmerged.len(),
        2,
        "two spellings with no record stay two rows, and say so"
    );
    for row in unmerged {
        assert_eq!(
            row.key, row.customer,
            "an unidentified row keys by its name"
        );
    }

    fixture.cleanup().await;
}

async fn refund(fixture: &Fixture, id: &str, reference: &str, amount: Money) -> Outcome {
    sales::refund_invoice(
        &fixture.db,
        &code(id),
        &Receipt {
            reference: reference.to_owned(),
            amount,
            received_on: when(),
            into: code("1010"),
        },
        "the customer changed their mind",
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
}

/// **Money back, and the credit note with it.** The order is the point: a
/// credit note may not undo a supply while the business keeps the cash, so
/// crediting is refused until the cash has gone — and then the refund issues
/// the document itself, because ZATCA wants one and a business that has to
/// remember a second call is a business whose books drift.
#[tokio::test]
async fn refunding_what_was_paid_credits_the_invoice() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-REF-1",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-REF-1", "wire-1", riyals(115))
        .await
        .expect("records");

    // Still holding it, so still refused.
    let error = credit(&fixture, "INV-REF-1", "CN-REF-1")
        .await
        .expect_err("the money is still here");
    assert!(matches!(
        rejection(&error),
        Some(SalesError::HasPayments(_))
    ));

    refund(&fixture, "INV-REF-1", "refund-1", riyals(115))
        .await
        .expect("hands it back");

    fixture.project().await;
    let invoice = fixture.invoice("INV-REF-1").await.expect("is there");
    assert_eq!(
        invoice.summary.paid,
        riyals(0),
        "paid is net of refunds — it is what the business is holding"
    );

    // **The refund issued it.** Its own number from the tenant's gapless
    // series, not the caller's reference.
    let credit_note = invoice
        .summary
        .credit_note
        .expect("the refund issued no credit note");
    assert_ne!(credit_note, "refund-1");

    assert_eq!(
        fixture.balance("1100").await,
        money(0),
        "the receivable is square"
    );
    assert_eq!(fixture.balance("4000").await, money(0), "revenue reversed");

    // And crediting it again is refused: the document exists, and a second
    // one would be a statutory number issued against nothing.
    let error = credit(&fixture, "INV-REF-1", "CN-REF-1")
        .await
        .expect_err("a second credit note was issued");
    assert!(matches!(
        rejection(&error),
        Some(SalesError::AlreadyCancelled { .. })
    ));

    fixture.cleanup().await;
}

/// Handing back more than was taken is refused, for the reason overpaying is:
/// a business giving away money it never received has made a decision somebody
/// needs to see, and a negative balance is how that decision never gets made.
#[tokio::test]
async fn refunding_more_than_is_held_is_refused() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-REF-2",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-REF-2", "wire-1", riyals(50))
        .await
        .expect("records");

    let error = refund(&fixture, "INV-REF-2", "refund-1", riyals(80))
        .await
        .expect_err("more than was taken");
    assert!(matches!(
        rejection(&error),
        Some(SalesError::Overrefund { .. })
    ));

    // And a retry of a good one is a no-op rather than a second refund.
    for _ in 0..3 {
        refund(&fixture, "INV-REF-2", "refund-1", riyals(50))
            .await
            .expect("a retry is not an error");
    }

    fixture.project().await;
    let invoice = fixture.invoice("INV-REF-2").await.expect("is there");
    assert_eq!(invoice.summary.paid, riyals(0), "refunded three times");
    assert_eq!(
        fixture.balance("1010").await,
        money(0),
        "the bank moved once"
    );

    fixture.cleanup().await;
}

/// **The two views must agree, and this is the test that keeps them agreeing.**
///
/// `invoice_status` groups and `invoice_row` correlates, because a paged read
/// through the grouped one aggregates every invoice in the tenant to return
/// twenty rows. Two shapes of the same numbers is exactly how a rule comes to be
/// written twice and drift, so it is asserted rather than trusted.
#[tokio::test]
async fn the_two_invoice_views_answer_the_same_numbers() {
    let fixture = Fixture::new().await;

    // One untouched, one part-paid, one paid and refunded, one credited.
    for (n, lines) in (1..=4).map(|n| {
        (
            n,
            vec![line("Consulting", riyals(100), VatCategory::Standard)],
        )
    }) {
        issue(&fixture, &format!("INV-VIEW-{n}"), lines)
            .await
            .expect("issues");
    }
    pay(&fixture, "INV-VIEW-2", "wire-1", riyals(40))
        .await
        .expect("records");
    pay(&fixture, "INV-VIEW-3", "wire-1", riyals(115))
        .await
        .expect("records");
    refund(&fixture, "INV-VIEW-3", "refund-1", riyals(115))
        .await
        .expect("hands it back");
    credit(&fixture, "INV-VIEW-4", "CN-VIEW-4")
        .await
        .expect("credits");

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let disagreements: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM proj_sales.invoice_status a
           JOIN proj_sales.invoice_row b USING (id)
          WHERE a.paid        IS DISTINCT FROM b.paid
             OR a.outstanding IS DISTINCT FROM b.outstanding
             OR a.payments    IS DISTINCT FROM b.payments",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("reads");
    assert_eq!(disagreements, 0, "the two views drifted");

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proj_sales.invoice_row
          WHERE id NOT IN (SELECT id FROM proj_sales.invoice_status)",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("reads");
    assert_eq!(rows, 0, "the views disagree about which invoices exist");

    drop(conn);
    fixture.cleanup().await;
}

/// **The Phase 7a reconciliation, end to end.**
///
/// An invoice issued before anybody kept a customer list names a buyer that no
/// record matches. A foreign key would have refused every one of them; this is
/// the surface that lets somebody work through the backlog instead.
#[tokio::test]
async fn an_unmatched_buyer_can_be_matched_to_a_record_afterwards() {
    let fixture = Fixture::new().await;

    // Two invoices to the same spelling, and one to somebody else, all issued
    // with no reference — the state a tenant is in before `crm` exists.
    for (id, printed, amount) in [
        ("INV-OLD-1", "نجد للاستشارات", riyals(1_000)),
        ("INV-OLD-2", "نجد للاستشارات", riyals(500)),
        ("INV-OLD-3", "شركة أخرى", riyals(200)),
    ] {
        issue_invoice(
            &fixture.db,
            &code(id),
            &Draft {
                prepayment: false,
                prepaid: None,
                customer: Customer::new(printed),
                issued_on: on("2026-03-01"),
                due_on: Some(on("2026-03-31")),
                currency: amount.currency(),
                lines: vec![line("Consulting", amount, VatCategory::Zero)],
                discounts: Vec::new(),
                note: String::new(),
            },
            &Metadata::default(),
            sales::Authority::System,
        )
        .await
        .expect("issues");
    }
    fixture.project().await;

    // The worklist: one row per spelling, largest first.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let backlog = sales::unmatched_customers(&mut conn, 50)
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(backlog.len(), 2, "grouped by spelling, not by invoice");
    assert_eq!(backlog[0].name, "نجد للاستشارات");
    assert_eq!(backlog[0].invoices, 2, "forty invoices is one decision");
    assert_eq!(backlog[0].gross, riyals(1_500), "biggest backlog first");

    // Somebody records the customer and matches the two invoices to it.
    customer_record(&fixture, "CUST-9", "نجد للاستشارات").await;
    for id in ["INV-OLD-1", "INV-OLD-2"] {
        sales::attach_customer(
            &fixture.db,
            &code(id),
            &code("CUST-9"),
            on("2026-04-01"),
            &Metadata::default(),
        )
        .await
        .expect("matches");
    }
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");

    // **The document still says what it printed.** This is the whole reason
    // this is a reference and not an edit: restating a filed document is what
    // the frozen copy exists to prevent.
    let stored: (String, Option<String>) = sqlx::query_as(
        "SELECT customer, customer_id FROM proj_sales.invoice WHERE id = 'INV-OLD-1'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("reads");
    assert_eq!(stored.0, "نجد للاستشارات", "the printed name moved");
    assert_eq!(stored.1.as_deref(), Some("CUST-9"), "the reference is set");

    // And the worklist has shrunk to what is genuinely left.
    let remaining = sales::unmatched_customers(&mut conn, 50)
        .await
        .expect("reads");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, "شركة أخرى");
    drop(conn);

    fixture.cleanup().await;
}

/// Matching the same record twice writes nothing; matching a different one is a
/// correction and does write, because a match made to the wrong customer has to
/// be fixable.
#[tokio::test]
async fn matching_is_idempotent_and_a_wrong_match_is_correctable() {
    let fixture = Fixture::new().await;
    customer_record(&fixture, "CUST-A", "أحمد الأول").await;
    customer_record(&fixture, "CUST-B", "أحمد الثاني").await;
    issue(
        &fixture,
        "INV-MATCH-1",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let first = sales::attach_customer(
        &fixture.db,
        &code("INV-MATCH-1"),
        &code("CUST-A"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("matches");
    assert!(first.at.is_some(), "the first match wrote nothing");

    let again = sales::attach_customer(
        &fixture.db,
        &code("INV-MATCH-1"),
        &code("CUST-A"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("a retry is not an error");
    assert!(
        again.at.is_none(),
        "the same match twice wrote a second time"
    );

    // The wrong Ahmed. Correcting it is an event, so the log shows both.
    sales::attach_customer(
        &fixture.db,
        &code("INV-MATCH-1"),
        &code("CUST-B"),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("corrects");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let stored: Option<String> =
        sqlx::query_scalar("SELECT customer_id FROM proj_sales.invoice WHERE id = 'INV-MATCH-1'")
            .fetch_one(&mut *conn)
            .await
            .expect("reads");
    assert_eq!(stored.as_deref(), Some("CUST-B"), "the correction was lost");
    drop(conn);

    fixture.cleanup().await;
}

/// Matching to a customer nobody recorded is refused — against the **log**, so a
/// record created a moment ago is not refused for lagging behind its projection.
#[tokio::test]
async fn matching_to_a_customer_who_is_not_there_is_refused() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-MATCH-2",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let error = sales::attach_customer(
        &fixture.db,
        &code("INV-MATCH-2"),
        &code("CUST-NOBODY"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("no such customer");
    assert!(matches!(
        rejection(&error),
        Some(SalesError::NoSuchCustomer(_))
    ));

    // Created now, matched immediately: no projection has run, and it works.
    customer_record(&fixture, "CUST-NEW", "جديد").await;
    sales::attach_customer(
        &fixture.db,
        &code("INV-MATCH-2"),
        &code("CUST-NEW"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("the log knows, even though the projection has not run");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Partial credit notes
// ---------------------------------------------------------------------------

async fn credit_part(
    fixture: &Fixture,
    invoice: &str,
    reference: &str,
    lines: Vec<sales::CreditLine>,
) -> Result<sales::Numbered, CommandError<SalesError>> {
    sales::credit_invoice_part(
        &fixture.db,
        &code(invoice),
        &sales::CreditNote {
            reference: reference.to_owned(),
            lines,
            reason: "cancelled within 24 hours".to_owned(),
            on: when(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
}

/// Credit `net` off line `against` of the invoice. The description and the
/// treatment come from that line, which is the point.
fn credit_line(against: u16, net: Money) -> sales::CreditLine {
    sales::CreditLine {
        against,
        net,
        quantity: None,
        serials: Vec::new(),
    }
}

/// **The half-a-deposit case**, which is the one a cancellation policy needs.
/// It posts its own entry rather than reversing the invoice's, because there is
/// no such thing as reversing half a journal entry.
#[tokio::test]
async fn crediting_part_of_an_invoice_takes_back_only_that_part() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-1",
        vec![line(
            "Colour treatment",
            riyals(1_000),
            VatCategory::Standard,
        )],
    )
    .await
    .expect("issues");

    let credited = credit_part(
        &fixture,
        "INV-PC-1",
        "half",
        vec![credit_line(0, riyals(500))],
    )
    .await
    .expect("credits half");

    // Its own number, from the same gapless series a cancellation draws on.
    assert!(!credited.number.is_empty());

    fixture.project().await;

    // The invoice was 1,000 + 150 VAT. Half is credited, so half stands.
    assert_eq!(fixture.balance("1100").await, riyals(575), "half is owed");
    assert_eq!(
        fixture.balance("4000").await,
        riyals(-500),
        "half is revenue"
    );
    assert_eq!(fixture.balance("2100").await, money(-7_500), "half the VAT");

    // **The invoice is not cancelled.** `credit_note` and `cancelled_on` mean
    // "a credit note undid this invoice", and none did — an invoice can carry
    // several partial ones, so there is no single answer to put there.
    let invoice = fixture.invoice("INV-PC-1").await.expect("is there");
    assert_eq!(invoice.summary.credit_note, None);

    let notes = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        sales::credit_notes(&mut conn, "INV-PC-1")
            .await
            .expect("reads")
    };
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].net, riyals(500));
    assert_eq!(notes[0].tax, money(7_500));
    assert_eq!(notes[0].reference, "half");

    fixture.cleanup().await;
}

/// **A treatment the invoice never carried is now unrepresentable.** It used to
/// be a refusal — a caller could name `standard` against a zero-rated invoice
/// and reclaim VAT nobody charged. Naming a line instead means the rate is the
/// invoice's by construction, and the only thing left to get wrong is naming a
/// line that is not there.
#[tokio::test]
async fn a_credit_note_cannot_name_a_line_the_invoice_does_not_have() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-2",
        vec![line("Exported service", riyals(1_000), VatCategory::Zero)],
    )
    .await
    .expect("issues");

    let refused = credit_part(
        &fixture,
        "INV-PC-2",
        "sneaky",
        vec![credit_line(3, riyals(500))],
    )
    .await
    .expect_err("there is no line 3");
    assert!(
        matches!(rejection(&refused), Some(SalesError::NoSuchLine { .. })),
        "{refused:?}"
    );

    // Nothing moved.
    fixture.project().await;
    assert_eq!(fixture.balance("2100").await, money(0));
    assert_eq!(fixture.balance("1100").await, riyals(1_000));

    fixture.cleanup().await;
}

/// **Per line, and both lines are at the same rate on purpose.**
///
/// With one line standard-rated and one zero-rated the *band* cap catches
/// everything, and this passes with no per-line check at all — which is what
/// the first version of this test did, so it asserted nothing it claimed to.
/// Two lines in one band is the only shape where the line cap does the work:
/// 500 off a line of 300 sits inside a band of 500 and is still more of that
/// item than was ever sold.
#[tokio::test]
async fn credits_are_capped_per_line_even_within_one_band() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-3",
        vec![
            line("Consulting", riyals(300), VatCategory::Standard),
            line("Products", riyals(200), VatCategory::Standard),
        ],
    )
    .await
    .expect("issues");

    let refused = credit_part(
        &fixture,
        "INV-PC-3",
        "too-much-of-one-line",
        vec![credit_line(0, riyals(500))],
    )
    .await
    .expect_err("500 off a line that only ever held 300");
    assert!(
        matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
        "{refused:?}"
    );

    // The same 500, taken off the lines that actually hold it, is fine.
    credit_part(
        &fixture,
        "INV-PC-3",
        "both",
        vec![credit_line(0, riyals(300)), credit_line(1, riyals(200))],
    )
    .await
    .expect("credits each line in full");

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0));
    assert_eq!(fixture.balance("2100").await, money(0));

    fixture.cleanup().await;
}

/// Several credit notes against one invoice, and the cap is cumulative.
#[tokio::test]
async fn credits_accumulate_until_there_is_nothing_left() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-4",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    for reference in ["first", "second"] {
        credit_part(
            &fixture,
            "INV-PC-4",
            reference,
            vec![credit_line(0, riyals(400))],
        )
        .await
        .expect("credits");
    }

    let refused = credit_part(
        &fixture,
        "INV-PC-4",
        "third",
        vec![credit_line(0, riyals(400))],
    )
    .await
    .expect_err("only 200 is left");
    assert!(
        matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
        "{refused:?}"
    );

    credit_part(
        &fixture,
        "INV-PC-4",
        "third",
        vec![credit_line(0, riyals(200))],
    )
    .await
    .expect("the rest fits");

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0));
    assert_eq!(fixture.balance("4000").await, money(0));

    let notes = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        sales::credit_notes(&mut conn, "INV-PC-4")
            .await
            .expect("reads")
    };
    assert_eq!(notes.len(), 3, "three documents, three numbers");
    let mut numbers: Vec<_> = notes.iter().map(|n| n.number.clone()).collect();
    numbers.sort();
    numbers.dedup();
    assert_eq!(numbers.len(), 3, "a number was reused");

    fixture.cleanup().await;
}

/// A retry issues one document and does not move the series.
#[tokio::test]
async fn a_retried_partial_credit_issues_one_document() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-5",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    let first = credit_part(
        &fixture,
        "INV-PC-5",
        "once",
        vec![credit_line(0, riyals(300))],
    )
    .await
    .expect("credits");
    let again = credit_part(
        &fixture,
        "INV-PC-5",
        "once",
        vec![credit_line(0, riyals(300))],
    )
    .await
    .expect("is a retry");

    assert_eq!(again.number, first.number);
    fixture.project().await;
    assert_eq!(
        fixture.balance("4000").await,
        riyals(-700),
        "the retry credited a second time"
    );

    // And the next credit note takes the *next* number, not one further on.
    let next = credit_part(
        &fixture,
        "INV-PC-5",
        "twice",
        vec![credit_line(0, riyals(100))],
    )
    .await
    .expect("credits");
    assert_ne!(next.number, first.number);

    fixture.cleanup().await;
}

/// **The two shapes are mutually exclusive.** Cancelling reverses the whole
/// issue entry, so on an invoice already partly credited it would take the
/// credited part back twice — in the books and in the return.
#[tokio::test]
async fn a_partly_credited_invoice_cannot_also_be_cancelled() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-6",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    credit_part(
        &fixture,
        "INV-PC-6",
        "part",
        vec![credit_line(0, riyals(300))],
    )
    .await
    .expect("credits part");

    let refused = credit(&fixture, "INV-PC-6", "CN-WHOLE")
        .await
        .expect_err("cancelling would credit the same part twice");
    assert!(
        matches!(rejection(&refused), Some(SalesError::AlreadyCredited(_))),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// And the other way round: an invoice cancelled outright has nothing left.
#[tokio::test]
async fn a_cancelled_invoice_cannot_be_partly_credited() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-7",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    credit(&fixture, "INV-PC-7", "CN-WHOLE")
        .await
        .expect("cancels");

    let refused = credit_part(
        &fixture,
        "INV-PC-7",
        "part",
        vec![credit_line(0, riyals(300))],
    )
    .await
    .expect_err("there is nothing left");
    assert!(
        matches!(rejection(&refused), Some(SalesError::AlreadyCredited(_))),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **The reason it exists, in the one place a tax authority looks.** A partial
/// credit note carries its own bands; borrowing the invoice's would take the
/// whole supply out of the return for a document that credited part of it.
#[tokio::test]
async fn a_partial_credit_takes_only_its_own_share_out_of_the_vat_return() {
    let fixture = Fixture::new().await;
    issue_on(
        &fixture,
        "VR-PART",
        "2026-02-01",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
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
    assert_eq!(read(&fixture).await.tax, riyals(150));

    sales::credit_invoice_part(
        &fixture.db,
        &code("VR-PART"),
        &sales::CreditNote {
            reference: "quarter".to_owned(),
            lines: vec![credit_line(0, riyals(250))],
            reason: "partly cancelled".to_owned(),
            on: on("2026-02-20"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("credits");
    fixture.project().await;

    let after = read(&fixture).await;
    assert_eq!(
        after.net,
        riyals(750),
        "three quarters of the supply stands"
    );
    assert_eq!(after.tax, money(11_250), "15% of 750, not zero and not 150");

    fixture.cleanup().await;
}

/// **A credit note falls in its own period**, like every other document. An
/// invoice supplied in Q1 and partly credited in Q2 is a Q1 supply and a Q2
/// adjustment, because re-running a filed return must give the number filed.
#[tokio::test]
async fn a_partial_credit_in_a_later_period_does_not_reach_back() {
    let fixture = Fixture::new().await;
    issue_on(
        &fixture,
        "VR-PART-2",
        "2026-02-01",
        vec![line("Consulting", riyals(1_000), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    sales::credit_invoice_part(
        &fixture.db,
        &code("VR-PART-2"),
        &sales::CreditNote {
            reference: "next-quarter".to_owned(),
            lines: vec![credit_line(0, riyals(500))],
            reason: "partly cancelled".to_owned(),
            on: on("2026-05-05"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("credits");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let q1 = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    let q2 = sales::vat_return(&mut conn, sar(), on("2026-04-01"), on("2026-07-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(q1.tax, riyals(150), "Q1 is what was filed");
    assert_eq!(q2.tax, money(-7_500), "the adjustment lands in Q2");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Line-level allowances
// ---------------------------------------------------------------------------

fn line_with(
    description: &str,
    net: Money,
    category: VatCategory,
    allowances: Vec<sales::Allowance>,
) -> DraftLine {
    DraftLine {
        description: description.to_owned(),
        net,
        category,
        allowances,
        product: None,
        quantity: None,
        serials: Vec::new(),
        lot: None,
    }
}

fn off(reason: &str, amount: Money) -> sales::Allowance {
    sales::Allowance {
        reason: reason.to_owned(),
        amount,
    }
}

/// **A line's allowance comes off before the tax**, because the standard says
/// the line net amount *is* the price less its allowances — and that is the
/// figure a return is built from.
#[tokio::test]
async fn an_allowance_on_a_line_reduces_what_that_line_is_taxed_on() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-LA-1",
        vec![line_with(
            "Consulting",
            riyals(1_000),
            VatCategory::Standard,
            vec![off("Loyalty", riyals(100))],
        )],
    )
    .await
    .expect("issues");
    fixture.project().await;

    // 1,000 less 100 is 900, taxed at 15% is 135.
    let invoice = fixture.invoice("INV-LA-1").await.expect("is there");
    assert_eq!(
        invoice.summary.net,
        riyals(900),
        "net is after the allowance"
    );
    assert_eq!(invoice.summary.tax, money(13_500));
    assert_eq!(invoice.summary.gross, money(103_500));

    assert_eq!(fixture.balance("4000").await, riyals(-900), "revenue");
    assert_eq!(fixture.balance("2100").await, money(-13_500), "output tax");

    fixture.cleanup().await;
}

/// It reaches the VAT return as the smaller number, which is the only place
/// getting this wrong would cost anything.
#[tokio::test]
async fn a_line_allowance_reaches_the_vat_return() {
    let fixture = Fixture::new().await;
    issue_on(
        &fixture,
        "VR-LA",
        "2026-02-01",
        vec![line_with(
            "Consulting",
            riyals(1_000),
            VatCategory::Standard,
            vec![off("Loyalty", riyals(200))],
        )],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let filed = sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(filed.net, riyals(800), "800, not 1,000");
    assert_eq!(filed.tax, riyals(120), "15% of 800");

    fixture.cleanup().await;
}

/// **A line allowance and a document discount are different things**, and an
/// invoice can carry both: the line one comes off first, then the document one
/// comes off the band.
#[tokio::test]
async fn a_line_allowance_and_a_document_discount_both_apply() {
    let fixture = Fixture::new().await;
    sales::issue_invoice(
        &fixture.db,
        &code("INV-LA-2"),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: sales::Customer::new("سارة"),
            issued_on: when(),
            due_on: None,
            currency: sar(),
            lines: vec![line_with(
                "Consulting",
                riyals(1_000),
                VatCategory::Standard,
                vec![off("Loyalty", riyals(100))],
            )],
            discounts: vec![sales::DraftDiscount {
                reason: "Goodwill".to_owned(),
                amount: riyals(50),
                category: VatCategory::Standard,
            }],
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("issues");
    fixture.project().await;

    // 1,000 − 100 (line) = 900; − 50 (document) = 850; tax 127.50.
    let invoice = fixture.invoice("INV-LA-2").await.expect("is there");
    assert_eq!(invoice.summary.net, riyals(850));
    assert_eq!(invoice.summary.tax, money(12_750));
    // **The line one is inside the line; the document one is the invoice's.**
    // 850 could only come from both being applied, each once.

    fixture.cleanup().await;
}

/// An allowance bigger than the line it comes off is refused. A line that comes
/// to nothing or less is not a discount, it is a mistake.
#[tokio::test]
async fn an_allowance_cannot_be_larger_than_its_line() {
    let fixture = Fixture::new().await;
    let refused = issue(
        &fixture,
        "INV-LA-3",
        vec![line_with(
            "Consulting",
            riyals(100),
            VatCategory::Standard,
            vec![off("Too much", riyals(100))],
        )],
    )
    .await
    .expect_err("an allowance that swallows its line");
    assert!(
        matches!(rejection(&refused), Some(SalesError::Tax(_))),
        "{refused:?}"
    );

    let refused = issue(
        &fixture,
        "INV-LA-4",
        vec![line_with(
            "Consulting",
            riyals(100),
            VatCategory::Standard,
            vec![off("Backwards", riyals(-10))],
        )],
    )
    .await
    .expect_err("a negative allowance is a surcharge");
    assert!(
        matches!(rejection(&refused), Some(SalesError::Tax(_))),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **It survives a rebuild.** The allowance is on the event, so a replay
/// reproduces the line and the document it renders to.
#[tokio::test]
async fn a_line_allowance_survives_a_rebuild() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-LA-5",
        vec![line_with(
            "Consulting",
            riyals(1_000),
            VatCategory::Standard,
            vec![off("Loyalty", riyals(100)), off("Damaged", riyals(50))],
        )],
    )
    .await
    .expect("issues");
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT reason, amount FROM proj_sales.invoice_line_allowance
          WHERE invoice_id = 'INV-LA-5' ORDER BY allowance_index",
    )
    .fetch_all(&pool)
    .await
    .expect("reads");
    pool.close().await;

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], ("Loyalty".to_owned(), 10_000));
    assert_eq!(rows[1], ("Damaged".to_owned(), 5_000));

    let invoice = fixture.invoice("INV-LA-5").await.expect("is there");
    assert_eq!(invoice.summary.net, riyals(850));

    fixture.cleanup().await;
}

/// **Both caps, because they catch different things.**
///
/// A document discount comes off the *band*, so an invoice's lines sum to more
/// than its bands whenever it carried one — two lines of 100 with 50 off the
/// document were charged 150, not 200. The per-line cap would happily allow
/// both lines in full; the band cap is what refuses the extra 50 and the 7.50
/// of VAT that was never collected on it.
#[tokio::test]
async fn the_band_cap_still_bites_when_a_document_discount_shrank_the_invoice() {
    let fixture = Fixture::new().await;
    sales::issue_invoice(
        &fixture.db,
        &code("INV-PC-8"),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: sales::Customer::new("سارة"),
            issued_on: when(),
            due_on: None,
            currency: sar(),
            lines: vec![
                line("Consulting", riyals(100), VatCategory::Standard),
                line("Training", riyals(100), VatCategory::Standard),
            ],
            discounts: vec![sales::DraftDiscount {
                reason: "Goodwill".to_owned(),
                amount: riyals(50),
                category: VatCategory::Standard,
            }],
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("issues");
    fixture.project().await;

    let invoice = fixture.invoice("INV-PC-8").await.expect("is there");
    assert_eq!(invoice.summary.net, riyals(150), "the lines say 200");

    // Each line is within its own cap, and together they exceed the band.
    let refused = credit_part(
        &fixture,
        "INV-PC-8",
        "both-in-full",
        vec![credit_line(0, riyals(100)), credit_line(1, riyals(100))],
    )
    .await
    .expect_err("200 credited against 150 charged");
    assert!(
        matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
        "{refused:?}"
    );

    // What was actually charged, credited in full, is fine.
    credit_part(
        &fixture,
        "INV-PC-8",
        "what-was-charged",
        vec![credit_line(0, riyals(100)), credit_line(1, riyals(50))],
    )
    .await
    .expect("150 is what it came to");

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0), "square");
    assert_eq!(fixture.balance("2100").await, money(0), "no tax left");

    fixture.cleanup().await;
}

/// **The document says what the invoice said.** The description and the rate
/// come off the named line, so a credit note cannot describe something that was
/// never sold — which is what it could do when the caller typed them.
#[tokio::test]
async fn a_credit_note_takes_its_wording_and_its_rate_from_the_line() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-9",
        vec![
            line("Colour treatment", riyals(300), VatCategory::Standard),
            line("Exported advice", riyals(200), VatCategory::Zero),
        ],
    )
    .await
    .expect("issues");

    let credited = credit_part(
        &fixture,
        "INV-PC-9",
        "returned",
        vec![credit_line(1, riyals(200))],
    )
    .await
    .expect("credits the zero-rated line");
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let rows: Vec<(String, i64, String, i32)> = sqlx::query_as(
        "SELECT description, net, vat_category, vat_rate_bp
           FROM proj_sales.credit_note_line WHERE credit_note_id = $1",
    )
    .bind(&credited.number)
    .fetch_all(&pool)
    .await
    .expect("reads");
    pool.close().await;

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "Exported advice", "it invented a description");
    assert_eq!(rows[0].2, "zero", "it credited at the wrong treatment");
    assert_eq!(rows[0].3, 0, "it credited at the wrong rate");

    // Zero-rated, so nothing comes off the tax.
    assert_eq!(fixture.balance("2100").await, money(-4_500), "15% of 300");

    fixture.cleanup().await;
}

/// Two lines of one credit note naming the same invoice line are capped on what
/// they come to **together**, not each in turn.
#[tokio::test]
async fn one_credit_note_cannot_take_a_line_twice() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-10",
        // **A second line, so the band has room.** With one line the band cap
        // would catch this and the test would prove nothing about the line cap.
        vec![
            line("Consulting", riyals(100), VatCategory::Standard),
            line("Products", riyals(100), VatCategory::Standard),
        ],
    )
    .await
    .expect("issues");

    let refused = credit_part(
        &fixture,
        "INV-PC-10",
        "twice",
        vec![credit_line(0, riyals(60)), credit_line(0, riyals(60))],
    )
    .await
    .expect_err("120 off a line of 100, with band room to spare");
    assert!(
        matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **An invoice credited in full refuses the next credit note**, whether it got
/// there in one go or in instalments. Nothing is left to take back.
#[tokio::test]
async fn a_fully_credited_invoice_refuses_another_credit_note() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-PC-11",
        vec![
            line("Consulting", riyals(300), VatCategory::Standard),
            line("Exported", riyals(200), VatCategory::Zero),
        ],
    )
    .await
    .expect("issues");

    // Down to nothing, in pieces.
    for (nth, (against, amount)) in [(0_u16, riyals(100)), (0, riyals(200)), (1, riyals(200))]
        .into_iter()
        .enumerate()
    {
        credit_part(
            &fixture,
            "INV-PC-11",
            &format!("part-{nth}"),
            vec![credit_line(against, amount)],
        )
        .await
        .expect("credits");
    }

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0), "nothing owed");
    assert_eq!(fixture.balance("4000").await, money(0), "no supply stands");
    assert_eq!(fixture.balance("2100").await, money(0), "no tax owed");

    // **And now there is nothing left**, on either line, however small.
    for against in [0_u16, 1] {
        let refused = credit_part(
            &fixture,
            "INV-PC-11",
            &format!("one-more-{against}"),
            vec![credit_line(against, money(1))],
        )
        .await
        .expect_err("a fully credited invoice was credited again");
        assert!(
            matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
            "{refused:?}"
        );
    }

    // Nothing moved on the refusal.
    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0));
    let notes = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        sales::credit_notes(&mut conn, "INV-PC-11")
            .await
            .expect("reads")
    };
    assert_eq!(notes.len(), 3, "a refused credit note was issued anyway");

    fixture.cleanup().await;
}

/// **The discounted case, where the line cap alone would let it through.**
///
/// Two lines of 100 with 50 off the document were charged 150. Credit that 150
/// and the invoice is square — but 50 of *line* room is still sitting there,
/// because lines are stated before a document discount. Only the band cap knows
/// the difference, and this is the test that would fail if it were dropped.
#[tokio::test]
async fn a_discounted_invoice_credited_to_its_band_refuses_the_line_room_left_over() {
    let fixture = Fixture::new().await;
    sales::issue_invoice(
        &fixture.db,
        &code("INV-PC-12"),
        &Draft {
            prepayment: false,
            prepaid: None,
            customer: sales::Customer::new("سارة"),
            issued_on: when(),
            due_on: None,
            currency: sar(),
            lines: vec![
                line("Consulting", riyals(100), VatCategory::Standard),
                line("Training", riyals(100), VatCategory::Standard),
            ],
            discounts: vec![sales::DraftDiscount {
                reason: "Goodwill".to_owned(),
                amount: riyals(50),
                category: VatCategory::Standard,
            }],
            note: String::new(),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("issues");

    credit_part(
        &fixture,
        "INV-PC-12",
        "all-of-it",
        vec![credit_line(0, riyals(100)), credit_line(1, riyals(50))],
    )
    .await
    .expect("credits what was charged");

    fixture.project().await;
    assert_eq!(fixture.balance("1100").await, money(0), "square");
    assert_eq!(fixture.balance("2100").await, money(0), "no tax owed");

    // Line 1 still has 50 of face value that was never charged. The band knows.
    let refused = credit_part(
        &fixture,
        "INV-PC-12",
        "the-leftover",
        vec![credit_line(1, riyals(50))],
    )
    .await
    .expect_err("credited the discount back as though it had been charged");
    assert!(
        matches!(rejection(&refused), Some(SalesError::CreditTooLarge { .. })),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Issuing a credit note is a claim
// ---------------------------------------------------------------------------

/// Puts a supervisor on the chart with `sales:approve_credit_note`, and a
/// colleague without it.
async fn staff_with_the_credit_claim(fixture: &Fixture) {
    for (id, identity) in [("EMP-KHALID", "khalid"), ("EMP-SARA", "sara")] {
        hr::hire(
            &fixture.db,
            &code(id),
            &hr::Hire {
                details: hr::Details {
                    name: id.to_owned(),
                    name_latin: None,
                    national_id: None,
                    email: Some(format!("{identity}@acme.test")),
                    phone: None,
                },
                reports_to: None,
                branch: None,
                at: "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
            },
            &Metadata::default(),
        )
        .await
        .expect("hires");
        hr::link_login(
            &fixture.db,
            &code(id),
            identity,
            "2026-01-01T00:00:00Z".parse().expect("a timestamp"),
            &Metadata::default(),
        )
        .await
        .expect("links a login");
    }
    let pool = fixture.tenant_pool().await;
    let owned = hr::projections();
    let refs: Vec<&dyn Projection<Group = hr::Hr>> = owned.iter().map(AsRef::as_ref).collect();
    run_to_head::<hr::Hr>(&pool, &refs, hr::upcasters(), 200)
        .await
        .expect("hr projects");

    hr::grant_claim(
        &fixture.db,
        &code("EMP-KHALID"),
        &hr::Claim {
            name: sales::APPROVE_CREDIT_NOTE.to_owned(),
            branch: None,
        },
        false,
    )
    .await
    .expect("grants the claim");
}

/// **Cancelling an invoice needs the claim, and so does crediting part of
/// one** — the same authority, asked for in the roots both paths go through
/// (§70), and asked for **only of a member who is not the owner**, which is the
/// branch the document limit is judged in.
#[tokio::test]
async fn crediting_an_invoice_needs_the_claim_once_the_tenant_uses_claims() {
    let fixture = Fixture::new().await;
    let by = |actor: &str| Metadata {
        actor: Some(actor.to_owned()),
        ..Metadata::default()
    };
    let db = &fixture.db;
    let cancel = |invoice: &'static str, note: &'static str, actor: &'static str, authority| {
        let by = by(actor);
        async move {
            sales::cancel_invoice(
                db,
                &code(invoice),
                note,
                "a mistake",
                on("2026-03-02"),
                &by,
                authority,
            )
            .await
        }
    };

    for id in ["INV-A", "INV-B", "INV-C", "INV-D", "INV-E"] {
        issue(
            &fixture,
            id,
            vec![line("Consulting", riyals(100), VatCategory::Standard)],
        )
        .await
        .expect("issues");
    }

    // Nothing granted: unchanged for everyone.
    cancel("INV-A", "CN-0", "sara", MEMBER)
        .await
        .expect("no claims in this tenant means no control");

    staff_with_the_credit_claim(&fixture).await;

    let refused = cancel("INV-B", "CN-1", "sara", MEMBER).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotApproved(_)
            )))
        ),
        "sara does not hold the claim, got {refused:?}"
    );

    cancel("INV-B", "CN-2", "khalid", MEMBER)
        .await
        .expect("khalid holds it");

    // **The owner is not asked**, the way they are not held to the document
    // limit: switching a control on must not strand the person who did.
    cancel("INV-C", "CN-3", "sara", OWNER)
        .await
        .expect("an owner is never refused their own credit note");

    // **Nobody acting is not asked either.** A worker's sweep and a gateway's
    // confirmed refund say `System` in so many words, and there is nobody to
    // hold a claim.
    cancel("INV-D", "CN-4", "sara", sales::Authority::System)
        .await
        .expect("a credit note nobody issued is not claim-judged");

    // **A refund that clears an invoice issues a whole-invoice credit note**,
    // and since §70 that one is asked for like any other: the member refused at
    // the credit-note route cannot refund their way to the same document.
    pay(&fixture, "INV-E", "wire-1", riyals(115))
        .await
        .expect("paid in full");
    let refused = sales::refund_invoice(
        &fixture.db,
        &code("INV-E"),
        &Receipt {
            reference: "back-1".to_owned(),
            amount: riyals(115),
            received_on: on("2026-03-02"),
            into: code("1010"),
        },
        "returned",
        &by("sara"),
        MEMBER,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotApproved(_)
            )))
        ),
        "clearing an invoice issues a credit note, got {refused:?}"
    );

    fixture.cleanup().await;
}

/// **Crediting part of an invoice is the same authority as cancelling all of
/// it**, and this test exists because a falsification of the shared helper
/// initially proved nothing — there was no test on this path at all.
#[tokio::test]
async fn a_partial_credit_needs_the_claim_too() {
    let fixture = Fixture::new().await;
    issue(
        &fixture,
        "INV-P",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");
    staff_with_the_credit_claim(&fixture).await;

    let note = |actor: &str, reference: &str| sales::CreditNote {
        reference: reference.to_owned(),
        lines: vec![credit_line(0, riyals(10))],
        reason: format!("asked by {actor}"),
        on: on("2026-03-02"),
    };
    let by = |actor: &str| Metadata {
        actor: Some(actor.to_owned()),
        ..Metadata::default()
    };

    let refused = sales::credit_invoice_part(
        &fixture.db,
        &code("INV-P"),
        &note("sara", "CP-1"),
        &by("sara"),
        MEMBER,
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotApproved(_)
            )))
        ),
        "a partial credit is still a credit note, got {refused:?}"
    );

    sales::credit_invoice_part(
        &fixture.db,
        &code("INV-P"),
        &note("khalid", "CP-2"),
        &by("khalid"),
        MEMBER,
    )
    .await
    .expect("khalid holds the claim");

    fixture.cleanup().await;
}

/// **A gateway refund is asked for the claim when a credit note will follow
/// it**, because by the time one is issued there is nobody left to ask.
///
/// `payments::request_refund_in` judges the member here; `payments::refund_in`
/// then issues the credit note with `System`, which no claim judges. Whole or
/// part — a partial refund of a single-band invoice credits exactly what went
/// back, and that document is a credit note like any other.
#[tokio::test]
async fn a_gateway_refund_is_asked_for_the_claim_when_it_will_credit() {
    let fixture = Fixture::new().await;
    for id in ["INV-GR-1", "INV-GR-2", "INV-GR-3", "INV-GR-4", "INV-GR-5"] {
        issue(
            &fixture,
            id,
            vec![line("Consulting", riyals(100), VatCategory::Standard)],
        )
        .await
        .expect("issues");
        pay(&fixture, id, "wire-1", riyals(115))
            .await
            .expect("paid in full");
    }
    let asked = async |invoice: &str, refunded: Money, actor: &str, authority| {
        let mut conn = fixture.db.acquire().await.expect("a connection");
        sales::may_refund(&mut conn, &code(invoice), refunded, authority, &by(actor)).await
    };
    let not_approved = |outcome: &Result<(), ExecuteError<SalesError>>| {
        matches!(
            outcome,
            Err(ExecuteError::Rejected(SalesError::NotApproved(_)))
        )
    };

    // Nothing granted: unchanged for everyone, and the invoice is not even read.
    asked("INV-GR-1", riyals(115), "sara", MEMBER)
        .await
        .expect("no claims in this tenant means no control");

    staff_with_the_credit_claim(&fixture).await;

    let refused = asked("INV-GR-2", riyals(115), "sara", MEMBER).await;
    assert!(
        not_approved(&refused),
        "clearing the invoice issues a whole credit note: {refused:?}"
    );
    // 23 back is 20 net at 15%, so it credits exactly what went back.
    let refused = asked("INV-GR-3", riyals(23), "sara", MEMBER).await;
    assert!(
        not_approved(&refused),
        "a partial refund credits exactly what went back, which is a credit note too: {refused:?}"
    );
    // **No net comes to 10 at 15%**, so that refund issues no credit note at
    // all — and a control on issuing one must not refuse a refund that issues
    // none.
    asked("INV-GR-3", riyals(10), "sara", MEMBER)
        .await
        .expect("no credit note follows, so there is nothing to approve");

    asked("INV-GR-2", riyals(115), "khalid", MEMBER)
        .await
        .expect("khalid holds the claim");
    asked("INV-GR-4", riyals(115), "sara", OWNER)
        .await
        .expect("an owner is never refused");
    asked("INV-GR-5", riyals(115), "sara", sales::Authority::System)
        .await
        .expect("a gateway's own answer has nobody to ask");

    fixture.cleanup().await;
}

/// **A retry answers with the document it issued, even after the claim is
/// revoked.**
///
/// `reference` is the client's idempotency key, and the book promises that
/// sending it again is a no-op. So the claim is applied *inside* the decision,
/// after the retry check, exactly where §68 put the document limit's
/// comparison — otherwise a till whose response was lost answers 403 to its own
/// retry and the clerk rings a second credit note by a different reference.
#[tokio::test]
async fn a_retry_answers_with_its_credit_note_after_the_claim_is_revoked() {
    let fixture = Fixture::new().await;
    for id in ["INV-RT-1", "INV-RT-2", "INV-RT-3"] {
        issue(
            &fixture,
            id,
            vec![line("Consulting", riyals(100), VatCategory::Standard)],
        )
        .await
        .expect("issues");
    }
    staff_with_the_credit_claim(&fixture).await;

    let cancel = async |invoice: &str, reference: &str| {
        sales::cancel_invoice(
            &fixture.db,
            &code(invoice),
            reference,
            "a mistake",
            on("2026-03-02"),
            &by("khalid"),
            MEMBER,
        )
        .await
    };
    let credit_part = async |invoice: &str, reference: &str| {
        sales::credit_invoice_part(
            &fixture.db,
            &code(invoice),
            &sales::CreditNote {
                reference: reference.to_owned(),
                lines: vec![credit_line(0, riyals(10))],
                reason: "a mistake".to_owned(),
                on: on("2026-03-02"),
            },
            &by("khalid"),
            MEMBER,
        )
        .await
    };
    let claim = || hr::Claim {
        name: sales::APPROVE_CREDIT_NOTE.to_owned(),
        branch: None,
    };

    let cancelled = cancel("INV-RT-1", "CN-R1")
        .await
        .expect("khalid holds the claim");
    let credited = credit_part("INV-RT-2", "CP-R1")
        .await
        .expect("khalid holds the claim");

    // **The claim moves to somebody else.** Sara is granted it so the tenant
    // still uses claims — without that the control would switch itself off and
    // this would prove nothing.
    hr::revoke_claim(&fixture.db, &code("EMP-KHALID"), &claim())
        .await
        .expect("revokes");
    hr::grant_claim(&fixture.db, &code("EMP-SARA"), &claim(), false)
        .await
        .expect("grants");

    let refused = cancel("INV-RT-3", "CN-R2").await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotApproved(_)
            )))
        ),
        "the control is on and khalid no longer holds it: {refused:?}"
    );

    let again = cancel("INV-RT-1", "CN-R1")
        .await
        .expect("the retry of a credit note khalid was allowed to issue");
    assert!(again.committed.did_nothing());
    assert_eq!(again.number, cancelled.number);

    let again = credit_part("INV-RT-2", "CP-R1")
        .await
        .expect("and the same for a partial credit note");
    assert!(again.committed.did_nothing());
    assert_eq!(again.number, credited.number);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// The document limit
// ---------------------------------------------------------------------------

/// A member who is not the owner, as a route passes one.
const MEMBER: sales::Authority = sales::Authority::Member { owner: false };
const OWNER: sales::Authority = sales::Authority::Member { owner: true };

/// What a route stamps: who is asking.
fn by(actor: &str) -> Metadata {
    Metadata {
        actor: Some(actor.to_owned()),
        ..Metadata::default()
    }
}

/// Sets the limit the way `PUT /v1/sales/document-limit` does, through the
/// same typed setter.
async fn limit_documents_to(fixture: &Fixture, limit: Money, basis: sales::Basis) {
    let mut conn = fixture.db.acquire().await.expect("a connection");
    erp_eventlog::configuration::set(
        &mut conn,
        sales::DocumentLimit::KEY,
        &Some(sales::DocumentLimit::new(limit, basis).expect("a limit")),
        Some("the-owner"),
        None,
    )
    .await
    .expect("the limit is set");
}

/// Puts people on the org chart with logins, each reporting to the one named,
/// and projects `hr` so a login resolves to its employee.
async fn staff(fixture: &Fixture, people: &[(&str, &str, Option<&str>)]) {
    for (id, login, reports_to) in people {
        hr::hire(
            &fixture.db,
            &code(id),
            &hr::Hire {
                details: hr::Details {
                    name: (*id).to_owned(),
                    name_latin: None,
                    national_id: None,
                    email: Some(format!("{login}@acme.test")),
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
    let pool = fixture.tenant_pool().await;
    let owned = hr::projections();
    let refs: Vec<&dyn Projection<Group = hr::Hr>> = owned.iter().map(AsRef::as_ref).collect();
    run_to_head::<hr::Hr>(&pool, &refs, hr::upcasters(), 200)
        .await
        .expect("hr projects");
    pool.close().await;
}

async fn grant_the_exemption(fixture: &Fixture, employee: &str) {
    hr::grant_claim(
        &fixture.db,
        &code(employee),
        &hr::Claim {
            name: sales::EXCEED_DOCUMENT_LIMIT.to_owned(),
            branch: None,
        },
        true,
    )
    .await
    .expect("grants the claim");
}

/// One standard-rated line of `net`, issued by `actor`.
async fn issue_as(
    fixture: &Fixture,
    id: &str,
    net: Money,
    actor: &str,
    authority: sales::Authority,
) -> Result<sales::Numbered, CommandError<SalesError>> {
    issue_invoice(
        &fixture.db,
        &code(id),
        &draft(vec![line("Consulting", net, VatCategory::Standard)]),
        &by(actor),
        authority,
    )
    .await
}

fn over_the_limit(outcome: &Result<sales::Numbered, CommandError<SalesError>>) -> Option<Money> {
    match outcome {
        Err(CommandError::Execute(ExecuteError::Rejected(SalesError::OverDocumentLimit {
            amount,
            ..
        }))) => Some(*amount),
        _ => None,
    }
}

/// **A clerk is held to the owner's limit; the owner and the worker are not.**
///
/// Before a limit exists nothing changes for anybody. After, a clerk's invoice
/// over it is refused naming what it came to, the owner's is not, a login
/// with no employee record is refused like any clerk, a document in another
/// currency is refused rather than counted as under, and a path nobody
/// performs is not judged at all.
#[tokio::test]
async fn a_clerk_is_held_to_the_document_limit_and_the_owner_is_not() {
    let fixture = Fixture::new().await;
    staff(&fixture, &[("EMP-CLERK", "clerk", None)]).await;

    issue_as(&fixture, "INV-NO-LIMIT", riyals(20_000), "clerk", MEMBER)
        .await
        .expect("with no limit set, nothing is refused");

    limit_documents_to(&fixture, riyals(10_000), sales::Basis::AfterVat).await;

    issue_as(&fixture, "INV-NO-LIMIT", riyals(20_000), "clerk", MEMBER)
        .await
        .expect("a retry of an invoice issued before the limit answers with it");
    issue_as(&fixture, "INV-UNDER", riyals(8_000), "clerk", MEMBER)
        .await
        .expect("9,200 after VAT is within 10,000");
    let refused = issue_as(&fixture, "INV-OVER", riyals(10_000), "clerk", MEMBER).await;
    assert_eq!(
        over_the_limit(&refused),
        Some(riyals(11_500)),
        "10,000 net is 11,500 after VAT, over the limit: {refused:?}"
    );
    assert!(
        !fixture.is_issued("INV-OVER").await,
        "a refused invoice left a document behind"
    );

    issue_as(&fixture, "INV-OWNER", riyals(50_000), "the-owner", OWNER)
        .await
        .expect("the owner is never limited");
    issue_as(
        &fixture,
        "INV-WORKER",
        riyals(50_000),
        "",
        sales::Authority::System,
    )
    .await
    .expect("nobody acting is not limited");

    let stranger = issue_as(&fixture, "INV-STRANGER", riyals(10_000), "stranger", MEMBER).await;
    assert!(
        over_the_limit(&stranger).is_some(),
        "a login with no employee record can hold no claim: {stranger:?}"
    );

    let mut dollars = draft(vec![line(
        "Consulting",
        Money::from_minor(100, usd()),
        VatCategory::Standard,
    )]);
    dollars.currency = usd();
    let refused = issue_invoice(
        &fixture.db,
        &code("INV-USD"),
        &dollars,
        &by("clerk"),
        MEMBER,
    )
    .await;
    assert_eq!(
        over_the_limit(&refused).map(Money::currency),
        Some(usd()),
        "a dollar invoice cannot be judged against a riyal limit, and is refused: {refused:?}"
    );

    fixture.cleanup().await;
}

/// **The basis is the question at the boundary.** Net 9,500 is 10,925 with
/// VAT: over a 10,000 limit after VAT, within it before.
#[tokio::test]
async fn the_basis_decides_which_total_is_held_to_the_limit() {
    let fixture = Fixture::new().await;
    staff(&fixture, &[("EMP-CLERK", "clerk", None)]).await;

    limit_documents_to(&fixture, riyals(10_000), sales::Basis::AfterVat).await;
    let refused = issue_as(&fixture, "INV-GROSS", riyals(9_500), "clerk", MEMBER).await;
    assert_eq!(
        over_the_limit(&refused),
        Some(riyals(10_925)),
        "{refused:?}"
    );

    limit_documents_to(&fixture, riyals(10_000), sales::Basis::BeforeVat).await;
    issue_as(&fixture, "INV-NET", riyals(9_500), "clerk", MEMBER)
        .await
        .expect("9,500 before VAT is within 10,000");
    let refused = issue_as(&fixture, "INV-NET-OVER", money(1_000_001), "clerk", MEMBER).await;
    assert_eq!(
        over_the_limit(&refused),
        Some(money(1_000_001)),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **The claim is the allow list, and it travels the way every claim does:
/// up.** Granted to a supervisor, it exempts them and the manager above them,
/// and not the clerk beneath — a manager holds what their reports hold, never
/// the reverse.
#[tokio::test]
async fn the_claim_lifts_the_limit_for_whoever_holds_it_and_everyone_above() {
    let fixture = Fixture::new().await;
    staff(
        &fixture,
        &[
            ("EMP-BOSS", "boss", None),
            ("EMP-SUPERVISOR", "supervisor", Some("EMP-BOSS")),
            ("EMP-CLERK", "clerk", Some("EMP-SUPERVISOR")),
        ],
    )
    .await;
    limit_documents_to(&fixture, riyals(10_000), sales::Basis::AfterVat).await;
    grant_the_exemption(&fixture, "EMP-SUPERVISOR").await;

    issue_as(
        &fixture,
        "INV-SUPERVISOR",
        riyals(20_000),
        "supervisor",
        MEMBER,
    )
    .await
    .expect("granted to them");
    issue_as(&fixture, "INV-BOSS", riyals(20_000), "boss", MEMBER)
        .await
        .expect("inherited from the supervisor beneath them");
    let refused = issue_as(&fixture, "INV-CLERK", riyals(20_000), "clerk", MEMBER).await;
    assert!(
        over_the_limit(&refused).is_some(),
        "a claim does not travel down the chart: {refused:?}"
    );

    grant_the_exemption(&fixture, "EMP-CLERK").await;
    issue_as(&fixture, "INV-CLERK-2", riyals(20_000), "clerk", MEMBER)
        .await
        .expect("granted to the clerk now");

    fixture.cleanup().await;
}

/// **A credit note is a document too**, whole or in part, and so is the one a
/// refund issues. Judged on what it credits, after the state checks: a whole
/// cancellation the invoice would refuse anyway is not what is reported.
#[tokio::test]
async fn a_credit_note_over_the_limit_is_refused_whole_or_in_part() {
    let fixture = Fixture::new().await;
    staff(&fixture, &[("EMP-CLERK", "clerk", None)]).await;
    issue_as(&fixture, "INV-BIG", riyals(20_000), "the-owner", OWNER)
        .await
        .expect("issues");
    limit_documents_to(&fixture, riyals(10_000), sales::Basis::AfterVat).await;

    let whole = sales::cancel_invoice(
        &fixture.db,
        &code("INV-BIG"),
        "CN-WHOLE",
        "a mistake",
        on("2026-03-02"),
        &by("clerk"),
        MEMBER,
    )
    .await;
    assert_eq!(over_the_limit(&whole), Some(riyals(23_000)), "{whole:?}");

    let part = |reference: &str, net: Money| sales::CreditNote {
        reference: reference.to_owned(),
        lines: vec![sales::CreditLine {
            against: 0,
            net,
            quantity: None,
            serials: Vec::new(),
        }],
        reason: "partly returned".to_owned(),
        on: on("2026-03-02"),
    };
    let refused = sales::credit_invoice_part(
        &fixture.db,
        &code("INV-BIG"),
        &part("CP-OVER", riyals(10_000)),
        &by("clerk"),
        MEMBER,
    )
    .await;
    assert_eq!(
        over_the_limit(&refused),
        Some(riyals(11_500)),
        "{refused:?}"
    );
    sales::credit_invoice_part(
        &fixture.db,
        &code("INV-BIG"),
        &part("CP-UNDER", riyals(5_000)),
        &by("clerk"),
        MEMBER,
    )
    .await
    .expect("5,750 is within the limit");

    fixture.cleanup().await;
}

/// **A refund is judged on what goes back** — after VAT the money itself,
/// before it the invoice's own share of net — and the credit note a refund
/// issues is judged as well.
#[tokio::test]
async fn a_refund_over_the_limit_is_refused() {
    let fixture = Fixture::new().await;
    staff(&fixture, &[("EMP-CLERK", "clerk", None)]).await;
    for id in ["INV-PAID", "INV-PART"] {
        issue_as(&fixture, id, riyals(20_000), "the-owner", OWNER)
            .await
            .expect("issues");
    }
    pay(&fixture, "INV-PAID", "wire-1", riyals(23_000))
        .await
        .expect("paid in full");
    pay(&fixture, "INV-PART", "wire-1", riyals(5_000))
        .await
        .expect("paid in part");
    // Two bands: a refund of part of it issues no credit note at all, so the
    // refund is the only document there is to judge.
    issue_invoice(
        &fixture.db,
        &code("INV-MIXED"),
        &draft(vec![
            line("Consulting", riyals(10_000), VatCategory::Standard),
            line("Export", riyals(10_000), VatCategory::Zero),
        ]),
        &by("the-owner"),
        OWNER,
    )
    .await
    .expect("issues");
    pay(&fixture, "INV-MIXED", "wire-1", riyals(21_500))
        .await
        .expect("paid in full");

    let refund = |invoice: &'static str, reference: &'static str, amount: Money| {
        refund_as_the_clerk(&fixture, invoice, reference, amount)
    };
    let refused_by_limit = |outcome: &Outcome| {
        matches!(
            outcome,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::OverDocumentLimit { .. }
            )))
        )
    };

    limit_documents_to(&fixture, riyals(10_000), sales::Basis::AfterVat).await;
    let refused = refund("INV-PAID", "back-1", riyals(11_500)).await;
    assert!(
        refused_by_limit(&refused),
        "11,500 back is over 10,000: {refused:?}"
    );
    let refused = refund("INV-MIXED", "back-1", riyals(11_000)).await;
    assert!(
        refused_by_limit(&refused),
        "a refund with no credit note is still judged: {refused:?}"
    );

    // The same 11,500 is 10,000 of the invoice before VAT: within a limit on
    // that basis, to the halala.
    limit_documents_to(&fixture, riyals(10_000), sales::Basis::BeforeVat).await;
    refund("INV-PAID", "back-2", riyals(11_500))
        .await
        .expect("10,000 before VAT is within 10,000");

    // 5,000 back leaves the part-paid invoice holding nothing, so the refund
    // cancels it — and that credit note is the whole invoice, 20,000 before
    // VAT, which the clerk may not issue.
    let refused = refund("INV-PART", "back-1", riyals(5_000)).await;
    assert!(
        refused_by_limit(&refused),
        "the whole-invoice credit note a refund issues is judged too: {refused:?}"
    );

    fixture.cleanup().await;
}

async fn refund_as_the_clerk(
    fixture: &Fixture,
    invoice: &str,
    reference: &str,
    amount: Money,
) -> Outcome {
    sales::refund_invoice(
        &fixture.db,
        &code(invoice),
        &Receipt {
            reference: reference.to_owned(),
            amount,
            received_on: on("2026-03-02"),
            into: code("1010"),
        },
        "returned",
        &by("clerk"),
        MEMBER,
    )
    .await
}

// ---------------------------------------------------------------------------
// The shelf
//
// **An invoice is what takes stock off a shelf.** Every one this system issues
// goes through `issue_in`, which is where `inventory::consume_in` is called —
// so a till sale, a booking bill and a `/v1/sales` invoice all deplete through
// one path (decision 2) and these tests cover all three at once. What is here
// is the contract between the two modules: what comes off, at what cost, what
// is refused, and what a credit note puts back.
// ---------------------------------------------------------------------------

/// Sacks of beans. A plain product: no batches, no names.
const BEANS: &str = "f81d4fae-7dec-11d0-a765-00a0c91e6bf6";
/// Milk. Lot-tracked, so a sale it cannot cover is refused (R1).
const MILK: &str = "9f2a6d0c-11d0-7dec-a765-00a0c91e6bf7";
/// A grinder. Serial-tracked: every unit has a name (decision 17).
const GRINDER: &str = "3c1b7e55-0a44-4f2d-8b9c-2d5a1e6f7a88";
/// A second place for a shelf to be.
const OLAYA: &str = "BRANCH-OLAYA";

impl Fixture {
    /// The tenant, plus the three accounts a stock movement touches.
    async fn keeping_stock() -> Self {
        let fixture = Self::new().await;
        for (account, kind) in [
            ("1300", AccountKind::Asset),     // Inventory
            ("2010", AccountKind::Liability), // Goods received, not invoiced
            ("5010", AccountKind::Expense),   // Cost of goods sold
        ] {
            fixture.open(account, kind, sar()).await;
        }
        fixture
    }

    async fn declare(&self, product: &str, tracking: inventory::Tracking) {
        inventory::declare(
            &self.db,
            &code(product),
            product,
            "piece",
            tracking,
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("the product is declared");
    }

    /// A delivery: `quantity` units for `value` altogether.
    async fn receive(&self, product: &str, quantity: i64, value: Money, reference: &str) {
        self.receiving(product, quantity, value, reference, None, Vec::new())
            .await;
    }

    async fn receiving(
        &self,
        product: &str,
        quantity: i64,
        value: Money,
        reference: &str,
        batch: Option<(&str, &str)>,
        serials: Vec<String>,
    ) {
        inventory::receive(
            &self.db,
            &code(product),
            &inventory::Receipt {
                quantity,
                value,
                code: batch.map(|(code, _)| code.to_owned()),
                expires_on: batch.map(|(_, day)| day.parse().expect("a date")),
                serials,
                reference: reference.to_owned(),
                at: on("2026-01-02"),
            },
            &Metadata::default(),
        )
        .await
        .expect("the delivery lands");
    }

    /// **The shelf as the write side holds it**, rehydrated from the log —
    /// never `proj_inventory`, which is a projection and says what the worker
    /// last saw.
    async fn shelf(&self, product: &str) -> inventory::Stock {
        let id = inventory::stock_id(&code(product), None).expect("a key");
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::load::<inventory::Stock>(&mut conn, &id, inventory::upcasters())
            .await
            .expect("loads")
            .aggregate
    }
}

/// A line that sells `quantity` units of `product` at `unit` each.
fn stocked(description: &str, unit: Money, quantity: i64, product: &str) -> DraftLine {
    DraftLine {
        allowances: Vec::new(),
        description: description.to_owned(),
        net: unit,
        category: VatCategory::Standard,
        product: Some(code(product)),
        quantity: Some(quantity),
        serials: Vec::new(),
        lot: None,
    }
}

/// **The whole of it in one test.** Three sacks off a shelf of ten, the line
/// priced per unit and totalled here, the asset down by what those three cost
/// on their own lot and the expense up by the same.
#[tokio::test]
async fn an_invoice_takes_its_lines_off_the_shelf_and_books_what_they_cost() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    // Ten sacks for 100.00 — 10.00 each.
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;

    let issued = issue(
        &fixture,
        "INV-STOCK",
        vec![stocked("بن", riyals(25), 3, BEANS)],
    )
    .await
    .expect("issues");
    assert!(issued.at.is_some());

    let shelf = fixture.shelf(BEANS).await;
    assert_eq!(shelf.on_hand(), 7, "three sacks went out");
    assert_eq!(shelf.value().expect("sums"), Some(riyals(70)));

    fixture.project().await;
    // **The line is 25.00 × 3**, computed and never divided back out.
    let invoice = fixture.invoice("INV-STOCK").await.expect("a row");
    assert_eq!(invoice.summary.net, riyals(75));
    // Three sacks at what *that lot* cost, not at what they sold for.
    assert_eq!(fixture.balance("5010").await, riyals(30));
    assert_eq!(fixture.balance("1300").await, riyals(70));
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

/// **Cost comes from the lot, so one line can cost two things.** Six sacks off
/// a cheap lot of five and a dear lot of five is 50.00 + 18.00, and an average
/// across the shelf would have said 84.00.
#[tokio::test]
async fn one_line_across_two_lots_costs_what_each_lot_cost() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 5, riyals(50), "dn-cheap").await;
    fixture.receive(BEANS, 5, riyals(90), "dn-dear").await;

    issue(
        &fixture,
        "INV-TWO-LOTS",
        vec![stocked("بن", riyals(30), 6, BEANS)],
    )
    .await
    .expect("issues");

    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        riyals(68),
        "an average across the shelf would have charged 84.00"
    );
    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 4);
    assert_eq!(fixture.balance("1300").await, riyals(72));

    fixture.cleanup().await;
}

/// **A retried invoice depletes once.** The client's request timed out and it
/// sent the same one again; the shelf recognises the movement's reference and
/// records nothing, which is the whole reason that reference is derived from
/// the invoice and the line rather than minted (L8).
#[tokio::test]
async fn the_same_invoice_sent_twice_takes_the_stock_once() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;

    let line = || vec![stocked("بن", riyals(25), 3, BEANS)];
    issue(&fixture, "INV-RETRY", line()).await.expect("issues");
    issue(&fixture, "INV-RETRY", line())
        .await
        .expect("the retry is the same invoice");

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 7, "not four");
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, riyals(30));

    fixture.cleanup().await;
}

/// **A tracked product refuses, and takes the document with it** (R1). What is
/// on a lot-tracked shelf is meant to be known exactly: a phantom carton has no
/// batch and no date, so there is nowhere to put it. The invoice is refused in
/// the same transaction, so nothing at all was written.
#[tokio::test]
async fn a_tracked_product_the_shelf_cannot_cover_refuses_and_leaves_no_invoice() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(MILK, inventory::Tracking::Lot).await;
    fixture
        .receiving(
            MILK,
            2,
            riyals(20),
            "dn-milk",
            Some(("B-2026-04", "2026-06-01")),
            Vec::new(),
        )
        .await;

    let refused = issue(
        &fixture,
        "INV-SHORT-MILK",
        vec![stocked("حليب", riyals(9), 3, MILK)],
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::NotEnoughStock { held: 2, wanted: 3 })
            )))
        ),
        "{refused:?}"
    );

    assert!(!fixture.is_issued("INV-SHORT-MILK").await, "no document");
    assert_eq!(fixture.shelf(MILK).await.on_hand(), 2, "nothing moved");
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, money(0));

    fixture.cleanup().await;
}

/// **A plain product sells anyway and says what it owes** (decision 16, R1).
/// The till does not stop for a bad count. Two sacks come off the lot at 10.00
/// each and the third is a shortfall at the last unit cost the shelf saw — so
/// the goods that left the building are in the books, and the negative number
/// is the report a count corrects.
#[tokio::test]
async fn a_plain_product_the_shelf_cannot_cover_sells_and_records_the_shortfall() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 2, riyals(20), "dn-1").await;

    issue(
        &fixture,
        "INV-SHORT",
        vec![stocked("بن", riyals(25), 3, BEANS)],
    )
    .await
    .expect("a till does not stop");

    let shelf = fixture.shelf(BEANS).await;
    assert_eq!(shelf.on_hand(), -1, "the shelf owes a sack");
    assert_eq!(
        shelf.value().expect("sums"),
        Some(riyals(-10)),
        "and the value owes what it was charged out at"
    );

    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        riyals(30),
        "two off the lot and one at the last unit cost — not 20.00"
    );
    assert_eq!(fixture.balance("1300").await, riyals(-10));
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

/// **A serial is an identity and identities are not invented** (decision 17).
/// A name that was never received is a wrong input, not a wrong count, and
/// nothing corrects it — so the sale is refused and no document exists.
#[tokio::test]
async fn a_line_naming_a_unit_that_is_not_on_the_shelf_refuses() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(GRINDER, inventory::Tracking::Serial).await;
    fixture
        .receiving(
            GRINDER,
            1,
            riyals(300),
            "dn-grinder",
            None,
            vec!["SN-1".to_owned()],
        )
        .await;

    let mut line = stocked("مطحنة", riyals(500), 1, GRINDER);
    line.serials = vec!["SN-NOBODY".to_owned()];
    let refused = issue(&fixture, "INV-SERIAL", vec![line]).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::NoSuchSerial(_))
            )))
        ),
        "{refused:?}"
    );
    assert!(!fixture.is_issued("INV-SERIAL").await);
    assert_eq!(fixture.shelf(GRINDER).await.on_hand(), 1);

    fixture.cleanup().await;
}

/// **A credit note puts the goods back where they came from, at what they left
/// at.** Not at today's cost, which would restate a margin already reported,
/// and not at a share of what was credited (decision 12) — the units are the
/// client's statement and the money is another. The lot the sale emptied
/// reopens as itself.
#[tokio::test]
async fn a_returned_line_puts_the_stock_back_and_the_cost_with_it() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 3, riyals(30), "dn-1").await;

    issue(
        &fixture,
        "INV-BACK",
        vec![stocked("بن", riyals(25), 3, BEANS)],
    )
    .await
    .expect("issues");
    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 0, "the lot emptied");

    sales::credit_invoice_part(
        &fixture.db,
        &code("INV-BACK"),
        &sales::CreditNote {
            reference: "returned-two".to_owned(),
            lines: vec![sales::CreditLine {
                against: 0,
                net: riyals(50),
                quantity: Some(2),
                serials: Vec::new(),
            }],
            reason: "أعاد كيسين".to_owned(),
            on: on("2026-03-02"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("credits");

    let shelf = fixture.shelf(BEANS).await;
    assert_eq!(shelf.on_hand(), 2, "two sacks are back");
    assert_eq!(
        shelf.value().expect("sums"),
        Some(riyals(20)),
        "at what they left at, on the lot they left from"
    );

    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        riyals(10),
        "30.00 went out and 20.00 came back"
    );
    assert_eq!(fixture.balance("1300").await, riyals(20));
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

/// **A whole cancellation puts the whole of it back**, at the quantities the
/// lines were sold at — which the invoice itself records, so nothing is derived
/// from the money.
#[tokio::test]
async fn cancelling_an_invoice_puts_everything_back_on_the_shelf() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;
    issue(
        &fixture,
        "INV-CANCEL",
        vec![stocked("بن", riyals(25), 4, BEANS)],
    )
    .await
    .expect("issues");

    sales::cancel_invoice(
        &fixture.db,
        &code("INV-CANCEL"),
        "CN-CLIENT-1",
        "لم يُسلَّم",
        on("2026-03-02"),
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("cancels");

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 10);
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, money(0));
    assert_eq!(fixture.balance("1300").await, riyals(100));

    fixture.cleanup().await;
}

/// **An invoice that sells nothing off a shelf is the invoice this system
/// always issued.** No movement, no cost, and the same posting — which is what
/// makes both new fields a widening rather than a change.
#[tokio::test]
async fn an_invoice_with_no_product_on_it_behaves_exactly_as_before() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;

    issue(
        &fixture,
        "INV-PLAIN",
        vec![line("Consulting", riyals(100), VatCategory::Standard)],
    )
    .await
    .expect("issues");

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 10, "nothing moved");
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, money(0));
    assert_eq!(fixture.balance("1300").await, riyals(100));
    assert_eq!(fixture.balance("1100").await, riyals(115));
    assert_eq!(fixture.balance("4000").await, riyals(-100));

    fixture.cleanup().await;
}

/// **A lot the sale emptied reopens as itself.**
///
/// The batch code and the expiry date are frozen onto the portion when the
/// goods leave, for the same reason the cost is: a lot that empties closes and
/// leaves the aggregate, so by the time the customer brings it back there is
/// nothing left to ask — and the read model may not be asked (L3). Without them
/// a returned carton of milk would come back undated and go out last, which is
/// the opposite of what an expiry rule is for.
#[tokio::test]
async fn a_returned_batch_comes_back_with_its_code_and_its_date() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(MILK, inventory::Tracking::Lot).await;
    fixture
        .receiving(
            MILK,
            2,
            riyals(20),
            "dn-milk",
            Some(("B-2026-04", "2026-06-01")),
            Vec::new(),
        )
        .await;

    issue(
        &fixture,
        "INV-MILK",
        vec![stocked("حليب", riyals(9), 2, MILK)],
    )
    .await
    .expect("issues");
    assert!(
        fixture.shelf(MILK).await.lots.is_empty(),
        "the lot emptied and closed"
    );

    sales::credit_invoice_part(
        &fixture.db,
        &code("INV-MILK"),
        &sales::CreditNote {
            reference: "milk-back".to_owned(),
            lines: vec![sales::CreditLine {
                against: 0,
                net: riyals(18),
                quantity: Some(2),
                serials: Vec::new(),
            }],
            reason: "أعاد الحليب".to_owned(),
            on: on("2026-03-02"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("credits");

    let shelf = fixture.shelf(MILK).await;
    let [lot] = shelf.lots.as_slice() else {
        panic!("one lot, reopened: {:?}", shelf.lots)
    };
    assert_eq!(lot.quantity, 2);
    assert_eq!(lot.code.as_deref(), Some("B-2026-04"), "the batch it was");
    assert_eq!(
        lot.expires_on.map(|d| d.to_string()).as_deref(),
        Some("2026-06-01"),
        "and it still spoils when it always did"
    );
    assert_eq!(lot.value, riyals(20));

    fixture.cleanup().await;
}

/// **Two lines of the same product on one invoice**, which is the case that
/// takes one shelf's lock twice inside one transaction.
///
/// Worth its own test because `consume_in` has no retry loop of its own — it
/// runs in the invoice's transaction, so a version conflict between the two
/// would surface as contention and the whole invoice would be retried until it
/// gave up. The second load sees the first line's own event, and the two
/// movements are told apart by the line's position in the reference.
#[tokio::test]
async fn two_lines_of_one_product_take_the_shelf_twice() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;

    issue(
        &fixture,
        "INV-TWICE",
        vec![
            stocked("بن", riyals(25), 2, BEANS),
            stocked("بن (هدية)", riyals(25), 3, BEANS),
        ],
    )
    .await
    .expect("issues");

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 5, "two then three");
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, riyals(50));

    fixture.cleanup().await;
}

/// Credits 10.00 off line zero of `INV-TWICE-BACK`, claiming `quantity`
/// units came back with it.
async fn returning(
    fixture: &Fixture,
    reference: &str,
    quantity: i64,
) -> Result<sales::Numbered, CommandError<SalesError>> {
    sales::credit_invoice_part(
        &fixture.db,
        &code("INV-TWICE-BACK"),
        &sales::CreditNote {
            reference: reference.to_owned(),
            lines: vec![sales::CreditLine {
                against: 0,
                net: riyals(10),
                quantity: Some(quantity),
                serials: Vec::new(),
            }],
            reason: "أعاد".to_owned(),
            on: on("2026-03-02"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
}

/// **Two credit notes against one sale put back what was sold, and no more.**
///
/// The money's cap does not stand in for the goods': how much of a line is
/// credited and how many units came back are two different statements
/// (decision 12), so a client crediting 10.00 twice off a 75.00 line could
/// claim three sacks each time. The shelf takes each return off what that
/// movement still has out, so the second one is refused for the part the first
/// already brought back.
#[tokio::test]
async fn a_second_credit_note_can_only_return_what_the_first_one_left() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 3, riyals(30), "dn-1").await;
    issue(
        &fixture,
        "INV-TWICE-BACK",
        vec![stocked("بن", riyals(25), 3, BEANS)],
    )
    .await
    .expect("issues");

    returning(&fixture, "back-1", 2)
        .await
        .expect("two come back");
    let refused = returning(&fixture, "back-2", 2).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::MoreThanWasTaken {
                    taken: 1,
                    wanted: 2
                })
            )))
        ),
        "only one sack is still out: {refused:?}"
    );

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 2, "two, never four");
    returning(&fixture, "back-3", 1)
        .await
        .expect("the last one comes back");
    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 3);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// What review found in the slice that depletes stock. Each test below is the
// failure a reviewer reproduced, and each was watched to fail with its fix
// broken. See IMPLEMENTATION.md §74, "What review found".
// ---------------------------------------------------------------------------

impl Fixture {
    /// Enough receipts that a shelf's window of references heard has rolled
    /// past everything before them.
    async fn move_the_shelf_on(&self, product: &str) {
        for n in 0..inventory::stock::HEARD_WINDOW {
            self.receive(product, 1, riyals(10), &format!("dn-later-{n}"))
                .await;
        }
    }
}

/// **A retried invoice takes nothing, however long after.** The invoice is
/// idempotent for ever and the shelf only remembers its last two hundred
/// movements, so a retry that reached the shelf took the stock a second time
/// — while the cost entry, whose id is derived, posted nothing, and the shelf
/// stopped agreeing with `1300 Inventory`.
#[tokio::test]
async fn a_retried_invoice_takes_nothing_once_the_shelf_has_moved_on() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;

    let line = || vec![stocked("بن", riyals(25), 3, BEANS)];
    issue(&fixture, "INV-OLD-RETRY", line())
        .await
        .expect("issues");
    fixture.move_the_shelf_on(BEANS).await;
    let before = fixture.shelf(BEANS).await;

    let retried = issue(&fixture, "INV-OLD-RETRY", line())
        .await
        .expect("the retry is the same invoice");
    assert!(retried.at.is_none(), "nothing was written");

    let after = fixture.shelf(BEANS).await;
    assert_eq!(
        after.on_hand(),
        before.on_hand(),
        "the sale took its three once"
    );
    assert_eq!(after.value().expect("sums"), before.value().expect("sums"));
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, riyals(30));
    assert_eq!(
        Some(fixture.balance("1300").await),
        after.value().expect("sums"),
        "the shelf and the books agree"
    );

    fixture.cleanup().await;
}

/// **An invoice can be cancelled however busy its shelf has been since.** A
/// return used to read what the sale took out of the shelf's window, so once
/// that shelf had seen two hundred more movements the credit note — and with
/// it the statutory cancellation — was refused for good.
#[tokio::test]
async fn an_invoice_can_be_cancelled_long_after_its_shelf_has_moved_on() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;
    issue(
        &fixture,
        "INV-OLD-CANCEL",
        vec![stocked("بن", riyals(25), 4, BEANS)],
    )
    .await
    .expect("issues");
    fixture.move_the_shelf_on(BEANS).await;
    let before = fixture.shelf(BEANS).await.on_hand();

    sales::cancel_invoice(
        &fixture.db,
        &code("INV-OLD-CANCEL"),
        "CN-OLD",
        "لم يُسلَّم",
        on("2026-03-02"),
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("cancels");

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), before + 4);
    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        money(0),
        "the cost came back"
    );

    fixture.cleanup().await;
}

/// Credits one unit off line zero of `invoice` under `reference`.
async fn one_back(
    fixture: &Fixture,
    invoice: &str,
    reference: &str,
    lines: Vec<sales::CreditLine>,
) -> Result<sales::Numbered, CommandError<SalesError>> {
    sales::credit_invoice_part(
        &fixture.db,
        &code(invoice),
        &sales::CreditNote {
            reference: reference.to_owned(),
            lines,
            reason: "أعاد".to_owned(),
            on: on("2026-03-02"),
        },
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
}

fn a_unit_of_line_zero() -> sales::CreditLine {
    sales::CreditLine {
        against: 0,
        net: riyals(25),
        quantity: Some(1),
        serials: Vec::new(),
    }
}

/// **One client reference on two invoices is two returns.** A client key is
/// only unique per invoice, and a return keyed on it alone made the second
/// invoice's unit a "retry" of the first's: its money was credited and its
/// goods never came back.
#[tokio::test]
async fn one_client_reference_on_two_invoices_puts_both_back() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;
    for invoice in ["INV-REF-A", "INV-REF-B"] {
        issue(&fixture, invoice, vec![stocked("بن", riyals(25), 2, BEANS)])
            .await
            .expect("issues");
    }

    for invoice in ["INV-REF-A", "INV-REF-B"] {
        one_back(&fixture, invoice, "RET-1", vec![a_unit_of_line_zero()])
            .await
            .expect("credits");
    }

    assert_eq!(
        fixture.shelf(BEANS).await.on_hand(),
        8,
        "one back from each"
    );
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, riyals(20));

    fixture.cleanup().await;
}

/// **Two lines of one credit note against one invoice line add up.** The
/// credit note may say so, and each line used to be its own return under the
/// same reference — the second heard as a retry and dropped.
#[tokio::test]
async fn two_credit_lines_against_one_invoice_line_put_both_back() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;
    issue(
        &fixture,
        "INV-SPLIT",
        vec![stocked("بن", riyals(25), 3, BEANS)],
    )
    .await
    .expect("issues");

    one_back(
        &fixture,
        "INV-SPLIT",
        "RET-SPLIT",
        vec![a_unit_of_line_zero(), a_unit_of_line_zero()],
    )
    .await
    .expect("credits");

    assert_eq!(
        fixture.shelf(BEANS).await.on_hand(),
        9,
        "both units are back"
    );
    fixture.project().await;
    assert_eq!(fixture.balance("5010").await, riyals(10));

    fixture.cleanup().await;
}

/// **A return lands on the shelf the sale came off**, not the one at the
/// branch that raised the credit note. A sale rung at Olaya and cancelled from
/// head office used to look for the consumption on head office's shelf, find
/// nothing, and refuse the cancellation.
#[tokio::test]
async fn a_cancellation_from_another_branch_puts_the_goods_back_where_they_were_sold() {
    let fixture = Fixture::keeping_stock().await;
    let at_olaya = fixture.opening_olaya().await;

    fixture.declare(BEANS, inventory::Tracking::None).await;
    inventory::receive(
        &fixture.db,
        &code(BEANS),
        &inventory::Receipt {
            quantity: 10,
            value: riyals(100),
            code: None,
            expires_on: None,
            serials: Vec::new(),
            reference: "dn-olaya".to_owned(),
            at: on("2026-01-02"),
        },
        &at_olaya,
    )
    .await
    .expect("the delivery lands at Olaya");
    issue_invoice(
        &fixture.db,
        &code("INV-OLAYA"),
        &draft(vec![stocked("بن", riyals(25), 4, BEANS)]),
        &at_olaya,
        sales::Authority::System,
    )
    .await
    .expect("rung at Olaya");

    sales::cancel_invoice(
        &fixture.db,
        &code("INV-OLAYA"),
        "CN-HQ",
        "لم يُسلَّم",
        on("2026-03-02"),
        &Metadata::default(),
        sales::Authority::System,
    )
    .await
    .expect("head office cancels a sale rung at Olaya");

    let olaya = inventory::stock_id(&code(BEANS), Some(OLAYA)).expect("a key");
    let mut conn = fixture.db.acquire().await.expect("connection");
    let shelf = erp_eventlog::load::<inventory::Stock>(&mut conn, &olaya, inventory::upcasters())
        .await
        .expect("loads")
        .aggregate;
    drop(conn);
    assert_eq!(shelf.on_hand(), 10, "back on Olaya's shelf");
    assert_eq!(
        fixture.shelf(BEANS).await.on_hand(),
        0,
        "and nothing appeared at head office"
    );

    fixture.cleanup().await;
}

/// **Units that cannot go anywhere are refused, not dropped** (L6). A quantity
/// against a line that sold no product, and a quantity of nothing, used to be
/// accepted and ignored — telling the client stock came back when none did.
#[tokio::test]
async fn units_coming_back_that_cannot_land_are_refused() {
    let fixture = Fixture::keeping_stock().await;
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture.receive(BEANS, 10, riyals(100), "dn-1").await;
    issue(
        &fixture,
        "INV-MIXED",
        vec![
            line("Consulting", riyals(100), VatCategory::Standard),
            stocked("بن", riyals(25), 3, BEANS),
        ],
    )
    .await
    .expect("issues");

    let on_a_service = one_back(
        &fixture,
        "INV-MIXED",
        "RET-SERVICE",
        vec![sales::CreditLine {
            against: 0,
            net: riyals(50),
            quantity: Some(5),
            serials: Vec::new(),
        }],
    )
    .await;
    assert!(
        matches!(
            on_a_service,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotAStockLine { line: 0, .. }
            )))
        ),
        "an hour of consultancy has no shelf: {on_a_service:?}"
    );

    let none = one_back(
        &fixture,
        "INV-MIXED",
        "RET-NONE",
        vec![sales::CreditLine {
            against: 1,
            net: riyals(25),
            quantity: Some(0),
            serials: Vec::new(),
        }],
    )
    .await;
    assert!(
        matches!(
            none,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NotAQuantity
            )))
        ),
        "nothing coming back is not a quantity: {none:?}"
    );

    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 7, "and nothing moved");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// A line that names its lot, and a return that names its units. See
// IMPLEMENTATION.md §76.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Opens Olaya and returns a request made there.
    async fn opening_olaya(&self) -> Metadata {
        {
            let mut conn = self.db.acquire().await.expect("connection");
            branches::install(&mut conn).await.expect("branches");
            ensure_group_schema::<branches::Branches>(&mut conn)
                .await
                .expect("the branches' checkpoint");
        }
        branches::open_branch(
            &self.db,
            &code(OLAYA),
            &branches::Details {
                name: "العليا".to_owned(),
                name_latin: None,
                address: branches::Address {
                    street: "طريق الملك فهد".to_owned(),
                    building: None,
                    district: None,
                    city: "الرياض".to_owned(),
                    postal_code: None,
                    country: "SA".to_owned(),
                },
            },
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("the branch opens");
        Metadata::default().at_branch(OLAYA)
    }
}

/// The lot a delivery made on the shelf with no branch.
fn lot_on_the_shelf(product: &str, reference: &str) -> String {
    inventory::lot_of(
        &inventory::stock_id(&code(product), None).expect("a key"),
        reference,
    )
}

/// Milk in two batches: ten of `B-EARLY` at 5.00, which goes off first, and
/// three of `B-LATE` at 8.00. The picking rule would take the early batch.
async fn two_batches_of_milk(fixture: &Fixture) {
    fixture.declare(MILK, inventory::Tracking::Lot).await;
    fixture
        .receiving(
            MILK,
            10,
            riyals(50),
            "dn-early",
            Some(("B-EARLY", "2026-04-01")),
            Vec::new(),
        )
        .await;
    fixture
        .receiving(
            MILK,
            3,
            riyals(24),
            "dn-late",
            Some(("B-LATE", "2026-09-01")),
            Vec::new(),
        )
        .await;
}

/// `quantity` bottles, off the lot the line names.
fn from_lot(quantity: i64, lot: &str) -> DraftLine {
    DraftLine {
        lot: Some(lot.to_owned()),
        ..stocked("حليب", riyals(10), quantity, MILK)
    }
}

/// **A line that names a lot takes from that lot**, whatever the picking rule
/// would have chosen (decision 9) — and at that lot's cost, because it is that
/// lot's bottles that left.
#[tokio::test]
async fn an_invoice_naming_a_lot_takes_from_that_lot() {
    let fixture = Fixture::keeping_stock().await;
    two_batches_of_milk(&fixture).await;
    let late = lot_on_the_shelf(MILK, "dn-late");

    issue(&fixture, "INV-NAMED-LOT", vec![from_lot(2, &late)])
        .await
        .expect("issues");

    let shelf = fixture.shelf(MILK).await;
    assert_eq!(
        shelf.lot(&late).map(|lot| lot.quantity),
        Some(1),
        "two came off the batch the line named"
    );
    assert_eq!(
        shelf
            .lot(&lot_on_the_shelf(MILK, "dn-early"))
            .map(|lot| lot.quantity),
        Some(10),
        "and none off the batch that goes off first"
    );
    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        riyals(16),
        "at the named batch's 8.00, not the early batch's 5.00"
    );

    fixture.cleanup().await;
}

/// **A named lot that cannot cover the line refuses** — although the shelf as
/// a whole could — rather than being topped up from the next batch: whoever
/// named it is holding that batch and is wrong about it. The invoice goes with
/// it. And a lot named with no product is refused before any shelf is asked.
///
/// **This guards a product owner's decision** (D-A, 2026-09-14; §77): a named
/// lot on a *plain* product refuses the same way. It is the one way a plain
/// sale is refused for stock, and it is deliberate — the line asked for that
/// lot. A change here is a change to that decision, not to this test.
#[tokio::test]
async fn a_named_lot_that_cannot_cover_the_line_refuses() {
    let fixture = Fixture::keeping_stock().await;
    two_batches_of_milk(&fixture).await;
    let late = lot_on_the_shelf(MILK, "dn-late");

    let refused = issue(&fixture, "INV-LOT-SHORT", vec![from_lot(4, &late)]).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::LotIsShort {
                    held: 3,
                    wanted: 4,
                    ..
                })
            )))
        ),
        "three in the batch and four asked for: {refused:?}"
    );
    assert!(!fixture.is_issued("INV-LOT-SHORT").await);
    assert_eq!(fixture.shelf(MILK).await.on_hand(), 13, "nothing moved");

    let orphan = DraftLine {
        product: None,
        ..from_lot(1, &late)
    };
    let refused = issue(&fixture, "INV-LOT-ORPHAN", vec![orphan]).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::LotWithoutAProduct { .. }
            )))
        ),
        "a lot of nothing: {refused:?}"
    );

    // **A plain product's named lot refuses the same way.** R1's shortfall is
    // for a line that names nothing; naming a lot is a claim about that lot.
    // Decided by the product owner after §76's review (D-A): the refusal
    // stands, for plain stock as for tracked.
    fixture.declare(BEANS, inventory::Tracking::None).await;
    fixture
        .receiving(BEANS, 2, riyals(20), "dn-beans", None, Vec::new())
        .await;
    let plain = DraftLine {
        lot: Some(lot_on_the_shelf(BEANS, "dn-beans")),
        ..stocked("بن", riyals(10), 3, BEANS)
    };
    let refused = issue(&fixture, "INV-PLAIN-LOT-SHORT", vec![plain]).await;
    assert!(
        matches!(
            refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::LotIsShort {
                    held: 2,
                    wanted: 3,
                    ..
                })
            )))
        ),
        "two in a plain lot and three asked for of that lot: {refused:?}"
    );
    assert_eq!(fixture.shelf(BEANS).await.on_hand(), 2, "nothing moved");

    fixture.cleanup().await;
}

/// **A lot at another branch is not on this shelf.** A shelf is a product at a
/// place, so Olaya's batch named on a sale at head office is refused — even
/// though head office holds milk the line could have had.
#[tokio::test]
async fn a_lot_from_another_branch_refuses() {
    let fixture = Fixture::keeping_stock().await;
    let at_olaya = fixture.opening_olaya().await;
    fixture.declare(MILK, inventory::Tracking::Lot).await;
    inventory::receive(
        &fixture.db,
        &code(MILK),
        &inventory::Receipt {
            quantity: 5,
            value: riyals(25),
            code: Some("B-OLAYA".to_owned()),
            expires_on: None,
            serials: Vec::new(),
            reference: "dn-olaya".to_owned(),
            at: on("2026-01-02"),
        },
        &at_olaya,
    )
    .await
    .expect("the delivery lands at Olaya");
    fixture
        .receiving(
            MILK,
            5,
            riyals(25),
            "dn-hq",
            Some(("B-HQ", "2026-09-01")),
            Vec::new(),
        )
        .await;
    let olaya = inventory::lot_of(
        &inventory::stock_id(&code(MILK), Some(OLAYA)).expect("a key"),
        "dn-olaya",
    );

    let refused = issue(&fixture, "INV-OTHER-SHELF", vec![from_lot(1, &olaya)]).await;
    assert!(
        matches!(
            &refused,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::NoSuchLot(lot))
            ))) if *lot == olaya
        ),
        "Olaya's batch is not on head office's shelf: {refused:?}"
    );
    assert!(!fixture.is_issued("INV-OTHER-SHELF").await);
    assert_eq!(
        fixture.shelf(MILK).await.on_hand(),
        5,
        "head office's own batch is untouched"
    );

    fixture.cleanup().await;
}

/// A delivery of three named grinders at 300.00 each.
async fn three_grinders(fixture: &Fixture) {
    fixture.declare(GRINDER, inventory::Tracking::Serial).await;
    fixture
        .receiving(
            GRINDER,
            3,
            riyals(900),
            "dn-grinders",
            None,
            vec!["SN-1".to_owned(), "SN-2".to_owned(), "SN-3".to_owned()],
        )
        .await;
}

/// A line selling these grinders by name.
fn grinders(serials: &[&str]) -> DraftLine {
    let units = i64::try_from(serials.len()).expect("a count");
    DraftLine {
        serials: serials.iter().map(|serial| (*serial).to_owned()).collect(),
        ..stocked("مطحنة", riyals(500), units, GRINDER)
    }
}

/// A credit note against line zero of `invoice`, saying these grinders came
/// back.
async fn grinders_back(
    fixture: &Fixture,
    invoice: &str,
    reference: &str,
    serials: &[&str],
) -> Result<sales::Numbered, CommandError<SalesError>> {
    one_back(
        fixture,
        invoice,
        reference,
        vec![sales::CreditLine {
            against: 0,
            net: riyals(500),
            quantity: Some(i64::try_from(serials.len()).expect("a count")),
            serials: serials.iter().map(|serial| (*serial).to_owned()).collect(),
        }],
    )
    .await
}

/// **A return names the units that came back — only ones this invoice sold,
/// and each of them once.**
///
/// A phone shop taking back one of two phones says which. `SN-3` went out, on
/// another invoice, so this one cannot bring it back. `SN-2` comes back once;
/// sold again to somebody else it is off the shelf as well, so a check against
/// the shelf would let the first invoice bring it back a second time. What is
/// still out is the sale itself, followed through the shelf's whole stream.
/// And names with no quantity beside them are refused, not skipped.
#[tokio::test]
async fn a_serial_return_names_only_what_the_invoice_sold_and_only_once() {
    let fixture = Fixture::keeping_stock().await;
    three_grinders(&fixture).await;
    issue(
        &fixture,
        "INV-TWO-GRINDERS",
        vec![grinders(&["SN-1", "SN-2"])],
    )
    .await
    .expect("issues");
    issue(&fixture, "INV-ONE-GRINDER", vec![grinders(&["SN-3"])])
        .await
        .expect("issues");

    let uncounted = one_back(
        &fixture,
        "INV-TWO-GRINDERS",
        "back-uncounted",
        vec![sales::CreditLine {
            against: 0,
            net: riyals(500),
            quantity: None,
            serials: vec!["SN-1".to_owned()],
        }],
    )
    .await;
    assert!(
        matches!(
            uncounted,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::NamedUnits { named: 1 }
            )))
        ),
        "a name with no quantity would have credited the money and put nothing back: \
         {uncounted:?}"
    );

    let wrong = grinders_back(&fixture, "INV-TWO-GRINDERS", "back-wrong", &["SN-3"]).await;
    assert!(
        matches!(
            &wrong,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::NotOut(serial))
            ))) if serial == "SN-3"
        ),
        "SN-3 went out on another invoice: {wrong:?}"
    );

    grinders_back(&fixture, "INV-TWO-GRINDERS", "back-sn-2", &["SN-2"])
        .await
        .expect("one of the two comes back, by name");
    let shelf = fixture.shelf(GRINDER).await;
    assert_eq!(shelf.on_hand(), 1);
    assert!(
        shelf.lot_holding("SN-2").is_some() && shelf.lot_holding("SN-1").is_none(),
        "the one on the shelf is the one the customer brought back"
    );

    issue(&fixture, "INV-SN-2-AGAIN", vec![grinders(&["SN-2"])])
        .await
        .expect("sold again");
    let twice = grinders_back(&fixture, "INV-TWO-GRINDERS", "back-sn-2-again", &["SN-2"]).await;
    assert!(
        matches!(
            &twice,
            Err(CommandError::Execute(ExecuteError::Rejected(
                SalesError::Stock(inventory::InventoryError::NotOut(serial))
            ))) if serial == "SN-2"
        ),
        "SN-2 has already come back on that invoice: {twice:?}"
    );
    assert_eq!(fixture.shelf(GRINDER).await.on_hand(), 0);

    fixture.project().await;
    assert_eq!(
        fixture.balance("5010").await,
        riyals(900),
        "900.00 out, 300.00 back, 300.00 out again"
    );
    assert_eq!(fixture.balance("1300").await, riyals(0));
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}
