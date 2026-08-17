//! The Saudi tax module, against a real tenant with everything under it.
//!
//! The test that carries this module is [`a_filed_return_records_what_went`]:
//! every other guarantee in the system makes re-running a period give the number
//! that was filed, and those are properties of the arithmetic. This one is a
//! record, and it survives a rebuild because it is an event.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;

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
                discounts: Vec::new(),
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
                discounts: Vec::new(),
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
        let all = tax_sa::documents(&mut conn, 100, None)
            .await
            .expect("reads")
            .items;
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
    // **An `<Invoice>` with 381 in the type code.** ZATCA's schema is UBL's
    // Invoice schema; its `CreditNote` document type is rejected by the gateway
    // before validation begins.
    assert!(xml.starts_with("<Invoice"), "{}", &xml[..40]);
    assert!(xml.contains("<cbc:InvoiceTypeCode name=\"0100000\">381</cbc:InvoiceTypeCode>"));
    assert!(xml.contains("<cac:BillingReference>"));
    assert!(
        xml.contains("<cbc:ID>INV-00001</cbc:ID>"),
        "it must name what it credits"
    );
    // The reason, in KSA-10 — where `BR-KSA-17` reads it.
    assert!(
        xml.contains("<cbc:InstructionNote>the wrong service</cbc:InstructionNote>"),
        "ZATCA requires the reason in PaymentMeans/InstructionNote"
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
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("inv-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("inv-2", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;
    // Nothing reaches ZATCA unsigned, so the queue is empty until this runs.
    tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
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
            .push((endpoint, submission.invoice.clone()));
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
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("b2b", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("b2c", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;
    tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
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
    // And what was sent is the signed document that was stored.
    let sent = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(&seen[0].1)
            .expect("base64"),
    )
    .expect("utf-8");
    assert!(
        sent.ends_with(
            &fixture
                .zatca("INV-00001")
                .await
                .signed_xml
                .unwrap_or_default()
        )
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
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    for n in 1..=3 {
        fixture
            .sell(&format!("inv-{n}"), "2026-02-10", riyals(100))
            .await;
    }
    fixture.project().await;
    tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-11"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
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

// ---------------------------------------------------------------------------
// Onboarding
// ---------------------------------------------------------------------------

use tax_sa::zatca::csr::{Environment, Issues, Unit};
use tax_sa::zatca::onboarding::{
    ComplianceRequest, Csid, CsidResponse, Onboarder, Otp, ProductionRequest, Registrar, Stage,
};

fn unit() -> Unit {
    Unit {
        vat_number: "310122393500003".to_owned(),
        organization: "روابي للاستشارات".to_owned(),
        branch: "الفرع الرئيسي".to_owned(),
        common_name: "EGS1-886431145".to_owned(),
        solution: "Spa".to_owned(),
        version: "1.0".to_owned(),
        serial: "886431145".to_owned(),
        address: "الرياض 12211".to_owned(),
        industry: "Consulting".to_owned(),
        issues: Issues::both(),
    }
}

fn otp() -> Otp {
    "123456".parse().expect("six digits")
}

/// A ZATCA that issues certificates, so onboarding can be driven without one.
///
/// It signs the CSR it is given with its own CA key — which is what makes the
/// test meaningful: the certificate that comes back really is over the public
/// key in the request, so [`Onboarder`]'s check that the certificate matches
/// the tenant's private key is exercised rather than trusted.
#[derive(Debug)]
struct FakeZatcaCa {
    /// What the taxpayer typed, as it arrived. Onboarding must send it.
    seen_otp: std::sync::Mutex<Vec<String>>,
    /// The CSRs it was asked to sign.
    seen_csr: std::sync::Mutex<Vec<String>>,
    /// Set to sign with a *different* key, which is the mismatch that must be
    /// caught before anything is stored.
    substitute_key: bool,
    /// Set to answer without issuing.
    refuse: bool,
    /// The compliance documents it was shown.
    checked: std::sync::Mutex<Vec<tax_sa::zatca::wire::Submission>>,
    /// Set to refuse every compliance document.
    refuse_checks: bool,
}

impl FakeZatcaCa {
    fn new() -> Self {
        Self {
            seen_otp: std::sync::Mutex::new(Vec::new()),
            seen_csr: std::sync::Mutex::new(Vec::new()),
            substitute_key: false,
            refuse: false,
            checked: std::sync::Mutex::new(Vec::new()),
            refuse_checks: false,
        }
    }

    fn otps(&self) -> Vec<String> {
        self.seen_otp.lock().expect("not poisoned").clone()
    }

    fn csrs(&self) -> Vec<String> {
        self.seen_csr.lock().expect("not poisoned").clone()
    }

    /// Signs the request the way ZATCA would: a certificate over the public key
    /// in the CSR, from a self-signed CA.
    fn issue(&self, csr_base64: &str, request_id: &str) -> CsidResponse {
        let engine = base64::engine::general_purpose::STANDARD;

        self.seen_csr
            .lock()
            .expect("not poisoned")
            .push(csr_base64.to_owned());

        if self.refuse {
            return CsidResponse {
                disposition: Some("REJECTED".to_owned()),
                errors: Some(serde_json::json!(["the OTP has expired"])),
                ..CsidResponse::default()
            };
        }

        let pem = engine.decode(csr_base64).expect("the CSR is base64");
        let request = openssl::x509::X509Req::from_pem(&pem).expect("a CSR");

        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP256K1).expect("secp256k1");
        let ca = openssl::ec::EcKey::generate(&group).expect("a CA key");
        let ca = openssl::pkey::PKey::from_ec_key(ca).expect("a CA key");

        // Either the key in the request, or somebody else's. Both are
        // rebuilt from PEM so the two branches have the same type — the public
        // half is all a certificate carries either way.
        let subject_key = if self.substitute_key {
            let other = openssl::ec::EcKey::generate(&group).expect("another key");
            openssl::pkey::PKey::from_ec_key(other)
                .expect("another key")
                .public_key_to_pem()
                .expect("pem")
        } else {
            request
                .public_key()
                .expect("the CSR's key")
                .public_key_to_pem()
                .expect("pem")
        };
        let subject_key = openssl::pkey::PKey::public_key_from_pem(&subject_key).expect("a key");

        let mut certificate = openssl::x509::X509::builder().expect("a builder");
        certificate.set_version(2).expect("v3");
        certificate
            .set_subject_name(request.subject_name())
            .expect("subject");
        certificate
            .set_issuer_name(request.subject_name())
            .expect("issuer");
        certificate.set_pubkey(&subject_key).expect("public key");
        certificate
            .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).expect("now"))
            .expect("not before");
        certificate
            .set_not_after(&openssl::asn1::Asn1Time::days_from_now(1826).expect("five years"))
            .expect("not after");
        let serial = openssl::bn::BigNum::from_u32(0x0BAD_CAFE)
            .and_then(|bn| openssl::asn1::Asn1Integer::from_bn(&bn))
            .expect("a serial");
        certificate.set_serial_number(&serial).expect("serial");
        certificate
            .sign(&ca, openssl::hash::MessageDigest::sha256())
            .expect("signs");
        let certificate = certificate.build().to_pem().expect("pem");

        CsidResponse {
            request_id: Some(serde_json::json!(request_id)),
            disposition: Some("ISSUED".to_owned()),
            // Base64 of the PEM, which is one of the two shapes ZATCA uses.
            token: Some(engine.encode(&certificate)),
            secret: Some("the-csid-secret".to_owned()),
            errors: None,
        }
    }
}

#[async_trait::async_trait]
impl Registrar for FakeZatcaCa {
    async fn compliance_csid(
        &self,
        _environment: Environment,
        otp: &Otp,
        request: &ComplianceRequest,
    ) -> Result<CsidResponse, tax_sa::zatca::wire::Unanswered> {
        self.seen_otp
            .lock()
            .expect("not poisoned")
            .push(otp.header().to_owned());
        Ok(self.issue(&request.csr, "compliance-1"))
    }

    async fn check_compliance(
        &self,
        _environment: Environment,
        compliance: &Csid,
        submission: &tax_sa::zatca::wire::Submission,
    ) -> Result<tax_sa::zatca::wire::Verdict, tax_sa::zatca::wire::Unanswered> {
        // Every check authenticates as the compliance certificate, and carries
        // a signed document.
        assert!(compliance.authorization().starts_with("Basic "));
        self.checked
            .lock()
            .expect("not poisoned")
            .push(submission.clone());
        if self.refuse_checks {
            return Ok(tax_sa::zatca::wire::Verdict::Refused {
                errors: vec![tax_sa::zatca::wire::Remark {
                    code: "BR-KSA-99".to_owned(),
                    category: "ERROR".to_owned(),
                    message: "the sample is not acceptable".to_owned(),
                }],
            });
        }
        Ok(tax_sa::zatca::wire::Verdict::Accepted {
            warnings: vec![],
            stamped: None,
        })
    }

    async fn production_csid(
        &self,
        _environment: Environment,
        compliance: &Csid,
        request: &ProductionRequest,
    ) -> Result<CsidResponse, tax_sa::zatca::wire::Unanswered> {
        // The production call must quote the compliance request's id, and
        // authenticate as the compliance certificate.
        assert_eq!(request.compliance_request_id, compliance.request_id);
        assert!(compliance.authorization().starts_with("Basic "));
        Ok(self.issue(
            &self.csrs().last().cloned().unwrap_or_default(),
            "production-1",
        ))
    }

    async fn renew_csid(
        &self,
        _environment: Environment,
        _production: &Csid,
        otp: &Otp,
        request: &ComplianceRequest,
    ) -> Result<CsidResponse, tax_sa::zatca::wire::Unanswered> {
        self.seen_otp
            .lock()
            .expect("not poisoned")
            .push(otp.header().to_owned());
        Ok(self.issue(&request.csr, "renewal-1"))
    }
}

fn sealing() -> spa_eventlog::SealingKey {
    spa_eventlog::SealingKey::new("test", &[3u8; 32]).expect("32 bytes")
}

/// **The whole onboarding.** An OTP goes in, two certificates come back, and
/// what is stored is sealed.
#[tokio::test]
async fn an_otp_becomes_a_compliance_certificate_and_then_a_production_one() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    // Nothing yet.
    assert!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads")
            .is_empty()
    );

    let compliance = onboarder
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");

    assert_eq!(compliance.stage, Stage::Compliance);
    assert_eq!(compliance.environment, Environment::Simulation);
    assert_eq!(compliance.request_id, "compliance-1");
    assert!(
        compliance.subject.contains("CN=EGS1-886431145"),
        "{compliance:?}"
    );
    assert!(compliance.serial.contains("BADCAFE"), "{compliance:?}");

    // **The OTP reached ZATCA**, which is the only thing it is for.
    assert_eq!(zatca.otps(), vec!["123456".to_owned()]);

    // Only compliance so far — going live is a separate call for a reason.
    assert_eq!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads"),
        vec![Stage::Compliance]
    );
    assert!(
        tax_sa::zatca::onboarding::production(&fixture.db, &sealing)
            .await
            .expect("reads")
            .is_none()
    );

    let production = onboarder
        .go_live(
            Environment::Simulation,
            on("2026-01-02"),
            &Metadata::default(),
        )
        .await
        .expect("goes live");
    assert_eq!(production.stage, Stage::Production);
    assert_eq!(production.request_id, "production-1");

    let credentials = tax_sa::zatca::onboarding::production(&fixture.db, &sealing)
        .await
        .expect("reads")
        .expect("production credentials");
    assert_eq!(credentials.secret, "the-csid-secret");
    assert!(credentials.authorization().starts_with("Basic "));
    assert!(
        credentials.certificate().is_ok(),
        "the token is a certificate"
    );

    fixture.cleanup().await;
}

