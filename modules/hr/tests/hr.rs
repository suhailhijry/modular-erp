//! The org chart, against a real tenant.
//!
//! The tests that carry this file are the three that make §9b's one line
//! either a good design or a dangerous one:
//! [`a_claim_travels_up_the_reporting_line`],
//! [`a_segregated_claim_travels_nowhere`], and
//! [`a_claim_carries_its_branch_up_the_tree`].

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{Actor, ClusterRegistry, ControlPlane, PoolConfig, TenantDb, TenantPools};
use erp_eventlog::Metadata;
use erp_projection::ensure_group_schema;
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};
use hr::{Claim, Details, Hire};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

fn on(day: &str) -> Timestamp {
    format!("{day}T00:00:00Z").parse().expect("a valid instant")
}

fn details(name: &str) -> Details {
    Details {
        name: name.to_owned(),
        name_latin: None,
        national_id: None,
        email: None,
        phone: Some("+966500000000".to_owned()),
    }
}

fn claim(name: &str) -> Claim {
    Claim {
        name: name.to_owned(),
        branch: None,
    }
}

fn at(name: &str, branch: &str) -> Claim {
    Claim {
        name: name.to_owned(),
        branch: Some(branch.to_owned()),
    }
}

struct Fixture {
    db: TenantDb,
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
            branches::install(&mut conn)
                .await
                .expect("branches installs");
            ensure_group_schema::<branches::Branches>(&mut conn)
                .await
                .expect("branches checkpoint");
            hr::install(&mut conn).await.expect("hr installs");
            ensure_group_schema::<hr::Hr>(&mut conn)
                .await
                .expect("hr checkpoint");
        }

        Self {
            db,
            _control: control,
            _control_db: control_db,
            database: tenant.database_name,
        }
    }

    /// Puts somebody on the books under a manager.
    async fn hire(&self, id: &str, name: &str, reports_to: Option<&str>, branch: Option<&str>) {
        hr::hire(
            &self.db,
            &code(id),
            &Hire {
                details: details(name),
                reports_to: reports_to.map(code),
                branch: branch.map(code),
                at: on("2026-01-01"),
            },
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{id} is hired: {e:?}"));
    }

    async fn open_branch(&self, id: &str, name: &str) {
        branches::open_branch(
            &self.db,
            &code(id),
            &branches::Details {
                name: name.to_owned(),
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
    }

    async fn holds(&self, employee: &str, claim: &str, branch: Option<&str>) -> bool {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::holds(&mut conn, &code(employee), claim, branch)
            .await
            .expect("asks")
    }

    async fn effective(&self, employee: &str) -> Vec<hr::Held> {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::effective(&mut conn, &code(employee))
            .await
            .expect("reads")
    }

    async fn may_work(&self, employee: &str, day: &str) -> bool {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::may_work_on(
            &mut conn,
            &code(employee),
            day.parse().expect("a valid date"),
        )
        .await
        .expect("asks")
    }

    async fn eligible(&self, employee: &str, service: &str, day: chrono::NaiveDate) -> bool {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::eligible_for(&mut conn, &code(employee), &code(service), day)
            .await
            .expect("asks")
    }

    async fn project(&self) {
        let owned = hr::projections();
        let refs: Vec<&dyn erp_projection::Projection<Group = hr::Hr>> =
            owned.iter().map(AsRef::as_ref).collect();
        let url = erp_testkit::database_url();
        let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
        let pool = sqlx::PgPool::connect(&format!("{base}/{}", self.database))
            .await
            .expect("connects");
        erp_projection::run_to_head::<hr::Hr>(&pool, &refs, hr::upcasters(), 200)
            .await
            .expect("hr projects");
        pool.close().await;
    }

    async fn cleanup(self) {
        drop(self.db);
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// A three-level chart: owner, manager, clerk.
async fn a_small_company() -> Fixture {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-OWNER", "المالك", None, None).await;
    fixture
        .hire("EMP-MANAGER", "المديرة", Some("EMP-OWNER"), None)
        .await;
    fixture
        .hire("EMP-CLERK", "الكاشير", Some("EMP-MANAGER"), None)
        .await;
    fixture
}

/// **The rule, and the reason for it.**
///
/// Granting the clerk something gives it to their manager and to the owner, so
/// nobody has to remember that a new permission for a junior also needs giving
/// to whoever covers for them. The failure this refuses is the support ticket
/// *"the branch manager cannot approve what her own cashier can"*.
#[tokio::test]
async fn a_claim_travels_up_the_reporting_line() {
    let fixture = a_small_company().await;

    let gained = hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
        true,
    )
    .await
    .expect("granted");

    // **The grant reports everyone who gained it**, which is the whole reason a
    // grant at a leaf is not a local act.
    assert_eq!(
        gained,
        vec![
            "EMP-CLERK".to_owned(),
            "EMP-MANAGER".to_owned(),
            "EMP-OWNER".to_owned()
        ],
        "a grant at a leaf escalated ancestors without saying so"
    );

    for who in ["EMP-CLERK", "EMP-MANAGER", "EMP-OWNER"] {
        assert!(
            fixture.holds(who, "sales.apply_discount", None).await,
            "{who} did not inherit what their team holds"
        );
    }

    // And it does not travel *down*: giving the owner something does not give
    // it to everybody, which would be the other, much worse, direction.
    hr::grant_claim(
        &fixture.db,
        &code("EMP-OWNER"),
        &claim("hr.approve_leave"),
        true,
    )
    .await
    .expect("granted");
    assert!(fixture.holds("EMP-OWNER", "hr.approve_leave", None).await);
    assert!(
        !fixture.holds("EMP-CLERK", "hr.approve_leave", None).await,
        "a claim travelled downward, which would make every grant a company-wide one"
    );

    fixture.cleanup().await;
}

/// **Segregation of duties, which the union would otherwise break.**
///
/// The control every accounting system is measured on is that the person who
/// raises an invoice is not the person who approves its payment. Under a
/// bottom-up union their shared manager holds both the moment the chart says
/// so — automatically, silently, and in a way that fails an audit.
#[tokio::test]
async fn a_segregated_claim_travels_nowhere() {
    let fixture = a_small_company().await;

    // Even asking for it to propagate does not make it propagate. What an
    // auditor requires is not a preference a tenant expresses.
    let gained = hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("purchases.approve_payment"),
        true,
    )
    .await
    .expect("granted");

    assert_eq!(
        gained,
        vec!["EMP-CLERK".to_owned()],
        "a segregation-of-duties claim escalated to a manager"
    );
    assert!(
        fixture
            .holds("EMP-CLERK", "purchases.approve_payment", None)
            .await
    );
    assert!(
        !fixture
            .holds("EMP-MANAGER", "purchases.approve_payment", None)
            .await,
        "the shared manager now holds both halves of a segregated pair"
    );
    assert!(
        !fixture
            .holds("EMP-OWNER", "purchases.approve_payment", None)
            .await
    );

    fixture.cleanup().await;
}

/// **The union is over `(claim, branch)` pairs, not bare claims.**
///
/// A regional manager over two branches accumulates authority in both.
/// Collapsing them to a bare claim would grant a branch manager authority in a
/// branch they have never seen.
#[tokio::test]
async fn a_claim_carries_its_branch_up_the_tree() {
    let fixture = Fixture::new().await;
    fixture.open_branch("BR-OLAYA", "العليا").await;
    fixture.open_branch("BR-MALAZ", "الملز").await;

    fixture
        .hire("EMP-REGION", "المدير الإقليمي", None, None)
        .await;
    fixture
        .hire(
            "EMP-OLAYA",
            "مدير العليا",
            Some("EMP-REGION"),
            Some("BR-OLAYA"),
        )
        .await;
    fixture
        .hire(
            "EMP-MALAZ",
            "مدير الملز",
            Some("EMP-REGION"),
            Some("BR-MALAZ"),
        )
        .await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-OLAYA"),
        &at("hr.approve_leave", "BR-OLAYA"),
        true,
    )
    .await
    .expect("granted");
    hr::grant_claim(
        &fixture.db,
        &code("EMP-MALAZ"),
        &at("hr.approve_leave", "BR-MALAZ"),
        true,
    )
    .await
    .expect("granted");

    // The regional manager accumulates both.
    assert!(
        fixture
            .holds("EMP-REGION", "hr.approve_leave", Some("BR-OLAYA"))
            .await
    );
    assert!(
        fixture
            .holds("EMP-REGION", "hr.approve_leave", Some("BR-MALAZ"))
            .await
    );

    // **The Olaya manager does not gain Malaz.** This is the assertion that
    // fails if the union ever collapses the branch away.
    assert!(
        fixture
            .holds("EMP-OLAYA", "hr.approve_leave", Some("BR-OLAYA"))
            .await
    );
    assert!(
        !fixture
            .holds("EMP-OLAYA", "hr.approve_leave", Some("BR-MALAZ"))
            .await,
        "a branch manager gained authority in a branch they have never seen"
    );

    fixture.cleanup().await;
}

/// A company-wide claim answers for any branch, which is what company-wide
/// means — and is why `None` is not the same as "some branch".
#[tokio::test]
async fn a_company_wide_claim_answers_everywhere() {
    let fixture = Fixture::new().await;
    fixture.open_branch("BR-OLAYA", "العليا").await;
    fixture.hire("EMP-PAYROLL", "المحاسبة", None, None).await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-PAYROLL"),
        &claim("hr.run_payroll"),
        true,
    )
    .await
    .expect("granted");

    assert!(fixture.holds("EMP-PAYROLL", "hr.run_payroll", None).await);
    assert!(
        fixture
            .holds("EMP-PAYROLL", "hr.run_payroll", Some("BR-OLAYA"))
            .await,
        "payroll is company-wide by nature and was refused at a branch"
    );

    fixture.cleanup().await;
}

