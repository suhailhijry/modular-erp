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

// ---------------------------------------------------------------------------
// ZATCA
// ---------------------------------------------------------------------------

fn registration() -> tax_sa::Registration {
    tax_sa::Registration {
        vat_number: "310122393500003".to_owned(),
        name: "روابي للاستشارات".to_owned(),
        name_latin: Some("Rawabi Consulting".to_owned()),
        scheme: tax_sa::taxpayer::IdScheme::Crn,
        identifier: "1010101010".to_owned(),
        address: tax_sa::taxpayer::Address {
            street: "طريق الملك فهد".to_owned(),
            building: "2322".to_owned(),
            additional: Some("9999".to_owned()),
            district: "العليا".to_owned(),
            city: "الرياض".to_owned(),
            postal_code: "12211".to_owned(),
            country: "SA".to_owned(),
        },
    }
}

impl Fixture {
    async fn register(&self) {
        tax_sa::register_taxpayer(
            &self.db,
            registration(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("registers");
    }

    /// One invoice to a consumer — no VAT number, so a simplified one.
    async fn sell_to_a_consumer(&self, id: &str, day: &str, net: Money) {
        sales::issue_invoice(
            &self.db,
            &code(id),
            &Draft {
                customer: Customer::new("زبون"),
                issued_on: on(day),
                due_on: None,
                currency: sar(),
                lines: vec![DraftLine {
                    description: "قهوة".to_owned(),
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

    async fn zatca(&self, number: &str) -> tax_sa::Stored {
        let mut conn = self.db.acquire().await.expect("connection");
        let found = tax_sa::document(&mut conn, number)
            .await
            .expect("reads")
            .expect("a ZATCA document with that number");
        drop(conn);
        found
    }

    async fn standing(&self, at: &str) -> tax_sa::Standing {
        let mut conn = self.db.acquire().await.expect("connection");
        let found = tax_sa::standing(&mut conn, on(at)).await.expect("reads");
        drop(conn);
        found
    }
}

/// **The decision this module exists to take.** A buyer with a VAT number gets a
/// standard invoice that must be cleared; everyone else gets a simplified one
/// that must be reported within 24 hours.
#[tokio::test]
async fn who_the_buyer_is_decides_which_document_zatca_gets() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("b2b-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("b2c-1", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;

    let business = fixture.zatca("INV-00001").await;
    assert_eq!(business.kind, tax_sa::zatca::Kind::Standard);
    assert_eq!(business.type_code, 388);
    assert_eq!(
        tax_sa::zatca::wire::Endpoint::of(business.kind),
        tax_sa::zatca::wire::Endpoint::Clearance,
        "a standard invoice is cleared, not reported"
    );

    let consumer = fixture.zatca("INV-00002").await;
    assert_eq!(consumer.kind, tax_sa::zatca::Kind::Simplified);
    assert_eq!(
        tax_sa::zatca::wire::Endpoint::of(consumer.kind),
        tax_sa::zatca::wire::Endpoint::Reporting
    );

    // And the XML says which it is, since that is what ZATCA reads.
    assert!(
        business
            .xml
            .as_deref()
            .unwrap_or_default()
            .contains("name=\"0100000\""),
        "the standard invoice is not marked standard"
    );
    assert!(
        consumer
            .xml
            .as_deref()
            .unwrap_or_default()
            .contains("name=\"0200000\"")
    );

    fixture.cleanup().await;
}

/// **The chain.** Each document points at the one before it, the counter never
/// resets, and the first points at ZATCA's genesis value.
#[tokio::test]
async fn every_document_links_to_the_one_before_it() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    for (n, day) in [(1, "2026-02-10"), (2, "2026-02-11"), (3, "2026-02-12")] {
        fixture
            .sell(&format!("inv-{n}"), day, riyals(100 * n))
            .await;
    }
    fixture.project().await;

    let first = fixture.zatca("INV-00001").await;
    assert_eq!(first.icv, Some(1));
    assert_eq!(
        first.previous_hash.as_deref(),
        Some(tax_sa::zatca::chain::genesis().as_str()),
        "the first document does not point at ZATCA's genesis value"
    );

    let mut previous = first;
    for number in ["INV-00002", "INV-00003"] {
        let next = fixture.zatca(number).await;
        assert_eq!(next.icv, previous.icv.map(|i| i + 1));
        assert_eq!(
            next.previous_hash, previous.invoice_hash,
            "{number} does not point at the document before it"
        );
        previous = next;
    }

    // And the hash is the hash of exactly the bytes that were stored.
    let stored = fixture.zatca("INV-00002").await;
    assert_eq!(
        stored.invoice_hash,
        stored
            .xml
            .as_deref()
            .map(tax_sa::zatca::chain::invoice_hash)
    );

    fixture.cleanup().await;
}

/// **The reason the registration is an event.** A rebuild must reproduce every
/// document byte for byte, or the hashes stop matching what ZATCA holds.
#[tokio::test]
async fn a_rebuild_reproduces_every_document_exactly() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("inv-2", "2026-02-11", riyals(20))
        .await;
    sales::cancel_invoice(
        &fixture.db,
        &code("inv-1"),
        "returned",
        "wrong address",
        on("2026-02-20"),
        &Metadata::default(),
    )
    .await
    .expect("cancels");
    fixture.project().await;

    let before: Vec<_> = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        let all = tax_sa::documents(&mut conn, 100).await.expect("reads");
        drop(conn);
        all
    };
    assert!(
        before.len() >= 3,
        "expected an invoice, a receipt and a credit note"
    );

    let pool = fixture.tenant_pool().await;
    let owned = tax_sa::projections();
    let refs: Vec<&dyn Projection<Group = TaxSa>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<TaxSa>(&pool, &refs, tax_sa::upcasters(), 100)
        .await
        .expect("replays");
    pool.close().await;

    assert!(
        report.is_reproducible(),
        "a rebuild changed a ZATCA document: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// A credit note is a document in its own right: its own number, its own place
/// in the chain, and a reference to what it credits.
#[tokio::test]
async fn a_credit_note_is_its_own_document_pointing_at_the_invoice() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(1_000)).await;
    fixture.project().await;

    sales::cancel_invoice(
        &fixture.db,
        &code("inv-1"),
        "returned",
        "the wrong service",
        on("2026-02-20"),
        &Metadata::default(),
    )
    .await
    .expect("cancels");
    fixture.project().await;

    let invoice = fixture.zatca("INV-00001").await;
    let note = fixture.zatca("CN-00001").await;

    assert_eq!(note.type_code, 381, "a credit note is 381, not 388");
    assert_eq!(note.icv, invoice.icv.map(|i| i + 1));
    assert_eq!(note.previous_hash, invoice.invoice_hash);

    let xml = note.xml.unwrap_or_default();
    assert!(
        xml.starts_with("<CreditNote"),
        "a credit note is its own UBL document"
    );
    assert!(xml.contains("<cac:BillingReference>"));
    assert!(
        xml.contains("<cbc:ID>INV-00001</cbc:ID>"),
        "it must name what it credits"
    );
    assert!(
        xml.contains("the wrong service"),
        "ZATCA requires the reason"
    );

    fixture.cleanup().await;
}

/// Invoices issued before the business registered cannot be cleared
/// retrospectively — the chain starts at onboarding. They are recorded, not
/// skipped, because a business needs to know they exist.
#[tokio::test]
async fn invoices_issued_before_registration_are_recorded_and_not_chained() {
    let fixture = Fixture::new().await;
    fixture.sell("early", "2026-01-05", riyals(100)).await;
    fixture.project().await;

    let early = fixture.zatca("INV-00001").await;
    assert_eq!(early.status, tax_sa::Status::Unregistered);
    assert_eq!(early.icv, None);
    assert_eq!(early.xml, None);

    // Registering does not retrospectively build it, and the chain still starts
    // at one for the first document issued after registration.
    fixture.register().await;
    fixture.sell("later", "2026-02-05", riyals(100)).await;
    fixture.project().await;

    assert_eq!(
        fixture.zatca("INV-00001").await.status,
        tax_sa::Status::Unregistered
    );
    let later = fixture.zatca("INV-00002").await;
    assert_eq!(later.status, tax_sa::Status::Pending);
    assert_eq!(later.icv, Some(1));

    let standing = fixture.standing("2026-03-01").await;
    assert!(standing.registered);
    assert_eq!(
        standing.counts,
        vec![
            (tax_sa::Status::Pending, 1),
            (tax_sa::Status::Unregistered, 1)
        ]
    );

    fixture.cleanup().await;
}

/// **The 24-hour clock.** A simplified invoice not reported within a day is the
/// number an inspection asks about, and it is answered here rather than counted
/// by hand.
#[tokio::test]
async fn a_simplified_invoice_goes_overdue_after_a_day() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture
        .sell_to_a_consumer("till-1", "2026-02-10", riyals(20))
        .await;
    fixture.sell("b2b-1", "2026-02-10", riyals(500)).await;
    fixture.project().await;

    // Within the window: nothing is late, and the standard invoice is waiting
    // for clearance rather than running out of time.
    let same_day = fixture.standing("2026-02-10").await;
    assert_eq!(same_day.overdue, 0);
    assert_eq!(same_day.awaiting_clearance, 1);
    assert_eq!(same_day.chain_length, 2);

    // A day later it is.
    let next_day = fixture.standing("2026-02-12").await;
    assert_eq!(next_day.overdue, 1, "the till receipt is past its 24 hours");
    assert_eq!(next_day.awaiting_clearance, 1);
    assert_eq!(next_day.oldest_pending, Some(on("2026-02-10")));

    fixture.cleanup().await;
}

/// What ZATCA said is recorded as an event, so it survives a rebuild — the one
/// part of this that cannot be derived from anything else.
#[tokio::test]
async fn a_verdict_is_recorded_and_survives_a_rebuild() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("inv-2", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;

    // What a submitter would take from the queue.
    let queued = {
        let mut conn = fixture.db.acquire().await.expect("connection");
        let found = tax_sa::pending(&mut conn, 10).await.expect("reads");
        drop(conn);
        found
    };
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].number, "INV-00001", "oldest first");
    assert!(!queued[0].xml.is_empty() && !queued[0].invoice_hash.is_empty());

    // ZATCA clears the standard one and reports the simplified one.
    tax_sa::record_outcome(
        &fixture.db,
        "INV-00001",
        tax_sa::zatca::Kind::Standard,
        &tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: Some("PHN0YW1wZWQ+".to_owned()),
        },
        on("2026-02-10"),
        &Metadata::default(),
    )
    .await
    .expect("records");