/// **The private key is sealed, and the plaintext is nowhere in the database.**
#[tokio::test]
async fn the_private_key_is_sealed_and_never_stored_in_the_clear() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();

    Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Sandbox,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");

    // The key is readable with the sealing key...
    let key = tax_sa::zatca::onboarding::private_key(&fixture.db, &sealing)
        .await
        .expect("reads")
        .expect("a key");
    assert!(String::from_utf8_lossy(&key).contains("BEGIN EC PRIVATE KEY"));

    // ...and the row itself contains none of it.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let rows: Vec<(String, Vec<u8>, String)> =
        sqlx::query_as("SELECT key, sealed, sealed_with FROM module_secret ORDER BY key")
            .fetch_all(&mut *conn)
            .await
            .expect("reads");
    drop(conn);

    assert_eq!(rows.len(), 2, "the key and the compliance credentials");
    for (name, sealed, sealed_with) in &rows {
        assert_eq!(sealed_with, "test");
        let text = String::from_utf8_lossy(sealed);
        assert!(
            !text.contains("BEGIN EC PRIVATE KEY"),
            "{name} is in the clear"
        );
        assert!(!text.contains("the-csid-secret"), "{name} is in the clear");
    }

    // And another sealing key gets nothing.
    let other = spa_eventlog::SealingKey::new("other", &[9u8; 32]).expect("32 bytes");
    assert!(
        tax_sa::zatca::onboarding::private_key(&fixture.db, &other)
            .await
            .is_err(),
        "another key unsealed it"
    );

    fixture.cleanup().await;
}