/// **A cycle is refused**, and not because it is untidy: the union would not
/// terminate. `A → B → A` is what two well-meaning edits a week apart produce.
#[tokio::test]
async fn a_reporting_line_cannot_loop() {
    let fixture = a_small_company().await;

    let error = hr::reparent(
        &fixture.db,
        &code("EMP-OWNER"),
        Some(&code("EMP-CLERK")),
        "خطأ",
        on("2026-02-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("a loop was allowed");
    assert!(
        format!("{error:?}").contains("Cycle"),
        "refused for the wrong reason: {error:?}"
    );

    // And nobody can report to themselves, which is the one-hop version.
    let error = hr::reparent(
        &fixture.db,
        &code("EMP-CLERK"),
        Some(&code("EMP-CLERK")),
        "خطأ",
        on("2026-02-01"),
        &Metadata::default(),
    )
    .await
    .expect_err("self-reporting was allowed");
    assert!(format!("{error:?}").contains("Cycle"));

    fixture.cleanup().await;
}

/// **Moving somebody moves everything they carry**, which is why it is its own
/// event: the old manager loses what the subtree held, and the new one gains
/// it.
#[tokio::test]
async fn moving_somebody_moves_what_their_team_holds() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-OWNER", "المالك", None, None).await;
    fixture
        .hire("EMP-SALES", "مديرة المبيعات", Some("EMP-OWNER"), None)
        .await;
    fixture
        .hire("EMP-OPS", "مدير العمليات", Some("EMP-OWNER"), None)
        .await;
    fixture
        .hire("EMP-CLERK", "الكاشير", Some("EMP-SALES"), None)
        .await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
        true,
    )
    .await
    .expect("granted");
    assert!(
        fixture
            .holds("EMP-SALES", "sales.apply_discount", None)
            .await
    );
    assert!(!fixture.holds("EMP-OPS", "sales.apply_discount", None).await);

    hr::reparent(
        &fixture.db,
        &code("EMP-CLERK"),
        Some(&code("EMP-OPS")),
        "نقل إلى العمليات",
        on("2026-02-01"),
        &Metadata::default(),
    )
    .await
    .expect("moved");

    assert!(
        fixture.holds("EMP-OPS", "sales.apply_discount", None).await,
        "the new manager did not gain what their new report holds"
    );
    assert!(
        !fixture
            .holds("EMP-SALES", "sales.apply_discount", None)
            .await,
        "the old manager kept authority over somebody who left their team"
    );
    // The owner is above both, so nothing changes for them.
    assert!(
        fixture
            .holds("EMP-OWNER", "sales.apply_discount", None)
            .await
    );

    fixture.cleanup().await;
}