    tax_sa::record_outcome(
        &fixture.db,
        "INV-00002",
        tax_sa::zatca::Kind::Simplified,
        &tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![tax_sa::zatca::wire::Remark {
                code: "BR-KSA-09".to_owned(),
                category: "WARNING".to_owned(),
                message: "check the address".to_owned(),
            }],
            stamped: None,
        },
        on("2026-02-11"),
        &Metadata::default(),
    )
    .await
    .expect("records");
    fixture.project().await;

    let cleared = fixture.zatca("INV-00001").await;
    assert_eq!(cleared.status, tax_sa::Status::Cleared);
    assert_eq!(cleared.stamped_xml.as_deref(), Some("PHN0YW1wZWQ+"));

    let reported = fixture.zatca("INV-00002").await;
    assert_eq!(reported.status, tax_sa::Status::Reported);
    assert_eq!(
        reported.remarks.len(),
        1,
        "an accepted document keeps its warnings"
    );
    assert_eq!(reported.remarks[0].code, "BR-KSA-09");

    // Nothing is left in the queue, and the standing says so.
    let standing = fixture.standing("2026-03-01").await;
    assert_eq!(standing.overdue, 0);
    assert_eq!(standing.awaiting_clearance, 0);

    let pool = fixture.tenant_pool().await;
    let owned = tax_sa::projections();
    let refs: Vec<&dyn Projection<Group = TaxSa>> = owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<TaxSa>(&pool, &refs, tax_sa::upcasters(), 100)
        .await
        .expect("replays");
    pool.close().await;
    assert!(
        report.is_reproducible(),
        "a rebuild lost what ZATCA said: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Recording the same verdict twice writes nothing, which is what makes a
/// submitter that crashed between the call and the append safe to re-run.
#[tokio::test]
async fn recording_the_same_verdict_twice_writes_nothing() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    let verdict = tax_sa::zatca::wire::Verdict::Accepted {
        warnings: vec![],
        stamped: None,
    };
    let first = tax_sa::record_outcome(
        &fixture.db,
        "INV-00001",
        tax_sa::zatca::Kind::Standard,
        &verdict,
        on("2026-02-10"),
        &Metadata::default(),
    )
    .await
    .expect("records");
    let again = tax_sa::record_outcome(
        &fixture.db,
        "INV-00001",
        tax_sa::zatca::Kind::Standard,
        &verdict,
        on("2026-02-10"),
        &Metadata::default(),
    )
    .await
    .expect("records");

    assert!(first.at.is_some(), "the first one appended");
    assert!(again.at.is_none(), "the second one appended nothing");

    fixture.cleanup().await;
}

