//! The Saudi tax module, against a real tenant with everything under it.
//!
//! The test that carries this module is [`a_filed_return_records_what_went`]:
//! every other guarantee in the system makes re-running a period give the number
//! that was filed, and those are properties of the arithmetic. This one is a
//! record, and it survives a rebuild because it is an event.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use ledger::{AccountKind, Ledger, VatCategory, open_account};
use purchases::Purchases;
use sales::{Customer, Draft, DraftLine, Sales};
use spa_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use spa_eventlog::{ExecuteError, Metadata};
use spa_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use spa_testkit::{Schema, TestDb};
use spa_types::{AggregateId, CurrencyCode, Money, Timestamp};
use tax_sa::{Sides, TaxError, TaxSa};

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
fn riyals(major: i64) -> Money {
    money(major * 100)
}
fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}
const BOTH: Sides = Sides {
    sells: true,
    buys: true,
};

struct Fixture {
    db: TenantDb,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    tenant_database: String,
}

impl Fixture {
    async fn new() -> Self {
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
            .register_tenant_on("acme", "Acme", "primary", Actor::system())
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
        ledger::install(&mut conn).await.expect("ledger");
        ensure_group_schema::<Ledger>(&mut conn)
            .await
            .expect("ledger checkpoint");
        sales::install(&mut conn).await.expect("sales");
        ensure_group_schema::<Sales>(&mut conn)
            .await
            .expect("sales checkpoint");
        purchases::install(&mut conn).await.expect("purchases");
        ensure_group_schema::<Purchases>(&mut conn)
            .await
            .expect("purchases checkpoint");
        tax_sa::install(&mut conn).await.expect("tax_sa");
        ensure_group_schema::<TaxSa>(&mut conn)
            .await
            .expect("tax checkpoint");
        drop(conn);

        let fixture = Self {
            db,
            _control: control,
            _control_db: control_db,
            tenant_database: tenant.database_name,
        };

        for (account, kind) in [
            ("1010", AccountKind::Asset),
            ("1100", AccountKind::Asset),
            ("1200", AccountKind::Asset),
            ("2000", AccountKind::Liability),
            ("2100", AccountKind::Liability),
            ("4000", AccountKind::Revenue),
            ("5000", AccountKind::Expense),
        ] {
            open_account(
                &fixture.db,
                &code(account),
                account,
                kind,
                sar(),
                &Metadata::default(),
            )
            .await
            .expect("opens");
        }
        fixture
    }

    async fn project(&self) {
        let pool = self.tenant_pool().await;
        for _ in 0..2 {
            let owned = ledger::projections();
            let refs: Vec<&dyn Projection<Group = Ledger>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<Ledger>(&pool, &refs, ledger::upcasters(), 200)
                .await
                .expect("ledger");

            let owned = sales::projections();
            let refs: Vec<&dyn Projection<Group = Sales>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<Sales>(&pool, &refs, sales::upcasters(), 200)
                .await
                .expect("sales");

            let owned = purchases::projections();
            let refs: Vec<&dyn Projection<Group = Purchases>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<Purchases>(&pool, &refs, purchases::upcasters(), 200)
                .await
                .expect("purchases");

            let owned = tax_sa::projections();
            let refs: Vec<&dyn Projection<Group = TaxSa>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<TaxSa>(&pool, &refs, tax_sa::upcasters(), 200)
                .await
                .expect("tax");
        }
        pool.close().await;
    }