/// **Authority ends when somebody leaves. Their record does not.**
#[tokio::test]
async fn a_leaver_keeps_their_record_and_loses_their_claims() {
    let fixture = a_small_company().await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
        true,
    )
    .await
    .expect("granted");
    assert!(
        fixture
            .holds("EMP-OWNER", "sales.apply_discount", None)
            .await
    );

    hr::record_leaving(
        &fixture.db,
        &code("EMP-CLERK"),
        "استقال",
        on("2026-03-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    assert!(
        !fixture
            .holds("EMP-CLERK", "sales.apply_discount", None)
            .await,
        "somebody who left kept their authority"
    );
    assert!(
        !fixture
            .holds("EMP-OWNER", "sales.apply_discount", None)
            .await,
        "a manager kept authority inherited from somebody who has gone"
    );

    // The record is still there, and still in the chart — their team reports to
    // them until the business decides otherwise, which a resignation does not.
    let mut conn = fixture.db.acquire().await.expect("connection");
    let held = erp_eventlog::load::<hr::Employee>(&mut conn, &code("EMP-CLERK"), hr::upcasters())
        .await
        .expect("loads");
    assert!(held.aggregate.exists(), "the record was forgotten");
    assert!(!held.aggregate.is_employed());
    drop(conn);

    fixture.cleanup().await;
}