/// **A certificate for somebody else's key is refused before it is stored.**
///
/// Every signature made with it would be rejected at clearance, on an invoice a
/// customer is waiting for, with an error that says nothing about why.
#[tokio::test]
async fn a_certificate_that_is_not_for_our_key_is_refused() {
    let fixture = Fixture::new().await;
    let mut zatca = FakeZatcaCa::new();
    zatca.substitute_key = true;
    let sealing = sealing();

    let refused = Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Production,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect_err("a certificate for another key is refused");
    assert!(
        matches!(
            refused,
            tax_sa::zatca::onboarding::OnboardError::KeyMismatch
        ),
        "{refused:?}"
    );

    // Nothing was stored, so the tenant is not half onboarded.
    assert!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads")
            .is_empty()
    );

    fixture.cleanup().await;
}

/// A refusal from ZATCA is an error with the reason in it, and stores nothing.
#[tokio::test]
async fn a_refused_request_is_reported_with_what_zatca_said() {
    let fixture = Fixture::new().await;
    let mut zatca = FakeZatcaCa::new();
    zatca.refuse = true;
    let sealing = sealing();

    let refused = Onboarder::new(&fixture.db, &sealing, &zatca)
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect_err("a rejection is not a certificate");

    let tax_sa::zatca::onboarding::OnboardError::NotIssued {
        disposition,
        detail,
    } = refused
    else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(disposition, "REJECTED");
    assert!(detail.contains("expired"), "{detail}");

    assert!(
        tax_sa::zatca::onboarding::reached(&fixture.db)
            .await
            .expect("reads")
            .is_empty(),
        "a refusal left credentials behind"
    );

    fixture.cleanup().await;
}