    async fn tenant_pool(&self) -> sqlx::PgPool {
        let url = spa_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(head, _)| head);
        sqlx::PgPool::connect(&format!("{base}/{}", self.tenant_database))
            .await
            .expect("connects")
    }

    /// One invoice, charged at whatever the tenant's rate is.
    async fn sell(&self, id: &str, day: &str, net: Money) {
        sales::issue_invoice(
            &self.db,
            &code(id),
            &Draft {
                customer: Customer::new("Rawabi").with_vat_number("310000000000003"),
                issued_on: on(day),
                due_on: None,
                currency: sar(),
                lines: vec![DraftLine {
                    description: "Consulting".to_owned(),
                    net,
                    category: VatCategory::Standard,
                }],
                note: String::new(),
            },
            &Metadata::default(),
        )
        .await
        .expect("issues");
    }

    /// One bill, at the tax the supplier stated.
    async fn buy(&self, id: &str, day: &str, net: Money, tax: Money) {
        purchases::record_bill(
            &self.db,
            &code(id),
            &purchases::Draft {
                supplier: purchases::Supplier::new("Najd").with_vat_number("311234567800003"),
                supplier_reference: id.to_owned(),
                billed_on: on(day),
                due_on: None,
                currency: sar(),
                lines: vec![purchases::BillLine {
                    description: "Subcontracting".to_owned(),
                    account: code("5000"),
                    net,
                    category: VatCategory::Standard,
                    rate_bp: 1_500,
                    tax,
                }],
                note: String::new(),
            },
            &Metadata::default(),
        )
        .await
        .expect("records");
    }

    async fn declared(&self, from: &str, until: &str) -> tax_sa::Return {
        let mut conn = self.db.acquire().await.expect("connection");
        let declared = tax_sa::vat_return(&mut conn, BOTH, sar(), on(from), on(until))
            .await
            .expect("reads");
        drop(conn);
        declared
    }

    async fn cleanup(self) {
        drop(self.db);
        let _ = spa_testkit::drop_named_database(&self.tenant_database).await;
    }
}

fn rejection(error: &CommandError<TaxError>) -> Option<&TaxError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

/// **The rate arrives with the module.**
///
/// This is what makes a country a module: `ledger` owns that a line has a rate
/// and has no opinion about the number, and enabling `tax_sa` is what supplies
/// 15%.
#[tokio::test]
async fn enabling_the_module_seeds_the_saudi_rate() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let rates = ledger::Rates::resolve(&mut conn).await.expect("reads");
    let stored = spa_eventlog::configuration::get::<ledger::Rates>(&mut conn, ledger::Rates::KEY)
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(rates.standard, 1_500, "15% since July 2020");
    assert!(
        stored.is_some(),
        "the rate is the module's, not a default the ledger fell back to"
    );

    fixture.cleanup().await;
}

/// **A tenant that corrected the rate keeps their correction.**
///
/// Enabling a country module must not stamp over a decision somebody made. The
/// seed is `ON CONFLICT DO NOTHING` and this is why.
#[tokio::test]
async fn re_installing_does_not_overwrite_a_rate_the_tenant_set() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    spa_eventlog::configuration::set(
        &mut conn,
        ledger::Rates::KEY,
        &ledger::Rates { standard: 500 },
        Some("the-accountant"),
    )
    .await
    .expect("sets");

    // As a refresh would.
    tax_sa::install(&mut conn).await.expect("installs again");
    let rates = ledger::Rates::resolve(&mut conn).await.expect("reads");
    drop(conn);

    assert_eq!(rates.standard, 500, "the module overwrote a tenant's rate");

    fixture.cleanup().await;
}

