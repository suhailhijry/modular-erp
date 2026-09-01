//! **What a busy tenant costs.**
//!
//! The sizing in `ARCHITECTURE.md` was written against a tenant issuing a couple
//! of hundred invoices a month. A tenant issuing **600 a day** is ninety times
//! that, and it changes which constraint binds: at a few hundred a month the
//! limit on tenants per instance is the catalog, and at 600 a day it is disk and
//! the read models.
//!
//! So this measures a tenant, end to end and through the real command path,
//! rather than extrapolating from a schema:
//!
//! - **bytes on disk per invoice**, broken down by what is holding them,
//! - **the issue rate**, which is bounded by the gapless numbering lock,
//! - **the read models at volume**, which is where a missing index shows up,
//! - **the rebuild**, because D17 leans on "projections are disposable" and that
//!   is only true while rebuilding one is affordable.
//!
//! Storage and rebuild are linear in invoice count and are safe to extrapolate
//! from the slope. Query latency is the one that could be nonlinear, so it is
//! sampled at the volume actually built rather than projected from a smaller
//! one.
//!
//! ```text
//! HV_INVOICES=20000 cargo test -p tax_sa --test high_volume -- --ignored --nocapture
//! ```

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    // `float_arithmetic` is denied workspace-wide because **money is integer
    // minor units, always**. Nothing here is money: these are rates, ratios and
    // megabytes, and computing a throughput in fixed point would be arithmetic
    // nobody could read to make a lint happy about a rule it is not breaking.
    clippy::float_arithmetic
)]

use std::sync::Arc;
use std::time::Instant;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use ledger::{AccountKind, Ledger, VatCategory, open_account};
use purchases::Purchases;
use sales::{Customer, Draft, DraftLine, Sales};
use tax_sa::TaxSa;

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn env(name: &str, fallback: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("valid")
}
fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}
fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

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

struct Busy {
    db: TenantDb,
    pool: sqlx::PgPool,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

impl Busy {
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
            .register_tenant_on("busy", "Busy", "primary", Actor::system())
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
        ledger::install(&mut conn).await.expect("ledger");
        ensure_group_schema::<Ledger>(&mut conn).await.expect("cp");
        sales::install(&mut conn).await.expect("sales");
        ensure_group_schema::<Sales>(&mut conn).await.expect("cp");
        purchases::install(&mut conn).await.expect("purchases");
        ensure_group_schema::<Purchases>(&mut conn)
            .await
            .expect("cp");
        tax_sa::install(&mut conn).await.expect("tax_sa");
        ensure_group_schema::<TaxSa>(&mut conn).await.expect("cp");
        drop(conn);

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        let me = Self {
            db,
            pool,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
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
                &me.db,
                &code(account),
                account,
                kind,
                sar(),
                &Metadata::default(),
            )
            .await
            .expect("opens");
        }

        // **Registered**, so the ZATCA projection does its real work. An
        // unregistered tenant's documents come out `Unregistered` and skip the
        // UBL render, the hash and the chain, which is most of the cost being
        // measured.
        tax_sa::register_taxpayer(
            &me.db,
            registration(),
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("registers");

        me
    }

    /// Half to businesses (standard, cleared) and half to consumers
    /// (simplified, reported), because the two take different ZATCA paths and a
    /// tenant at this volume issues both.
    async fn issue(&self, i: i64) {
        let business = i % 2 == 0;
        let mut customer = Customer::new(format!("عميل {}", i % 500));
        if business {
            customer = customer.with_vat_number("399999999900003");
        }
        // Spread across a year so the period reports have something to filter.
        let day = 1 + (i % 28);
        let month = 1 + (i % 12);
        sales::issue_invoice(
            &self.db,
            &code(&format!("INV-{i:07}")),
            &Draft {
                customer,
                issued_on: on(&format!("2026-{month:02}-{day:02}")),
                due_on: Some(on(&format!("2026-{month:02}-{day:02}"))),
                currency: sar(),
                lines: vec![
                    DraftLine {
                        description: "استشارات".to_owned(),
                        net: Money::from_minor(25_000 + (i % 997) * 13, sar()),
                        category: VatCategory::Standard,
                    },
                    DraftLine {
                        description: "مصاريف".to_owned(),
                        net: Money::from_minor(4_500, sar()),
                        category: VatCategory::Standard,
                    },
                ],
                discounts: Vec::new(),
                note: String::new(),
            },
            &Metadata::default(),
        )
        .await
        .expect("issues");
    }

    async fn project(&self, batch: i64) {
        let l = ledger::projections();
        let r: Vec<&dyn Projection<Group = Ledger>> = l.iter().map(AsRef::as_ref).collect();
        run_to_head::<Ledger>(&self.pool, &r, ledger::upcasters(), batch)
            .await
            .expect("ledger");
        let s = sales::projections();
        let r: Vec<&dyn Projection<Group = Sales>> = s.iter().map(AsRef::as_ref).collect();
        run_to_head::<Sales>(&self.pool, &r, sales::upcasters(), batch)
            .await
            .expect("sales");
        let t = tax_sa::projections();
        let r: Vec<&dyn Projection<Group = TaxSa>> = t.iter().map(AsRef::as_ref).collect();
        run_to_head::<TaxSa>(&self.pool, &r, tax_sa::upcasters(), batch)
            .await
            .expect("tax_sa");
    }