/// Going live before there is a compliance certificate is refused here rather
/// than sent to ZATCA to be refused there.
#[tokio::test]
async fn going_live_without_a_compliance_certificate_is_refused() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();

    let refused = Onboarder::new(&fixture.db, &sealing, &zatca)
        .go_live(
            Environment::Simulation,
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect_err("there is nothing to go live with");
    assert!(
        matches!(
            refused,
            tax_sa::zatca::onboarding::OnboardError::NotYet("compliance")
        ),
        "{refused:?}"
    );
    assert!(zatca.csrs().is_empty(), "it asked ZATCA anyway");

    fixture.cleanup().await;
}

/// **A renewal keeps the key.** A new one would mean two keys to hold and two
/// certificates to reconcile.
#[tokio::test]
async fn a_renewal_reuses_the_key_and_replaces_the_certificate() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    onboarder
        .onboard(
            &unit(),
            Environment::Production,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");
    onboarder
        .go_live(
            Environment::Production,
            on("2026-01-02"),
            &Metadata::default(),
        )
        .await
        .expect("goes live");

    let key_before = tax_sa::zatca::onboarding::private_key(&fixture.db, &sealing)
        .await
        .expect("reads");
    let before = tax_sa::zatca::onboarding::production(&fixture.db, &sealing)
        .await
        .expect("reads")
        .expect("credentials");

    let renewed = onboarder
        .renew(
            &unit(),
            Environment::Production,
            &"654321".parse().expect("six digits"),
            on("2030-12-01"),
            &Metadata::default(),
        )
        .await
        .expect("renews");
    assert_eq!(renewed.stage, Stage::Production);
    assert_eq!(renewed.request_id, "renewal-1");

    let key_after = tax_sa::zatca::onboarding::private_key(&fixture.db, &sealing)
        .await
        .expect("reads");
    assert_eq!(key_before, key_after, "the renewal changed the key");

    let after = tax_sa::zatca::onboarding::production(&fixture.db, &sealing)
        .await
        .expect("reads")
        .expect("credentials");
    assert_ne!(
        before.request_id, after.request_id,
        "the certificate is the old one"
    );

    // The second OTP reached ZATCA too.
    assert_eq!(zatca.otps(), vec!["123456".to_owned(), "654321".to_owned()]);

    fixture.cleanup().await;
}