/// Every claim says where it came from, because the first question anybody asks
/// of an inherited permission is exactly that.
#[tokio::test]
async fn an_inherited_claim_says_who_it_came_from() {
    let fixture = a_small_company().await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
        true,
    )
    .await
    .expect("granted");
    hr::grant_claim(
        &fixture.db,
        &code("EMP-MANAGER"),
        &claim("hr.approve_leave"),
        true,
    )
    .await
    .expect("granted");

    let held = fixture.effective("EMP-MANAGER").await;
    let discount = held
        .iter()
        .find(|h| h.claim.name == "sales.apply_discount")
        .expect("inherited the discount");
    assert_eq!(
        discount.source, "EMP-CLERK",
        "an inherited claim could not say where it came from"
    );

    let own = held
        .iter()
        .find(|h| h.claim.name == "hr.approve_leave")
        .expect("holds their own");
    assert_eq!(
        own.source, "EMP-MANAGER",
        "their own claim named somebody else"
    );

    fixture.cleanup().await;
}

/// Revoking reports everybody who lost it, and it bites at once — there is no
/// projection between the revocation and the next check.
#[tokio::test]
async fn revoking_a_claim_bites_immediately_and_says_who_lost_it() {
    let fixture = a_small_company().await;

    hr::grant_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
        true,
    )
    .await
    .expect("granted");

    let lost = hr::revoke_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales.apply_discount"),
    )
    .await
    .expect("revoked");

    assert_eq!(
        lost,
        vec![
            "EMP-CLERK".to_owned(),
            "EMP-MANAGER".to_owned(),
            "EMP-OWNER".to_owned()
        ],
    );
    for who in ["EMP-CLERK", "EMP-MANAGER", "EMP-OWNER"] {
        assert!(
            !fixture.holds(who, "sales.apply_discount", None).await,
            "{who} kept a revoked claim"
        );
    }

    fixture.cleanup().await;
}