/// A refusal is about the document and is final. It is not a failure to reach
/// ZATCA, which is not recorded at all.
#[tokio::test]
async fn a_refusal_is_recorded_with_what_was_wrong() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    tax_sa::record_outcome(
        &fixture.db,
        "INV-00001",
        tax_sa::zatca::Kind::Standard,
        &tax_sa::zatca::wire::Verdict::Refused {
            errors: vec![tax_sa::zatca::wire::Remark {
                code: "BR-KSA-40".to_owned(),
                category: "ERROR".to_owned(),
                message: "invalid buyer VAT number".to_owned(),
            }],
        },
        on("2026-02-10"),
        &Metadata::default(),
    )
    .await
    .expect("records");
    fixture.project().await;

    let refused = fixture.zatca("INV-00001").await;
    assert_eq!(refused.status, tax_sa::Status::Refused);
    assert_eq!(refused.remarks[0].code, "BR-KSA-40");
    assert!(
        !refused.status.is_settled(),
        "a refused document is not done"
    );

    fixture.cleanup().await;
}

/// A registration ZATCA would refuse is refused here, because by the time ZATCA
/// says so the invoice exists and the sale has stalled.
#[tokio::test]
async fn a_registration_zatca_would_refuse_is_refused_here() {
    let fixture = Fixture::new().await;

    let mut bad = registration();
    bad.vat_number = "123456789012345".to_owned();
    let refused =
        tax_sa::register_taxpayer(&fixture.db, bad, on("2026-01-01"), &Metadata::default())
            .await
            .expect_err("a VAT number that is not one is refused");
    assert!(matches!(
        rejection(&refused),
        Some(TaxError::Registration(_))
    ));

    // And nothing was recorded, so nothing renders.
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;
    assert_eq!(
        fixture.zatca("INV-00001").await.status,
        tax_sa::Status::Unregistered
    );

    fixture.cleanup().await;
}