/// The log records which certificate is in force, and no secret at all.
#[tokio::test]
async fn the_log_records_the_certificate_and_never_the_key() {
    let fixture = Fixture::new().await;
    let zatca = FakeZatcaCa::new();
    let sealing = sealing();
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    onboarder
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");
    onboarder
        .go_live(
            Environment::Simulation,
            on("2026-01-02"),
            &Metadata::default(),
        )
        .await
        .expect("goes live");

    let mut conn = fixture.db.acquire().await.expect("connection");
    let loaded = spa_eventlog::load::<tax_sa::Onboarding>(
        &mut conn,
        &tax_sa::onboarding_id(),
        tax_sa::upcasters(),
    )
    .await
    .expect("loads");

    let events: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT event_name, payload FROM event
          WHERE event_name = 'tax_sa.zatca.csid_issued' ORDER BY position",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("reads");
    drop(conn);

    assert_eq!(loaded.aggregate.stage, Some(Stage::Production));
    assert_eq!(events.len(), 2, "one per certificate");

    for (_, payload) in &events {
        let text = payload.to_string();
        assert!(text.contains("EGS1-886431145"), "the subject is useful");
        assert!(!text.contains("BEGIN EC PRIVATE KEY"), "{text}");
        assert!(!text.contains("the-csid-secret"), "{text}");
        assert!(!text.contains("123456"), "the OTP is in the log: {text}");
    }

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

impl Fixture {
    /// Takes this tenant all the way to a production certificate.
    async fn go_live(&self, sealing: &spa_eventlog::SealingKey) {
        let zatca = FakeZatcaCa::new();
        let onboarder = Onboarder::new(&self.db, sealing, &zatca);
        onboarder
            .onboard(
                &unit(),
                Environment::Simulation,
                &otp(),
                on("2026-01-01"),
                &Metadata::default(),
            )
            .await
            .expect("onboards");
        onboarder
            .go_live(
                Environment::Simulation,
                on("2026-01-02"),
                &Metadata::default(),
            )
            .await
            .expect("goes live");
    }
}

/// **The whole path.** An invoice is issued, built, signed with the tenant's
/// certificate, and the signature verifies under it.
#[tokio::test]
async fn an_invoice_is_signed_with_the_certificate_zatca_issued() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("inv-1", "2026-02-10", riyals(1_000)).await;
    fixture
        .sell_to_a_consumer("inv-2", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;

    // Built but not signed: it can be neither submitted nor printed.
    let standing = fixture.standing("2026-02-12").await;
    assert_eq!(standing.unsigned, 2);
    {
        let mut conn = fixture.db.acquire().await.expect("connection");
        assert!(
            tax_sa::pending(&mut conn, 10)
                .await
                .expect("reads")
                .is_empty(),
            "an unsigned document must not be submitted"
        );
        drop(conn);
    }

    let signed = tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
    assert_eq!(signed.signed, 2);
    assert_eq!(signed.waiting_for_a_certificate, 0);
    fixture.project().await;

    let document = fixture.zatca("INV-00001").await;
    let signature = document.signature.clone().expect("a signature");
    let submitted = document.signed_xml.clone().expect("the submitted document");

    // The submitted document is the hashed one plus the three things ZATCA
    // removes before it re-derives the hash.
    let hashed = document.xml.clone().expect("the canonical bytes");
    assert!(submitted.contains("<ext:UBLExtensions>"));
    assert!(submitted.contains("<cac:Signature>"));
    assert!(submitted.contains("<cbc:ID>QR</cbc:ID>"));
    assert!(!hashed.contains("<ext:UBLExtensions>"));
    assert_eq!(
        document.invoice_hash.as_deref(),
        Some(tax_sa::zatca::chain::invoice_hash(&hashed).as_str()),
        "the hash is no longer the hash of the bytes it was taken over"
    );

    // **The signature verifies under the certificate the tenant holds.**
    let credentials = tax_sa::zatca::onboarding::production(&fixture.db, &sealing)
        .await
        .expect("reads")
        .expect("credentials");
    let certificate = credentials.certificate().expect("a certificate");
    let public = certificate.public_key().expect("a key");

    let properties = tax_sa::zatca::signing::signed_properties(
        &base64::engine::general_purpose::STANDARD.encode(certificate.to_der().expect("der")),
        &tax_sa::zatca::signing::issuer_name(&certificate),
        &tax_sa::zatca::signing::serial_number(&certificate),
        on("2026-02-12"),
    );
    let signed_info = tax_sa::zatca::signing::signed_info(
        document.invoice_hash.as_deref().unwrap_or_default(),
        &tax_sa::zatca::signing::digest(&properties),
    );

    let mut verifier =
        openssl::sign::Verifier::new(openssl::hash::MessageDigest::sha256(), &public)
            .expect("a verifier");
    verifier.update(signed_info.as_bytes()).expect("update");
    assert!(
        verifier
            .verify(
                &base64::engine::general_purpose::STANDARD
                    .decode(&signature)
                    .expect("base64")
            )
            .expect("verifies"),
        "the signature does not verify under the tenant's own certificate"
    );

    // **The QR gained the stamp**, which is what a customer's phone checks.
    let fields =
        tax_sa::zatca::qr::decode(document.qr.as_deref().unwrap_or_default()).expect("a QR");
    assert_eq!(fields.len(), 9, "a signed invoice carries all nine tags");

    fixture.cleanup().await;
}

/// A tenant that has not finished onboarding signs nothing, and that is a
/// normal state rather than an error.
#[tokio::test]
async fn without_a_certificate_nothing_is_signed_and_nothing_breaks() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    fixture.register().await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    let signed = tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");

    assert_eq!(signed.signed, 0);
    assert_eq!(signed.waiting_for_a_certificate, 1);
    assert_eq!(fixture.standing("2026-02-12").await.unsigned, 1);
    assert!(fixture.zatca("INV-00001").await.signature.is_none());

    fixture.cleanup().await;
}