/// The return nets one module's answer against another's.
#[tokio::test]
async fn a_return_nets_what_was_charged_against_what_was_paid() {
    let fixture = Fixture::new().await;

    fixture.sell("crm-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .buy("ap-1", "2026-02-14", riyals(400), riyals(60))
        .await;
    fixture.project().await;

    let q1 = fixture.declared("2026-01-01", "2026-04-01").await;
    assert_eq!(q1.output.tax, riyals(150), "15% of 1,000");
    assert_eq!(q1.input.tax, riyals(60), "what the supplier charged");
    assert_eq!(q1.payable, riyals(90), "150 charged less 60 reclaimed");

    // A period with nothing in it is zero, not an error.
    let q3 = fixture.declared("2026-07-01", "2026-10-01").await;
    assert_eq!(q3.payable, money(0));

    fixture.cleanup().await;
}

/// A quarter that reclaimed more than it charged is money back.
#[tokio::test]
async fn a_return_can_be_negative() {
    let fixture = Fixture::new().await;

    fixture.sell("crm-1", "2026-02-10", riyals(100)).await;
    fixture
        .buy("ap-1", "2026-02-14", riyals(1_000), riyals(150))
        .await;
    fixture.project().await;

    let q1 = fixture.declared("2026-01-01", "2026-04-01").await;
    assert_eq!(
        q1.payable,
        riyals(-135),
        "15 charged less 150 reclaimed — ZATCA owes the business"
    );

    fixture.cleanup().await;
}

/// **What went to ZATCA is recorded, not recomputed.**
#[tokio::test]
async fn a_filed_return_records_what_went() {
    let fixture = Fixture::new().await;

    fixture.sell("crm-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .buy("ap-1", "2026-02-14", riyals(400), riyals(60))
        .await;
    fixture.project().await;

    let filed = tax_sa::file_return(
        &fixture.db,
        BOTH,
        sar(),
        on("2026-01-01"),
        on("2026-04-01"),
        on("2026-04-28"),
        &Metadata::default(),
    )
    .await
    .expect("files");
    assert_eq!(filed.payable, riyals(90));
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let returns = tax_sa::filed(&mut conn, 100).await.expect("reads");
    drop(conn);

    assert_eq!(returns.len(), 1);
    let record = &returns[0];
    assert_eq!(record.period, "SAR.2026-01-01.2026-04-01");
    assert_eq!(record.output_tax, riyals(150));
    assert_eq!(record.input_tax, riyals(60));
    assert_eq!(record.payable, riyals(90));
    assert_eq!(record.filed_on, on("2026-04-28"));
    assert_eq!(
        record.reference, None,
        "nothing acknowledged it; clearance is what fills this in"
    );

    fixture.cleanup().await;
}

/// **Filing twice is a conflict, because the second one is an amendment.**
#[tokio::test]
async fn a_period_is_filed_once() {
    let fixture = Fixture::new().await;
    fixture.sell("crm-1", "2026-02-10", riyals(1_000)).await;
    fixture.project().await;

    let file = async |fixture: &Fixture| {
        tax_sa::file_return(
            &fixture.db,
            BOTH,
            sar(),
            on("2026-01-01"),
            on("2026-04-01"),
            on("2026-04-28"),
            &Metadata::default(),
        )
        .await
    };

    file(&fixture).await.expect("files");
    let refused = file(&fixture).await;
    assert!(
        matches!(
            rejection(&refused.expect_err("is refused")),
            Some(TaxError::AlreadyFiled { .. })
        ),
        "a period was filed twice"
    );

    // A *different* period is fine, which is what makes the period the identity.
    tax_sa::file_return(
        &fixture.db,
        BOTH,
        sar(),
        on("2026-04-01"),
        on("2026-07-01"),
        on("2026-07-28"),
        &Metadata::default(),
    )
    .await
    .expect("files the next quarter");

    fixture.cleanup().await;
}

/// A period that ends before it starts is a mistake worth naming.
#[tokio::test]
async fn a_period_must_end_after_it_starts() {
    let fixture = Fixture::new().await;

    let refused = tax_sa::file_return(
        &fixture.db,
        BOTH,
        sar(),
        on("2026-04-01"),
        on("2026-01-01"),
        on("2026-04-28"),
        &Metadata::default(),
    )
    .await;
    assert!(matches!(
        rejection(&refused.expect_err("is refused")),
        Some(TaxError::EmptyPeriod)
    ));

    fixture.cleanup().await;
}

/// **A filing survives a rebuild, because it is an event and not a derivation.**
///
/// If it were recomputed from `proj_sales` and `proj_purchases`, a rebuild would
/// replace what went to ZATCA with what the system thinks today — which is the
/// one thing a record of a filing must never do.
#[tokio::test]
async fn a_filing_replays_to_exactly_what_it_recorded() {
    let fixture = Fixture::new().await;
    fixture.sell("crm-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .buy("ap-1", "2026-02-14", riyals(400), riyals(60))
        .await;
    fixture.project().await;

    tax_sa::file_return(
        &fixture.db,
        BOTH,
        sar(),
        on("2026-01-01"),
        on("2026-04-01"),
        on("2026-04-28"),
        &Metadata::default(),
    )
    .await
    .expect("files");
    fixture.project().await;

    let pool = fixture.tenant_pool().await;
    let owned = tax_sa::projections();
    let refs: Vec<&dyn Projection<Group = TaxSa>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<TaxSa>(&pool, &refs, tax_sa::upcasters(), 100)
        .await
        .expect("replays");
    pool.close().await;

    assert!(
        report.is_reproducible(),
        "a rebuild changed what was filed: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every event name this module writes is a valid one.
#[test]
fn names_are_valid() {
    for name in tax_sa::FilingEvent::NAMES {
        assert!(
            spa_types::EventName::new(name).is_ok(),
            "{name} is not a usable event name"
        );
    }
}