/// Registering the same details twice appends nothing; correcting them appends,
/// and only documents issued after the correction carry it.
#[tokio::test]
async fn a_correction_applies_from_where_it_was_made() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    let repeat = tax_sa::register_taxpayer(
        &fixture.db,
        registration(),
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("accepts");
    assert!(
        repeat.at.is_none(),
        "registering identical details is a no-op"
    );

    let mut moved = registration();
    moved.address.building = "1111".to_owned();
    tax_sa::register_taxpayer(&fixture.db, moved, on("2026-03-01"), &Metadata::default())
        .await
        .expect("corrects");
    fixture.sell("inv-2", "2026-03-10", riyals(100)).await;
    fixture.project().await;

    let before = fixture.zatca("INV-00001").await.xml.unwrap_or_default();
    let after = fixture.zatca("INV-00002").await.xml.unwrap_or_default();
    assert!(before.contains("<cbc:BuildingNumber>2322</cbc:BuildingNumber>"));
    assert!(
        after.contains("<cbc:BuildingNumber>1111</cbc:BuildingNumber>"),
        "the correction did not reach the document issued after it"
    );

    fixture.cleanup().await;
}

/// A ZATCA that answers from a script, so the sweep can be driven without one.
#[derive(Debug)]
struct FakeZatca {
    answers: std::sync::Mutex<
        Vec<Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered>>,
    >,
    seen: std::sync::Mutex<Vec<(tax_sa::zatca::wire::Endpoint, String)>>,
}