/// Signing twice would be a second signature over one invoice, and ZATCA holds
/// the first.
#[tokio::test]
async fn a_document_is_signed_once() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    let first = tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
    fixture.project().await;
    let signature = fixture.zatca("INV-00001").await.signature;

    // Nothing is unsigned any more, so a second sweep finds nothing at all.
    let again = tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-13"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");
    fixture.project().await;

    assert_eq!(first.signed, 1);
    assert_eq!(again.signed, 0);
    assert_eq!(
        fixture.zatca("INV-00001").await.signature,
        signature,
        "the signature changed under a document ZATCA may already hold"
    );

    fixture.cleanup().await;
}

/// **A rebuild reproduces the signed document**, because the signature is
/// replayed from the log rather than recomputed — which it could not be.
#[tokio::test]
async fn a_rebuild_reproduces_the_signature_it_cannot_recompute() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture
        .sell_to_a_consumer("inv-2", "2026-02-11", riyals(20))
        .await;
    fixture.project().await;
    tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
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
        "a rebuild changed a signature: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Signed, then submitted: the two sweeps in the order a worker runs them.
#[tokio::test]
async fn only_a_signed_document_reaches_zatca() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    fixture.register().await;
    fixture.go_live(&sealing).await;
    fixture.sell("inv-1", "2026-02-10", riyals(100)).await;
    fixture.project().await;

    // Submitting before signing sends nothing.
    let zatca = FakeZatca::saying(vec![Ok(tax_sa::zatca::wire::Verdict::Accepted {
        warnings: vec![],
        stamped: None,
    })]);
    let swept = tax_sa::submit_pending(
        &fixture.db,
        &zatca,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("sweeps");
    assert_eq!(swept.accepted, 0, "an unsigned document was submitted");
    assert!(zatca.seen().is_empty());

    // Sign, then submit.
    tax_sa::sign_pending(
        &fixture.db,
        &sealing,
        on("2026-02-12"),
        10,
        &Metadata::default(),
    )
    .await
    .expect("signs");
    fixture.project().await;

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

    assert_eq!(swept.accepted, 1);
    assert_eq!(
        fixture.zatca("INV-00001").await.status,
        tax_sa::Status::Cleared
    );

    // **What went to ZATCA was the signed document.**
    let sent = zatca.seen();
    assert_eq!(sent.len(), 1);
    let submitted = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(&sent[0].1)
            .expect("base64"),
    )
    .expect("utf-8");
    assert!(submitted.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(submitted.contains("<ds:SignatureValue>"));
    assert!(submitted.contains("<cbc:ID>QR</cbc:ID>"));

    fixture.cleanup().await;
}

