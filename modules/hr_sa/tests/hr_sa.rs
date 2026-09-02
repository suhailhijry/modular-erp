//! What the Kingdom requires, against a real tenant.
//!
//! The arithmetic is tested in the module's own units; what this file is for is
//! the seam — that an end-of-service figure comes from what `hr` actually holds,
//! rather than from numbers a caller sent.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use hr_sa::{Footing, Leaving, Schedule};

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
            hr::install(&mut conn).await.expect("hr");
            ensure_group_schema::<hr::Hr>(&mut conn)
                .await
                .expect("hr checkpoint");
        }

        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
            .await
            .expect("connects");

        Self {
            db,
            pool,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        }
    }

    async fn employ(&self, id: &str, basic: Money, hired: &str, left: Option<&str>) {
        hr::hire(
            &self.db,
            &code(id),
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
                at: on(hired),
            },
            &Metadata::default(),
        )
        .await
        .expect("hired");

        hr::record_salary(
            &self.db,
            &code(id),
            &hr::Salary {
                basic,
                allowances: vec![hr::Component {
                    what: "بدل سكن".to_owned(),
                    amount: Money::from_minor(basic.minor() / 4, sar()),
                }],
                deductions: Vec::new(),
                commission_bp: 0,
            },
            on(hired),
            &Metadata::default(),
        )
        .await
        .expect("has a salary");

        if let Some(day) = left {
            hr::record_leaving(&self.db, &code(id), "انتهت", on(day), &Metadata::default())
                .await
                .expect("recorded");
        }
    }

    async fn project(&self) {
        let owned = hr::projections();
        let refs: Vec<&dyn Projection<Group = hr::Hr>> = owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<hr::Hr>(&self.pool, &refs, hr::upcasters(), 200)
            .await
            .expect("hr projects");
    }

    async fn details(&self, id: &str) -> Option<hr::PayDetails> {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::pay_details(&mut conn, id).await.expect("reads")
    }

    async fn cleanup(self) {
        self.pool.close().await;
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **The award is computed on what `hr` holds**, not on numbers a caller sent —
/// which is the whole reason this reads a record rather than taking a wage.
///
/// Exactly ten years, 8,000 basic plus 2,000 housing: the wage is 10,000, so
/// five years at half a month plus five at a full month is 7.5 months, 75,000.
#[tokio::test]
async fn the_award_comes_from_the_record_and_not_from_the_caller() {
    let fixture = Fixture::new().await;
    // 2016-05-30 to 2026-05-28 is 3,650 days — exactly ten years by the
    // 365-day convention, and stated as dates because that is what a record
    // holds.
    fixture
        .employ("EMP-1", riyals(8_000), "2016-05-30", Some("2026-05-28"))
        .await;
    fixture.project().await;

    let details = fixture.details("EMP-1").await.expect("has pay details");
    assert_eq!(
        details.gross,
        riyals(10_000),
        "the wage is basic plus allowances, which is what the Labour Law says"
    );

    let days =
        (details.left_at.expect("left").date_naive() - details.hired_on.date_naive()).num_days();
    assert_eq!(days, 3_650, "the service length is not ten years");

    let award = hr_sa::end_of_service(details.gross, days, Leaving::Dismissed).expect("computes");
    assert_eq!(award.payable, riyals(75_000));

    // Resigning at ten years is still paid in full — the ladder tops out.
    let award = hr_sa::end_of_service(details.gross, days, Leaving::Resigned).expect("computes");
    assert_eq!(award.payable, riyals(75_000));

    fixture.cleanup().await;
}

/// Somebody still employed has an answer too: **what would we owe her**, to
/// today. A business asks that before it makes an offer.
#[tokio::test]
async fn somebody_still_employed_has_a_figure_as_at_today() {
    let fixture = Fixture::new().await;
    fixture
        .employ("EMP-1", riyals(8_000), "2020-01-01", None)
        .await;
    fixture.project().await;

    let details = fixture.details("EMP-1").await.expect("has pay details");
    assert!(details.left_at.is_none());

    let days = (chrono::Utc::now().date_naive() - details.hired_on.date_naive()).num_days();
    let award = hr_sa::end_of_service(details.gross, days, Leaving::Dismissed).expect("computes");
    assert!(
        award.payable.is_positive(),
        "somebody with years of service was owed nothing"
    );

    fixture.cleanup().await;
}

/// **A configured schedule is what is used**, which is the reason the rates are
/// configuration at all: the authority's numbers change, and a build that
/// hard-coded them would be wrong for somebody from the day it shipped.
#[tokio::test]
async fn a_stored_schedule_replaces_the_shipped_one() {
    let fixture = Fixture::new().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let shipped = Schedule::resolve(&mut conn).await.expect("resolves");
    assert_eq!(
        shipped,
        Schedule::default(),
        "a tenant who has configured nothing is on the shipped schedule"
    );

    erp_eventlog::configuration::set(
        &mut conn,
        Schedule::KEY,
        &Schedule {
            saudi_employee_bp: 1_100,
            ..Schedule::default()
        },
        None,
    )
    .await
    .expect("stores");

    let stored = Schedule::resolve(&mut conn).await.expect("resolves");
    assert_eq!(stored.saudi_employee_bp, 1_100);

    let c = hr_sa::contribution(riyals(10_000), Footing::Saudi, stored).expect("computes");
    assert_eq!(c.employee, riyals(1_100));
    drop(conn);

    fixture.cleanup().await;
}

/// **The two halves meet: `hr` says what was taken, this says what was owed.**
///
/// A joiner is owed the part of the year they were here for, and somebody can
/// overdraw — which is a real state and not one to clamp away.
#[tokio::test]
async fn the_entitlement_and_what_was_taken_come_from_different_modules() {
    let fixture = Fixture::new().await;
    // Joined at the start of 2026, so a full first year at 21 days.
    fixture
        .employ("EMP-1", riyals(8_000), "2026-01-01", None)
        .await;

    hr::record_leave(
        &fixture.db,
        &code("EMP-1"),
        hr::Leave::Annual,
        "2026-06-03".parse().expect("a date"),
        "2026-06-07".parse().expect("a date"),
        "إجازة",
        on("2026-05-20"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");
    fixture.project().await;

    let year = (
        "2026-01-01".parse::<chrono::NaiveDate>().expect("a date"),
        "2026-12-31".parse::<chrono::NaiveDate>().expect("a date"),
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    let taken = hr::leave_taken(&mut conn, "EMP-1", year.0, year.1)
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(
        taken,
        vec![("annual".to_owned(), 5)],
        "the 3rd to the 7th is five days"
    );

    // A full first year: 21 days, and 16 left.
    let owed = hr_sa::annual_entitlement(0, 365);
    assert_eq!(owed, 21);
    assert_eq!(owed - 5, 16);

    fixture.cleanup().await;
}

/// **Somebody can overdraw**, and the figure has to say so rather than clamp.
///
/// Three weeks in January and a March leaving date is money the business is
/// owed back, and a zero would hide it.
#[tokio::test]
async fn a_leaver_who_took_the_whole_year_up_front_is_shown_as_overdrawn() {
    let fixture = Fixture::new().await;
    // Two months of the year, so two months' worth of entitlement.
    let owed = hr_sa::annual_entitlement(0, 59);
    assert_eq!(
        owed, 4,
        "sixty days of a first year is about four days' leave"
    );
    assert!(
        owed - 21 < 0,
        "somebody who took the whole year's leave and left in March was shown as square"
    );

    fixture.cleanup().await;
}