    async fn size_mb(&self) -> f64 {
        let bytes: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
            .fetch_one(&self.pool)
            .await
            .expect("size");
        bytes as f64 / 1_048_576.0
    }

    async fn biggest(&self) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT n.nspname || '.' || c.relname, pg_total_relation_size(c.oid)
               FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')
                AND c.relkind = 'r'
              ORDER BY pg_total_relation_size(c.oid) DESC LIMIT 8",
        )
        .fetch_all(&self.pool)
        .await
        .expect("relations")
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

async fn timed<F, T>(label: &str, f: F) -> (T, f64)
where
    F: std::future::Future<Output = T>,
{
    let t = Instant::now();
    let out = f.await;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("  {label:<34} {ms:>9.1} ms");
    (out, ms)
}

/// A tenant at 600 invoices a day, measured rather than assumed.
#[tokio::test]
#[ignore = "benchmark; run with --ignored"]
async fn what_a_busy_tenant_costs() {
    let n = env("HV_INVOICES", 5_000);
    let batch = env("HV_BATCH", 500);
    let per_day = env("HV_PER_DAY", 600);

    let busy = Busy::new().await;
    let baseline = busy.size_mb().await;

    let t = Instant::now();
    for i in 0..n {
        busy.issue(i).await;
    }
    let issuing = t.elapsed().as_secs_f64();

    let t = Instant::now();
    busy.project(batch).await;
    let projecting = t.elapsed().as_secs_f64();

    let total = busy.size_mb().await;
    let grown = total - baseline;

    let invoices: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_sales.invoice")
        .fetch_one(&busy.pool)
        .await
        .expect("count");
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(&busy.pool)
        .await
        .expect("count");
    let docs: i64 = sqlx::query_scalar("SELECT count(*) FROM proj_tax_sa.zatca_document")
        .fetch_one(&busy.pool)
        .await
        .expect("count");
    assert_eq!(invoices, n, "every invoice must have projected");
    assert_eq!(docs, n, "every invoice must have become a ZATCA document");

    println!("\n=== a tenant at {per_day} invoices a day ===\n");
    println!("built            : {n} invoices, {events} events, {docs} ZATCA documents");
    println!(
        "issuing          : {:.0} invoices/sec  ({:.1} ms each, serialised by the numbering lock)",
        n as f64 / issuing,
        issuing * 1000.0 / n as f64
    );
    println!(
        "projecting       : {:.0} events/sec  ({:.1} s for {events})",
        events as f64 / projecting,
        projecting
    );

    let per_invoice_kb = grown * 1024.0 / n as f64;
    println!("\n--- storage ---");
    println!("empty tenant     : {baseline:.1} MB");
    println!("after {n:>6}     : {total:.1} MB   (+{grown:.1} MB)");
    println!("per invoice      : {per_invoice_kb:.1} KB");
    for (rel, bytes) in busy.biggest().await {
        println!("  {rel:<34} {:>8.1} MB", bytes as f64 / 1_048_576.0);
    }

    println!("\n--- the read models at this volume ---");
    let mut conn = busy.pool.acquire().await.expect("connection");
    timed("invoice list, first page", async {
        sales::invoices(&mut conn, 50, None)
            .await
            .expect("invoices")
    })
    .await;
    timed("receivables, aged", async {
        sales::receivables(&mut conn, on("2026-12-31"), 50, None)
            .await
            .expect("receivables")
    })
    .await;
    timed("VAT return, one quarter", async {
        sales::vat_return(&mut conn, sar(), on("2026-01-01"), on("2026-04-01"))
            .await
            .expect("return")
    })
    .await;
    timed("zatca documents, first page", async {
        tax_sa::documents(&mut conn, 50, None)
            .await
            .expect("documents")
    })
    .await;
    timed("zatca standing", async {
        tax_sa::standing(&mut conn, on("2026-12-31"))
            .await
            .expect("standing")
    })
    .await;
    drop(conn);

    // What five years at this rate comes to, from the measured slope. Storage
    // and rebuild are linear in invoice count; the latencies above are not
    // extrapolated, which is why they are measured at volume.
    let five_years = per_day * 365 * 5;
    let gb = per_invoice_kb * five_years as f64 / 1_048_576.0;
    let rebuild_s =
        (events as f64 / projecting).recip() * (events as f64 / n as f64) * five_years as f64;
    println!("\n--- extrapolated to five years at {per_day}/day ---");
    println!("invoices         : {five_years}");
    println!("storage          : {gb:.1} GB per tenant");
    println!("rebuild          : {:.0} min", rebuild_s / 60.0);
    println!(
        "tenants per 4 TB : {:.0}  (disk, against ~6000 by catalog)",
        4096.0 / gb
    );
    println!();

    busy.cleanup().await;
}
