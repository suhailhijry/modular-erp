//! Payroll, against a real tenant.
//!
//! The test that carries this file is [`a_payroll_run_posts_and_the_books_balance`]
//! — half of Phase 9's exit criterion, and the one that says what a run
//! actually is: a journal entry, not a spreadsheet.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use payroll::{Period, approve_run, draft_run};

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

/// Whole riyals, so the tests read in the units a person uses.
fn riyals(major: i64) -> Money {
    money(major * 100)
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
            .register_tenant_on("salon", "Salon", "primary", Actor::system())
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
            branches::install(&mut conn).await.expect("branches");
            ensure_group_schema::<branches::Branches>(&mut conn)
                .await
                .expect("branches checkpoint");
            ledger::install(&mut conn).await.expect("ledger");
            ensure_group_schema::<ledger::Ledger>(&mut conn)
                .await
                .expect("ledger checkpoint");
            hr::install(&mut conn).await.expect("hr");
            ensure_group_schema::<hr::Hr>(&mut conn)
                .await
                .expect("hr checkpoint");
            payroll::install(&mut conn).await.expect("payroll");
            ensure_group_schema::<payroll::Payroll>(&mut conn)
                .await
                .expect("payroll checkpoint");
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

    /// Somebody on the books, with a salary.
    async fn employ(&self, id: &str, name: &str, basic: Money, deduction: Option<Money>) {
        hr::hire(
            &self.db,
            &code(id),
            &hr::Hire {
                details: hr::Details {
                    name: name.to_owned(),
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
        .unwrap_or_else(|e| panic!("{id} is hired: {e:?}"));

        hr::record_salary(
            &self.db,
            &code(id),
            &hr::Salary {
                basic,
                allowances: vec![hr::Component {
                    what: "بدل سكن".to_owned(),
                    amount: money(basic.minor() / 4),
                }],
                deductions: deduction
                    .map(|amount| {
                        vec![hr::Component {
                            what: "سلفة".to_owned(),
                            amount,
                        }]
                    })
                    .unwrap_or_default(),
                commission_bp: 0,
            },
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{id} has a salary: {e:?}"));
    }

    async fn project(&self) {
        for _ in 0..2 {
            let owned = ledger::projections();
            let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<ledger::Ledger>(&self.pool, &refs, ledger::upcasters(), 200)
                .await
                .expect("ledger projects");

            let owned = payroll::projections();
            let refs: Vec<&dyn Projection<Group = payroll::Payroll>> =
                owned.iter().map(AsRef::as_ref).collect();
            run_to_head::<payroll::Payroll>(&self.pool, &refs, payroll::upcasters(), 200)
                .await
                .expect("payroll projects");
        }
    }

    async fn balance(&self, account: &str) -> Money {
        let mut conn = self.db.acquire().await.expect("connection");
        ledger::account_balances(&mut conn)
            .await
            .expect("reads")
            .into_iter()
            .find(|a| a.code == account)
            .map_or_else(|| money(0), |a| a.balance)
    }

    async fn imbalances(&self) -> Vec<ledger::TrialBalance> {
        let mut conn = self.db.acquire().await.expect("connection");
        ledger::imbalances(&mut conn).await.expect("reads")
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **Phase 9's exit criterion, the payroll half.**
///
/// A run posts, the books balance, and the entry says what it should: gross is
/// the cost, net is what is owed, and what is withheld is a liability rather
/// than a reduction of the cost.
#[tokio::test]
async fn a_payroll_run_posts_and_the_books_balance() {
    let fixture = Fixture::new().await;

    // Two people: one plain, one repaying an advance.
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;
    fixture
        .employ("EMP-2", "نورة", riyals(4_000), Some(riyals(500)))
        .await;

    // 8,000 + 2,000 housing = 10,000; 4,000 + 1,000 = 5,000. Gross 15,000,
    // deductions 500, net 14,500.
    let drafted = draft_run(
        &fixture.db,
        &code("PAY-2026-05"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1"), code("EMP-2")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");
    assert!(drafted.at.is_some());

    // **Drafting posts nothing.** That is the whole reason it is a step of its
    // own: a business reads the draft before it reaches the ledger.
    assert_eq!(
        fixture.balance("5000").await,
        money(0),
        "drafting posted to the ledger"
    );

    approve_run(&fixture.db, &code("PAY-2026-05"), &Metadata::default())
        .await
        .expect("approves");

    fixture.project().await;

    assert_eq!(
        fixture.balance("5000").await,
        riyals(15_000),
        "the expense is the gross cost, not what was handed over"
    );
    assert_eq!(fixture.balance("2200").await, riyals(-14_500));
    assert_eq!(fixture.balance("2210").await, riyals(-500));
    assert!(
        fixture.imbalances().await.is_empty(),
        "the trial balance does not balance"
    );

    // And the run reads back with its payslips.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let run = payroll::run(&mut conn, "PAY-2026-05")
        .await
        .expect("reads")
        .expect("is there");
    assert_eq!(run.period, "2026-05");
    assert_eq!(run.gross, riyals(15_000));
    assert_eq!(run.net, riyals(14_500));
    assert_eq!(run.people, 2);
    assert!(run.approved_at.is_some());
    assert_eq!(run.entry.as_deref(), Some("pr.PAY-2026-05"));

    let slips = payroll::payslips(&mut conn, "PAY-2026-05")
        .await
        .expect("reads");
    assert_eq!(slips.len(), 2);
    let sara = slips.iter().find(|s| s.employee == "EMP-1").expect("there");
    assert_eq!(sara.gross, riyals(10_000));
    assert_eq!(sara.net, riyals(10_000));
    let noura = slips.iter().find(|s| s.employee == "EMP-2").expect("there");
    assert_eq!(noura.gross, riyals(5_000));
    assert_eq!(noura.deductions, riyals(500));
    assert_eq!(noura.net, riyals(4_500));
    drop(conn);

    fixture.cleanup().await;
}

/// **The entry is dated to the period, not to the day it was approved.**
///
/// A February run approved on the 3rd of March belongs in February, and the
/// whole point of a period is that it does.
#[tokio::test]
async fn the_entry_lands_in_the_month_the_run_is_for() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;

    draft_run(
        &fixture.db,
        &code("PAY-2026-02"),
        Period::parse("2026-02").expect("a month"),
        &[code("EMP-1")],
        // Drafted in March, for February.
        &[],
        on("2026-03-03"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");
    approve_run(&fixture.db, &code("PAY-2026-02"), &Metadata::default())
        .await
        .expect("approves");

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let entry: (chrono::DateTime<chrono::Utc>,) = sqlx::query_as(
        "SELECT DISTINCT occurred_on FROM proj_ledger.posting
          WHERE entry_id = 'pr.PAY-2026-02'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("the entry is there");
    drop(conn);

    assert_eq!(
        entry.0.date_naive(),
        chrono::NaiveDate::from_ymd_opt(2026, 2, 28).expect("a real date"),
        "the entry landed in the month it was approved rather than the month it is for"
    );

    fixture.cleanup().await;
}

/// **Redrafting replaces.** A run that accumulated drafts would pay somebody
/// twice.
#[tokio::test]
async fn redrafting_replaces_the_previous_draft() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;
    fixture.employ("EMP-2", "نورة", riyals(4_000), None).await;

    let period = Period::parse("2026-05").expect("a month");
    draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[code("EMP-1"), code("EMP-2")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");

    // The same draft again writes nothing — a screen recomputing on open is not
    // a change.
    let again = draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[code("EMP-1"), code("EMP-2")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("a retry is not an error");
    assert!(again.at.is_none(), "an unchanged redraft wrote an event");

    // Somebody was in the run who should not have been.
    draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[code("EMP-1")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("redrafts");

    approve_run(&fixture.db, &code("PAY-1"), &Metadata::default())
        .await
        .expect("approves");
    fixture.project().await;

    assert_eq!(
        fixture.balance("5000").await,
        riyals(10_000),
        "the replaced draft was paid as well as the one that replaced it"
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let slips = payroll::payslips(&mut conn, "PAY-1").await.expect("reads");
    assert_eq!(slips.len(), 1, "a payslip from the replaced draft survived");
    drop(conn);

    fixture.cleanup().await;
}

/// An approved run cannot be redrafted: the entry is in the books and the
/// payslips are what people were told.
#[tokio::test]
async fn an_approved_run_cannot_be_changed() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;

    let period = Period::parse("2026-05").expect("a month");
    draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[code("EMP-1")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");
    approve_run(&fixture.db, &code("PAY-1"), &Metadata::default())
        .await
        .expect("approves");

    let error = draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect_err("an empty run is refused before anything else");
    assert!(format!("{error:?}").contains("NobodyToPay"), "{error:?}");

    fixture.employ("EMP-2", "نورة", riyals(4_000), None).await;
    let error = draft_run(
        &fixture.db,
        &code("PAY-1"),
        period,
        &[code("EMP-1"), code("EMP-2")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect_err("an approved run was redrafted");
    assert!(format!("{error:?}").contains("Approved"), "{error:?}");

    fixture.cleanup().await;
}

/// **A retried approval posts once.** An approval that timed out is safe to
/// send again, which is the property every money-moving command here has.
#[tokio::test]
async fn a_retried_approval_posts_once() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;

    draft_run(
        &fixture.db,
        &code("PAY-1"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");

    for _ in 0..3 {
        approve_run(&fixture.db, &code("PAY-1"), &Metadata::default())
            .await
            .expect("a retry is not an error");
    }

    fixture.project().await;
    assert_eq!(
        fixture.balance("5000").await,
        riyals(10_000),
        "an approval was posted more than once"
    );

    fixture.cleanup().await;
}

/// **Somebody with no salary refuses the whole run**, rather than being skipped.
/// A run that quietly left somebody out is a run somebody notices on payday.
#[tokio::test]
async fn a_run_refuses_rather_than_quietly_leaving_somebody_out() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;

    // On the books, and nobody has entered what she is paid.
    hr::hire(
        &fixture.db,
        &code("EMP-2"),
        &hr::Hire {
            details: hr::Details {
                name: "نورة".to_owned(),
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
    .expect("hired");

    let error = draft_run(
        &fixture.db,
        &code("PAY-1"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1"), code("EMP-2")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect_err("somebody with no salary was silently paid nothing");
    assert!(format!("{error:?}").contains("NotPayable"), "{error:?}");

    fixture.cleanup().await;
}

/// Somebody who has left is not paid — and the refusal names them rather than
/// producing a run that is quietly short.
#[tokio::test]
async fn somebody_who_has_left_is_not_in_the_run() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;
    hr::record_leaving(
        &fixture.db,
        &code("EMP-1"),
        "استقالت",
        on("2026-04-30"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    let error = draft_run(
        &fixture.db,
        &code("PAY-1"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1")],
        &[],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect_err("somebody who left was paid");
    assert!(format!("{error:?}").contains("NotPayable"), "{error:?}");

    fixture.cleanup().await;
}

/// **The caller sends what was performed; the rate is the employee's.**
///
/// That split is the whole design: who is in the run and what they did are
/// facts a person assembles, and a caller could get either wrong. What fraction
/// of it they earn is a term of their employment, read from their own record —
/// so a caller can be wrong about the *basis* and never about the *commission*.
#[tokio::test]
async fn commission_is_computed_from_the_rate_on_the_record() {
    let fixture = Fixture::new().await;

    // 8,000 basic plus 2,000 housing, and five per cent of what she performs.
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
    .expect("hired");
    hr::record_salary(
        &fixture.db,
        &code("EMP-1"),
        &hr::Salary {
            basic: riyals(8_000),
            allowances: vec![hr::Component {
                what: "بدل سكن".to_owned(),
                amount: riyals(2_000),
            }],
            deductions: Vec::new(),
            commission_bp: 500,
        },
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("has a salary");

    // She performed 24,000 of work. Five per cent is 1,200.
    draft_run(
        &fixture.db,
        &code("PAY-1"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1")],
        &[(code("EMP-1"), riyals(24_000))],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");
    approve_run(&fixture.db, &code("PAY-1"), &Metadata::default())
        .await
        .expect("approves");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let slips = payroll::payslips(&mut conn, "PAY-1").await.expect("reads");
    drop(conn);

    let slip = &slips[0];
    assert_eq!(slip.commission, riyals(1_200), "five per cent of 24,000");
    assert_eq!(
        slip.performed,
        riyals(24_000),
        "the payslip cannot justify the number it paid"
    );

    // **Commission is part of gross**, because statutory contributions and
    // end-of-service are computed from what somebody earned rather than from
    // the predictable part of it.
    assert_eq!(slip.gross, riyals(11_200));
    assert_eq!(slip.net, riyals(11_200));

    assert_eq!(
        fixture.balance("5000").await,
        riyals(11_200),
        "the wage cost left the commission out"
    );
    assert!(fixture.imbalances().await.is_empty());

    fixture.cleanup().await;
}

/// Somebody on no commission rate earns none, however much they performed — the
/// rate is the record's and a basis cannot create one.
#[tokio::test]
async fn a_basis_without_a_rate_earns_nothing() {
    let fixture = Fixture::new().await;
    fixture.employ("EMP-1", "سارة", riyals(8_000), None).await;

    draft_run(
        &fixture.db,
        &code("PAY-1"),
        Period::parse("2026-05").expect("a month"),
        &[code("EMP-1")],
        &[(code("EMP-1"), riyals(99_000))],
        on("2026-05-28"),
        &Metadata::default(),
    )
    .await
    .expect("drafts");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let slips = payroll::payslips(&mut conn, "PAY-1").await.expect("reads");
    drop(conn);

    assert_eq!(
        slips[0].commission,
        riyals(0),
        "a caller's basis created a commission nobody agreed to"
    );
    assert_eq!(slips[0].gross, riyals(10_000));

    fixture.cleanup().await;
}
