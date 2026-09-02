//! Reporting, against a real tenant.
//!
//! The test that carries this file is [`every_figure_agrees_with_the_books`] —
//! Phase 10's exit criterion, and the one that says what this module is for: a
//! figure on a dashboard that does not reconcile is worse than no figure,
//! because somebody acts on it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn sar() -> CurrencyCode {
    CurrencyCode::new("SAR").expect("a real code")
}

fn riyals(major: i64) -> Money {
    Money::from_minor(major * 100, sar())
}

fn on(day: &str) -> Timestamp {
    format!("{day}T09:00:00Z").parse().expect("a valid instant")
}

struct Fixture {
    db: TenantDb,
    pool: sqlx::PgPool,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

impl Fixture {
    async fn new(slug: &str) -> Self {
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
            .register_tenant_on(slug, "Salon", "primary", Actor::system())
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

        {
            let mut conn = db.acquire().await.expect("connection");
            for group in ["crm", "ledger", "sales", "pos", "hr", "payroll", "booking"] {
                install(&mut conn, group).await;
            }
            reports::install(&mut conn).await.expect("reports");
            ensure_group_schema::<reports::Reports>(&mut conn)
                .await
                .expect("reports checkpoint");
        }

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

        ledger::install_chart(
            &fixture.db,
            ledger::chart("services").expect("the services chart ships"),
            sar(),
            erp_i18n::Locale::English,
            &Metadata::default(),
        )
        .await
        .expect("the chart installs");

        fixture
    }

    /// Runs this module's group to the head of the log.
    ///
    /// **Only this one.** Every other group is irrelevant to what is asserted
    /// here, which is the point of the module: nothing it reports depends on
    /// another projection having caught up.
    async fn project(&self) {
        let owned = reports::projections();
        let refs: Vec<&dyn Projection<Group = reports::Reports>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<reports::Reports>(&self.pool, &refs, reports::upcasters(), 200)
            .await
            .expect("reports projects");
    }

    async fn revenue(&self) -> Vec<reports::RevenueRow> {
        let mut conn = self.db.acquire().await.expect("connection");
        reports::revenue(&mut conn, "2000-01", "2099-12")
            .await
            .expect("reads")
    }

    async fn utilisation(&self) -> Vec<reports::UtilisationRow> {
        let mut conn = self.db.acquire().await.expect("connection");
        reports::utilisation(&mut conn, "2000-01", "2099-12")
            .await
            .expect("reads")
    }

    async fn takings(&self) -> Vec<reports::TakingsRow> {
        let mut conn = self.db.acquire().await.expect("connection");
        reports::takings(&mut conn, "2000-01", "2099-12")
            .await
            .expect("reads")
    }

    async fn people_cost(&self) -> Vec<reports::PeopleCostRow> {
        let mut conn = self.db.acquire().await.expect("connection");
        reports::people_cost(&mut conn, "2000-01", "2099-12")
            .await
            .expect("reads")
    }

    async fn discrepancies(&self) -> Vec<reports::Discrepancy> {
        let mut conn = self.db.acquire().await.expect("connection");
        reports::reconciles(&mut conn).await.expect("reads")
    }