/// **Step 3, end to end.** The solution proves it can produce every document
/// type it declared, and only then can it go live.
#[tokio::test]
async fn the_compliance_checks_submit_one_of_every_declared_document() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    let zatca = FakeZatcaCa::new();
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    onboarder
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");

    let checks = onboarder
        .pass_compliance_checks(
            &registration(),
            &unit(),
            Environment::Simulation,
            on("2026-01-01"),
        )
        .await
        .expect("runs the checks");

    assert_eq!(checks.submitted, 6, "both kinds, three documents each");
    assert_eq!(checks.passed, 6);
    assert!(checks.failures.is_empty());
    assert!(checks.all_passed());

    // **Every one was signed and chained**, which is what ZATCA is checking.
    let seen = zatca.checked.lock().expect("not poisoned").clone();
    assert_eq!(seen.len(), 6);

    let mut previous: Option<String> = None;
    for submission in &seen {
        let xml = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(&submission.invoice)
                .expect("base64"),
        )
        .expect("utf-8");

        assert!(xml.contains("<ds:SignatureValue>"), "an unsigned sample");
        assert!(xml.contains("<cbc:ID>QR</cbc:ID>"), "a sample with no QR");
        // The hash is over the document without those, which is what the
        // submitted hash has to be.
        let canonical = xml
            .split_once('\n')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default();
        assert!(!canonical.is_empty());

        // Each points at the one before it.
        let pih = xml
            .split("<cbc:ID>PIH</cbc:ID>")
            .nth(1)
            .and_then(|rest| rest.split("mimeCode=\"text/plain\">").nth(1))
            .and_then(|rest| rest.split('<').next())
            .unwrap_or_default()
            .to_owned();
        match &previous {
            None => assert_eq!(pih, tax_sa::zatca::chain::genesis(), "the first sample"),
            Some(hash) => assert_eq!(&pih, hash, "the chain is broken"),
        }
        previous = Some(submission.invoice_hash.clone());
    }

    // And the samples are obviously not sales.
    assert!(
        seen.iter()
            .all(|s| !s.uuid.is_empty() && s.invoice_hash.len() == 44)
    );

    fixture.cleanup().await;
}

/// The checks report what ZATCA refused, per document — the whole point of
/// running them before a real invoice exists.
#[tokio::test]
async fn a_failed_compliance_check_names_the_document_and_the_reason() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    let mut zatca = FakeZatcaCa::new();
    zatca.refuse_checks = true;
    let onboarder = Onboarder::new(&fixture.db, &sealing, &zatca);

    onboarder
        .onboard(
            &unit(),
            Environment::Simulation,
            &otp(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("onboards");

    let checks = onboarder
        .pass_compliance_checks(
            &registration(),
            &unit(),
            Environment::Simulation,
            on("2026-01-01"),
        )
        .await
        .expect("runs the checks");

    assert_eq!(checks.submitted, 6);
    assert_eq!(checks.passed, 0);
    assert!(!checks.all_passed());
    assert_eq!(checks.failures.len(), 6);
    let (document, errors) = &checks.failures[0];
    assert!(document.starts_with("COMPLIANCE-"), "{document}");
    assert_eq!(errors[0].code, "BR-KSA-99");

    fixture.cleanup().await;
}

/// Running the checks before there is a compliance certificate is refused here
/// rather than sent to ZATCA to be refused there.
#[tokio::test]
async fn the_compliance_checks_need_a_compliance_certificate_first() {
    let fixture = Fixture::new().await;
    let sealing = sealing();
    let zatca = FakeZatcaCa::new();

    let refused = Onboarder::new(&fixture.db, &sealing, &zatca)
        .pass_compliance_checks(
            &registration(),
            &unit(),
            Environment::Simulation,
            on("2026-01-01"),
        )
        .await
        .expect_err("there is nothing to sign the samples with");
    assert!(
        matches!(
            refused,
            tax_sa::zatca::onboarding::OnboardError::NotYet("compliance")
        ),
        "{refused:?}"
    );
    assert!(zatca.checked.lock().expect("not poisoned").is_empty());

    fixture.cleanup().await;
}