impl FakeZatca {
    fn saying(
        answers: Vec<Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered>>,
    ) -> Self {
        Self {
            answers: std::sync::Mutex::new(answers),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<(tax_sa::zatca::wire::Endpoint, String)> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

#[async_trait::async_trait]
impl tax_sa::zatca::wire::Submitter for FakeZatca {
    async fn submit(
        &self,
        endpoint: tax_sa::zatca::wire::Endpoint,
        submission: &tax_sa::zatca::wire::Submission,
    ) -> Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered> {
        self.seen
            .lock()
            .expect("not poisoned")
            .push((endpoint, submission.invoice_hash.clone()));
        let mut answers = self.answers.lock().expect("not poisoned");
        if answers.is_empty() {
            return Err(tax_sa::zatca::wire::Unanswered::Unavailable(
                "the script ran out".to_owned(),
            ));
        }
        answers.remove(0)
    }
}

/// **The sweep, end to end.** Each document goes to the endpoint its kind
/// decides, and what comes back is recorded against it.
#[tokio::test]
async fn a_sweep_sends_each_document_to_the_call_its_kind_decides() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    fixture.sell("b2b", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("b2c", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;

    let zatca = FakeZatca::saying(vec![
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: Some("PHN0YW1wZWQ+".to_owned()),
        }),
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        }),
    ]);

    let swept = tax_sa::submit_pending(
        &fixture.db,
        &zatca,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");
    fixture.project().await;

    assert_eq!(swept.accepted, 2);
    assert_eq!(swept.refused, 0);
    assert!(swept.stopped.is_none());

    // The standard one was cleared and the simplified one reported, at the two
    // different endpoints.
    let seen = zatca.seen();
    assert_eq!(seen[0].0, tax_sa::zatca::wire::Endpoint::Clearance);
    assert_eq!(seen[1].0, tax_sa::zatca::wire::Endpoint::Reporting);
    // And what was sent is the hash of what was stored.
    assert_eq!(
        seen[0].1,
        fixture
            .zatca("INV-00001")
            .await
            .invoice_hash
            .unwrap_or_default()
    );

    assert_eq!(
        fixture.zatca("INV-00001").await.status,
        tax_sa::Status::Cleared
    );
    assert_eq!(
        fixture.zatca("INV-00002").await.status,
        tax_sa::Status::Reported
    );

    fixture.cleanup().await;
}

/// **The rule the sweep exists to get right.** An expired certificate is not a
/// verdict on anybody's invoice, and nothing may be marked refused by an outage.
#[tokio::test]
async fn an_outage_marks_nothing_refused_and_stops_the_sweep() {
    let fixture = Fixture::new().await;
    fixture.register().await;
    for n in 1..=3 {
        fixture
            .sell(&format!("inv-{n}"), "2026-02-10", riyals(100))
            .await;
    }
    fixture.project().await;

    let zatca = FakeZatca::saying(vec![
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        }),
        Err(tax_sa::zatca::wire::Unanswered::NotOnboarded { status: 401 }),
        // Never reached: the sweep stops at the one above.
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        }),
    ]);

    let swept = tax_sa::submit_pending(
        &fixture.db,
        &zatca,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");
    fixture.project().await;

    assert_eq!(
        swept.accepted, 1,
        "the one that answered before the failure"
    );
    assert_eq!(swept.refused, 0, "an outage refuses nothing");
    assert!(matches!(
        swept.stopped,
        Some(tax_sa::zatca::wire::Unanswered::NotOnboarded { status: 401 })
    ));
    assert_eq!(
        zatca.seen().len(),
        2,
        "it stopped rather than working through the batch"
    );

    assert_eq!(
        fixture.zatca("INV-00001").await.status,
        tax_sa::Status::Cleared
    );
    // **The two that were not decided are still pending**, not refused.
    for number in ["INV-00002", "INV-00003"] {
        assert_eq!(
            fixture.zatca(number).await.status,
            tax_sa::Status::Pending,
            "{number} was marked by an outage rather than by ZATCA"
        );
    }

    // And the next sweep picks them up where this one stopped.
    let zatca = FakeZatca::saying(vec![
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        }),
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        }),
    ]);
    let swept = tax_sa::submit_pending(
        &fixture.db,
        &zatca,
        on("2026-02-13"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");
    fixture.project().await;
    assert_eq!(swept.accepted, 2);
    assert_eq!(
        fixture.standing("2026-02-14").await.awaiting_clearance,
        0,
        "nothing is left waiting"
    );

    fixture.cleanup().await;
}