    /// Issues an invoice for one line, at the standard rate.
    async fn invoice(&self, id: &str, net: Money, day: &str) {
        sales::issue_invoice(
            &self.db,
            &code(id),
            &sales::Draft {
                customer: sales::Customer {
                    id: None,
                    name: "زبون".to_owned(),
                    vat_number: None,
                    address: None,
                },
                issued_on: on(day),
                due_on: None,
                currency: sar(),
                lines: vec![sales::DraftLine {
                    description: "قص".to_owned(),
                    net,
                    category: ledger::VatCategory::Standard,
                }],
                discounts: Vec::new(),
                note: String::new(),
            },
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{id} is issued: {e:?}"));
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// Installs one module's read models and its checkpoint.
///
/// A `match` rather than a registry, because a test crate cannot depend on
/// `erp-api` without the dependency arrow pointing back at the composition
/// root.
async fn install(conn: &mut sqlx::PgConnection, module: &str) {
    match module {
        "crm" => {
            crm::install(&mut *conn).await.expect("crm");
            ensure_group_schema::<crm::Crm>(&mut *conn)
                .await
                .expect("c");
        }
        "ledger" => {
            ledger::install(&mut *conn).await.expect("ledger");
            ensure_group_schema::<ledger::Ledger>(&mut *conn)
                .await
                .expect("l");
        }
        "sales" => {
            sales::install(&mut *conn).await.expect("sales");
            ensure_group_schema::<sales::Sales>(&mut *conn)
                .await
                .expect("s");
        }
        "pos" => {
            pos::install(&mut *conn).await.expect("pos");
            ensure_group_schema::<pos::Pos>(&mut *conn)
                .await
                .expect("p");
        }
        "hr" => {
            hr::install(&mut *conn).await.expect("hr");
            ensure_group_schema::<hr::Hr>(&mut *conn).await.expect("h");
        }
        "payroll" => {
            payroll::install(&mut *conn).await.expect("payroll");
            ensure_group_schema::<payroll::Payroll>(&mut *conn)
                .await
                .expect("y");
        }
        "booking" => {
            booking::install(&mut *conn).await.expect("booking");
            ensure_group_schema::<booking::Booking>(&mut *conn)
                .await
                .expect("b");
        }
        other => panic!("{other} has no install in this fixture"),
    }
}

// ---------------------------------------------------------------------------

/// **Phase 10's exit criterion.**
///
/// A business sells, credits one of its sales, rings up a till and pays its
/// people — and every figure this module reports agrees with the ledger.
///
/// The reconciliation is not a summary of what the other assertions checked.
/// It is the invariant: what this module says each document came to equals the
/// debits of the journal entry that document posted, and every currency's
/// postings still sum to zero — both against this group's own copy of the
/// books, at its own checkpoint.
#[tokio::test]
async fn every_figure_agrees_with_the_books() {
    let fixture = Fixture::new("agree").await;

    fixture.invoice("INV-1", riyals(1_000), "2026-01-05").await;
    fixture.invoice("INV-2", riyals(400), "2026-01-20").await;
    fixture.invoice("INV-3", riyals(250), "2026-02-03").await;

    sales::cancel_invoice(
        &fixture.db,
        &code("INV-2"),
        "CN-1",
        "خطأ في الفاتورة",
        on("2026-02-10"),
        &Metadata::default(),
    )
    .await
    .expect("credits");

    fixture.project().await;

    assert!(
        fixture.discrepancies().await.is_empty(),
        "the figures do not agree with the books: {:?}",
        fixture.discrepancies().await
    );

    // And the revenue reads the way the accounting does. January kept both
    // documents; February took one back out and added its own.
    let rows = fixture.revenue().await;
    let january = rows
        .iter()
        .find(|r| r.period == "2026-01")
        .expect("january");
    assert_eq!(january.net, riyals(1_400), "january sold 1,400");
    assert_eq!(january.documents, 2);
    assert_eq!(january.credited, 0, "the credit is february's");

    let february = rows
        .iter()
        .find(|r| r.period == "2026-02")
        .expect("february");
    assert_eq!(
        february.net,
        riyals(250 - 400),
        "february issued 250 and took back 400"
    );
    assert_eq!(february.documents, 1);
    assert_eq!(february.credited, 1);

    fixture.cleanup().await;
}

/// **The falsification.** The invariant has to be able to fail.
///
/// A test that only ever sees zero discrepancies proves the query returns an
/// empty list, not that it can find anything. So a row is corrupted the way a
/// double-applied event would corrupt it, and the check must say so.
#[tokio::test]
async fn a_figure_that_disagrees_with_the_books_is_a_failure() {
    let fixture = Fixture::new("disagree").await;

    fixture.invoice("INV-1", riyals(1_000), "2026-01-05").await;
    fixture.project().await;
    assert!(fixture.discrepancies().await.is_empty(), "clean to start");

    // What applying the issue twice would leave behind: the report says the
    // document came to more than the entry it posted.
    sqlx::query("UPDATE proj_reports.invoiced SET net = net * 2")
        .execute(&fixture.pool)
        .await
        .expect("tampers");

    let found = fixture.discrepancies().await;
    assert_eq!(found.len(), 1, "expected one discrepancy, got {found:?}");
    assert!(
        matches!(found[0], reports::Discrepancy::Mismatched { .. }),
        "expected a mismatch, got {found:?}"
    );

    // And the other half: a posting that went missing.
    sqlx::query("UPDATE proj_reports.invoiced SET net = net / 2")
        .execute(&fixture.pool)
        .await
        .expect("restores");
    sqlx::query("DELETE FROM proj_reports.entry")
        .execute(&fixture.pool)
        .await
        .expect("tampers");

    let found = fixture.discrepancies().await;
    assert!(
        matches!(found[0], reports::Discrepancy::Unposted { .. }),
        "expected an unposted document, got {found:?}"
    );

    fixture.cleanup().await;
}

/// A booking walked through its stages is one completion, not three.
///
/// `reserved → confirmed → completed` is the ordinary path, and a report that
/// counted every move would say a salon did three times the work it did.
#[tokio::test]
async fn a_booking_that_walks_its_stages_completes_once() {
    let fixture = Fixture::new("diary").await;

    booking::declare_resource(
        &fixture.db,
        &code("CHAIR-1"),
        &booking::Details {
            name: "كرسي ١".to_owned(),
            name_latin: None,
            kind: booking::Kind::Place,
            capacity: 1,
            branch: None,
            employee: None,
        },
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("declares");

    let span = erp_occupancy::Span::new(
        on("2026-03-10"),
        on("2026-03-10") + chrono::Duration::hours(1),
    )
    .expect("a valid span");

    booking::reserve(
        &fixture.db,
        &code("BK-1"),
        &booking::Draft {
            customer: booking::Customer {
                id: None,
                name: "نورة".to_owned(),
                phone: None,
            },
            lines: vec![booking::DraftLine {
                what: "قص".to_owned(),
                span,
                takes: vec![booking::Held::one(code("CHAIR-1"))],
                charge: None,
            }],
            note: String::new(),
            at: on("2026-03-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("reserves");

    for stage in [booking::Stage::Confirmed, booking::Stage::Arrived] {
        booking::move_to(
            &fixture.db,
            &code("BK-1"),
            stage,
            "",
            on("2026-03-10"),
            &Metadata::default(),
        )
        .await
        .expect("moves");
    }
    booking::move_to(
        &fixture.db,
        &code("BK-1"),
        booking::Stage::InService,
        "",
        on("2026-03-10"),
        &Metadata::default(),
    )
    .await
    .expect("moves");
    booking::move_to(
        &fixture.db,
        &code("BK-1"),
        booking::Stage::Completed,
        "",
        on("2026-03-10"),
        &Metadata::default(),
    )
    .await
    .expect("completes");

    fixture.project().await;

    let rows = fixture.utilisation().await;
    assert_eq!(rows.len(), 1, "one resource, one month: {rows:?}");
    assert_eq!(rows[0].booked, 1);
    assert_eq!(rows[0].completed, 1, "three stage moves are one completion");
    assert_eq!(rows[0].no_shows, 0);
    assert_eq!(rows[0].minutes, 60, "an hour of diary time");
    // Booked on the 1st for the 10th, so nine days of notice.
    assert_eq!(rows[0].average_lead_minutes(), 9 * 24 * 60);
    assert_eq!(rows[0].no_show_rate_bp(), 0);

    fixture.cleanup().await;
}

/// Takings are attributed to whoever had the till open.
///
/// The operator is on `pos.shift.opened` and on nothing after it, so this is
/// the module remembering rather than asking `proj_pos` — which is the
/// cross-group read L3 forbids.
#[tokio::test]
async fn takings_are_attributed_to_whoever_had_the_till_open() {
    let fixture = Fixture::new("counter").await;

    pos::open_shift(
        &fixture.db,
        &code("SH-1"),
        &pos::Opening {
            till: "الاستقبال".to_owned(),
            operator: "layla".to_owned(),
            float: riyals(200),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("opens");

    pos::sell(
        &fixture.db,
        &code("SH-1"),
        &code("TILL-1"),
        &pos::Basket {
            customer: sales::Customer {
                id: None,
                name: "زبون".to_owned(),
                vat_number: None,
                address: None,
            },
            lines: vec![sales::DraftLine {
                description: "قص".to_owned(),
                net: riyals(100),
                category: ledger::VatCategory::Standard,
            }],
            discounts: Vec::new(),
            currency: sar(),
            tenders: vec![pos::Tender::new(pos::Method::Cash, riyals(115))],
            note: String::new(),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("sells");

    pos::pay_out(
        &fixture.db,
        &code("SH-1"),
        &pos::PayOut {
            reference: "bank-1".to_owned(),
            amount: riyals(50),
            to: code("1010"),
            why: "إيداع بنكي".to_owned(),
            at: on("2026-04-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("pays out");

    // 200 float + 115 taken − 50 banked = 265, and the drawer is 5 short.
    pos::close_shift(
        &fixture.db,
        &code("SH-1"),
        riyals(260),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("closes");

    fixture.project().await;

    let rows = fixture.takings().await;
    assert_eq!(rows.len(), 1, "one operator, one method: {rows:?}");
    assert_eq!(rows[0].operator, "layla");
    assert_eq!(rows[0].method, "cash");
    assert_eq!(rows[0].taken, riyals(115));
    assert_eq!(
        rows[0].paid_out,
        riyals(50),
        "a banking run is not a refund"
    );
    assert_eq!(rows[0].variance, riyals(-5), "the drawer was five short");
    assert_eq!(rows[0].shifts, 1);

    assert!(
        fixture.discrepancies().await.is_empty(),
        "a till sale is an invoice, and it has to reconcile like one"
    );

    fixture.cleanup().await;
}

/// **A draft is not a cost.**
///
/// Payroll drafts, redrafts and only then approves. The wage bill must not move
/// until it is approved, and redrafting must replace rather than accumulate —
/// otherwise a business that corrected two payslips would report paying twice.
#[tokio::test]
async fn only_an_approved_payroll_run_is_a_cost() {
    let fixture = Fixture::new("wages").await;

    hr::hire(
        &fixture.db,
        &code("EMP-1"),
        &hr::Hire {
            details: hr::Details {
                name: "سارة".to_owned(),
                name_latin: None,
                national_id: None,
                email: None,
                phone: Some("+966500000000".to_owned()),
            },
            reports_to: None,
            branch: None,
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("hires");

    hr::record_salary(
        &fixture.db,
        &code("EMP-1"),
        &hr::Salary {
            basic: riyals(8_000),
            allowances: Vec::new(),
            deductions: Vec::new(),
            commission_bp: 0,
        },
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("salary");

    let period = payroll::Period::parse("2026-05").expect("a month");
    let run = code("RUN-1");

    payroll::draft_run(
        &fixture.db,
        &run,
        period,
        &[code("EMP-1")],
        &[],
        on("2026-05-31"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");

    fixture.project().await;
    assert!(
        fixture.people_cost().await.is_empty(),
        "a draft is not a cost"
    );

    // Redrafted, because somebody found a mistake. Still not a cost, and still
    // one run rather than two.
    payroll::draft_run(
        &fixture.db,
        &run,
        period,
        &[code("EMP-1")],
        &[],
        on("2026-05-31"),
        &Metadata::default(),
    )
    .await
    .expect("redrafts");

    payroll::approve_run(&fixture.db, &run, &Metadata::default())
        .await
        .expect("approves");

    fixture.project().await;

    let cost = fixture.people_cost().await;
    assert_eq!(cost.len(), 1, "one month: {cost:?}");
    assert_eq!(cost[0].period, "2026-05");
    assert_eq!(
        cost[0].gross,
        riyals(8_000),
        "redrafting replaced, not added"
    );
    assert_eq!(cost[0].people, 1);

    fixture.cleanup().await;
}

/// **L3, as a fact about this crate's source.**
///
/// The whole architectural point of this module is that it subscribes to the
/// log instead of reading other groups. A single `proj_sales.` in here would
/// undo it silently — the query would work, and the answer would be a total
/// across two checkpoints that was never true at any moment.
#[test]
fn this_module_names_no_other_projection_group() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for entry in std::fs::read_dir(&root).expect("the source directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable");
        for (number, line) in text.lines().enumerate() {
            // Prose may name them; only code may not.
            let statement = line.split("//").next().unwrap_or(line);
            let Some(at) = statement.find("proj_") else {
                continue;
            };
            if statement[at..].starts_with("proj_reports") {
                continue;
            }
            offenders.push(format!(
                "{}:{}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                number + 1
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "this module reads another projection group, which L3 forbids: {offenders:?}"
    );
}