/// Somebody cannot be hired into a branch that is not open, and the check is
/// against the **log** — so a branch opened a moment ago works, and a closed
/// one does not.
#[tokio::test]
async fn nobody_is_hired_into_a_branch_that_is_not_open() {
    let fixture = Fixture::new().await;

    let error = hr::hire(
        &fixture.db,
        &code("EMP-1"),
        &Hire {
            details: details("سارة"),
            reports_to: None,
            branch: Some(code("BR-NOWHERE")),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect_err("hired into nowhere");
    assert!(format!("{error:?}").contains("NoSuchBranch"), "{error:?}");

    // Opened a moment ago, with no projection run: it works, because the check
    // reads the log.
    fixture.open_branch("BR-OLAYA", "العليا").await;
    fixture.hire("EMP-1", "سارة", None, Some("BR-OLAYA")).await;

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// 9e — documents that expire
// ---------------------------------------------------------------------------

fn day(s: &str) -> chrono::NaiveDate {
    s.parse().expect("a valid date")
}

/// **A lapsed document is a refusal, not a warning.**
///
/// An expired iqama does not mean a reminder somebody ignored. It means a
/// person who may not legally work, and rostering them is the employer's
/// offence.
#[tokio::test]
async fn somebody_whose_document_has_lapsed_may_not_be_rostered() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    // Nothing recorded: they may work. A business that has not started
    // recording documents must not find its whole rota refused the day this
    // module is switched on.
    assert!(fixture.may_work("EMP-1", "2026-06-01").await);

    hr::record_document(
        &fixture.db,
        &code("EMP-1"),
        hr::DocumentKind::Identity,
        "2312345678",
        day("2026-05-31"),
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    // Valid **on** its expiry date, which is what the document itself means and
    // what the person holding it will argue.
    assert!(fixture.may_work("EMP-1", "2026-05-31").await);
    assert!(
        !fixture.may_work("EMP-1", "2026-06-01").await,
        "somebody with a lapsed iqama was cleared to work"
    );

    // Renewed: the same command, and they may work again.
    hr::record_document(
        &fixture.db,
        &code("EMP-1"),
        hr::DocumentKind::Identity,
        "2312345678",
        day("2027-05-31"),
        on("2026-05-20"),
        &Metadata::default(),
    )
    .await
    .expect("renewed");
    assert!(fixture.may_work("EMP-1", "2026-06-01").await);

    fixture.cleanup().await;
}

/// Somebody who has left may not be rostered either — the same question one
/// step earlier, so a caller cannot get one right and forget the other.
#[tokio::test]
async fn somebody_who_has_left_may_not_be_rostered() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;
    assert!(fixture.may_work("EMP-1", "2026-06-01").await);

    hr::record_leaving(
        &fixture.db,
        &code("EMP-1"),
        "استقالت",
        on("2026-03-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    assert!(!fixture.may_work("EMP-1", "2026-06-01").await);
    fixture.cleanup().await;
}

/// The expiry screen shows what has gone **and** what is about to, soonest
/// first — burying the lapsed ones below the upcoming ones is how they stay
/// buried.
#[tokio::test]
async fn the_expiry_list_shows_what_has_gone_and_what_is_going() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;
    fixture.hire("EMP-2", "نورة", None, None).await;

    let today = chrono::Utc::now().date_naive();
    let recorded = [
        // Gone a week ago.
        (
            "EMP-1",
            hr::DocumentKind::Identity,
            today - chrono::Days::new(7),
        ),
        // Going in ten days.
        (
            "EMP-2",
            hr::DocumentKind::Medical,
            today + chrono::Days::new(10),
        ),
        // Two years out, and not this screen's business.
        (
            "EMP-2",
            hr::DocumentKind::Licence,
            today + chrono::Days::new(730),
        ),
    ];
    for (who, kind, expires) in recorded {
        hr::record_document(
            &fixture.db,
            &code(who),
            kind,
            "X-1",
            expires,
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("recorded");
    }

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let soon = hr::expiring(&mut conn, 60, 50).await.expect("reads");
    drop(conn);

    assert_eq!(soon.len(), 2, "a two-year licence was reported as expiring");
    assert_eq!(soon[0].employee, "EMP-1", "the lapsed one was not first");
    assert!(
        soon[0].days_left < 0,
        "a lapsed document reported as though it had time left"
    );
    assert_eq!(soon[1].employee, "EMP-2");
    assert!(soon[1].days_left > 0);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// 9a — skills
// ---------------------------------------------------------------------------

/// **An empty skill list means anything, not nothing.**
///
/// The alternative would refuse every assignment in every existing tenant the
/// day this module is switched on. The sharp edge — that recording the *first*
/// skill starts restricting — is why the API takes the whole set at once.
#[tokio::test]
async fn nobody_is_restricted_until_a_skill_is_recorded() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    let today = chrono::Utc::now().date_naive();
    assert!(
        fixture.eligible("EMP-1", "SERVICE-CUT", today).await,
        "somebody with no skills recorded was refused a service"
    );

    hr::record_skills(
        &fixture.db,
        &code("EMP-1"),
        &[code("SERVICE-COLOUR")],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    assert!(fixture.eligible("EMP-1", "SERVICE-COLOUR", today).await);
    assert!(
        !fixture.eligible("EMP-1", "SERVICE-CUT", today).await,
        "recording one skill did not restrict the rest"
    );

    // Recording the set again writes nothing — a form submitted twice is not
    // two changes.
    let again = hr::record_skills(
        &fixture.db,
        &code("EMP-1"),
        &[code("SERVICE-COLOUR")],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("a retry is not an error");
    assert!(again.at.is_none(), "the same set wrote a second event");

    fixture.cleanup().await;
}

/// Eligibility is **one question**: a lapsed document and a missing skill both
/// mean the same thing here — not somebody who can do this job.
#[tokio::test]
async fn eligibility_asks_about_documents_and_skills_together() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;
    hr::record_skills(
        &fixture.db,
        &code("EMP-1"),
        &[code("SERVICE-CUT")],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    let today = chrono::Utc::now().date_naive();
    assert!(fixture.eligible("EMP-1", "SERVICE-CUT", today).await);

    // Qualified, and her iqama has gone.
    hr::record_document(
        &fixture.db,
        &code("EMP-1"),
        hr::DocumentKind::Identity,
        "2312345678",
        today - chrono::Days::new(1),
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    assert!(
        !fixture.eligible("EMP-1", "SERVICE-CUT", today).await,
        "a qualified person with a lapsed document was still eligible"
    );

    fixture.cleanup().await;
}

/// **The screen and the rota must agree**, so both answer from the same rule:
/// somebody with no skills recorded can do anything, and appears in the list of
/// who can do a service.
#[tokio::test]
async fn the_who_can_do_this_list_matches_what_assign_would_allow() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-ANY", "سارة", None, None).await;
    fixture.hire("EMP-CUT", "نورة", None, None).await;
    fixture.hire("EMP-COLOUR", "ريم", None, None).await;

    hr::record_skills(
        &fixture.db,
        &code("EMP-CUT"),
        &[code("SERVICE-CUT")],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");
    hr::record_skills(
        &fixture.db,
        &code("EMP-COLOUR"),
        &[code("SERVICE-COLOUR")],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let can = hr::who_can_perform(&mut conn, "SERVICE-CUT", 50)
        .await
        .expect("reads");
    drop(conn);

    let ids: Vec<&str> = can.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&"EMP-CUT"), "the qualified one was missing");
    assert!(
        ids.contains(&"EMP-ANY"),
        "somebody with no skills recorded was left off, though `assign` \
         would allow them — the screen and the rota disagree"
    );
    assert!(
        !ids.contains(&"EMP-COLOUR"),
        "somebody qualified for a different service was offered"
    );

    // And the two agree the other way too.
    let today = chrono::Utc::now().date_naive();
    for id in &ids {
        assert!(
            fixture.eligible(id, "SERVICE-CUT", today).await,
            "{id} was offered by the screen and would be refused by the rota"
        );
    }

    fixture.cleanup().await;
}
