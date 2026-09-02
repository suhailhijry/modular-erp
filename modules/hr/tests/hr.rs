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

    async fn working(&self, employee: &str, span: erp_occupancy::Span) -> bool {
        let mut conn = self.db.acquire().await.expect("connection");
        hr::is_working_at(&mut conn, &code(employee), span)
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
        &claim("sales:apply_discount"),
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
            fixture.holds(who, "sales:apply_discount", None).await,
            "{who} did not inherit what their team holds"
        );
    }

    // And it does not travel *down*: giving the owner something does not give
    // it to everybody, which would be the other, much worse, direction.
    hr::grant_claim(
        &fixture.db,
        &code("EMP-OWNER"),
        &claim("hr:approve_leave"),
        true,
    )
    .await
    .expect("granted");
    assert!(fixture.holds("EMP-OWNER", "hr:approve_leave", None).await);
    assert!(
        !fixture.holds("EMP-CLERK", "hr:approve_leave", None).await,
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
        &claim("purchases:approve_payment"),
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
            .holds("EMP-CLERK", "purchases:approve_payment", None)
            .await
    );
    assert!(
        !fixture
            .holds("EMP-MANAGER", "purchases:approve_payment", None)
            .await,
        "the shared manager now holds both halves of a segregated pair"
    );
    assert!(
        !fixture
            .holds("EMP-OWNER", "purchases:approve_payment", None)
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
        &at("hr:approve_leave", "BR-OLAYA"),
        true,
    )
    .await
    .expect("granted");
    hr::grant_claim(
        &fixture.db,
        &code("EMP-MALAZ"),
        &at("hr:approve_leave", "BR-MALAZ"),
        true,
    )
    .await
    .expect("granted");

    // The regional manager accumulates both.
    assert!(
        fixture
            .holds("EMP-REGION", "hr:approve_leave", Some("BR-OLAYA"))
            .await
    );
    assert!(
        fixture
            .holds("EMP-REGION", "hr:approve_leave", Some("BR-MALAZ"))
            .await
    );

    // **The Olaya manager does not gain Malaz.** This is the assertion that
    // fails if the union ever collapses the branch away.
    assert!(
        fixture
            .holds("EMP-OLAYA", "hr:approve_leave", Some("BR-OLAYA"))
            .await
    );
    assert!(
        !fixture
            .holds("EMP-OLAYA", "hr:approve_leave", Some("BR-MALAZ"))
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
        &claim("hr:run_payroll"),
        true,
    )
    .await
    .expect("granted");

    assert!(fixture.holds("EMP-PAYROLL", "hr:run_payroll", None).await);
    assert!(
        fixture
            .holds("EMP-PAYROLL", "hr:run_payroll", Some("BR-OLAYA"))
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
        &claim("sales:apply_discount"),
        true,
    )
    .await
    .expect("granted");
    assert!(
        fixture
            .holds("EMP-SALES", "sales:apply_discount", None)
            .await
    );
    assert!(!fixture.holds("EMP-OPS", "sales:apply_discount", None).await);

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
        fixture.holds("EMP-OPS", "sales:apply_discount", None).await,
        "the new manager did not gain what their new report holds"
    );
    assert!(
        !fixture
            .holds("EMP-SALES", "sales:apply_discount", None)
            .await,
        "the old manager kept authority over somebody who left their team"
    );
    // The owner is above both, so nothing changes for them.
    assert!(
        fixture
            .holds("EMP-OWNER", "sales:apply_discount", None)
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
        &claim("sales:apply_discount"),
        true,
    )
    .await
    .expect("granted");
    assert!(
        fixture
            .holds("EMP-OWNER", "sales:apply_discount", None)
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
            .holds("EMP-CLERK", "sales:apply_discount", None)
            .await,
        "somebody who left kept their authority"
    );
    assert!(
        !fixture
            .holds("EMP-OWNER", "sales:apply_discount", None)
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
        &claim("sales:apply_discount"),
        true,
    )
    .await
    .expect("granted");
    hr::grant_claim(
        &fixture.db,
        &code("EMP-MANAGER"),
        &claim("hr:approve_leave"),
        true,
    )
    .await
    .expect("granted");

    let held = fixture.effective("EMP-MANAGER").await;
    let discount = held
        .iter()
        .find(|h| h.claim.name == "sales:apply_discount")
        .expect("inherited the discount");
    assert_eq!(
        discount.source, "EMP-CLERK",
        "an inherited claim could not say where it came from"
    );

    let own = held
        .iter()
        .find(|h| h.claim.name == "hr:approve_leave")
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
        &claim("sales:apply_discount"),
        true,
    )
    .await
    .expect("granted");

    let lost = hr::revoke_claim(
        &fixture.db,
        &code("EMP-CLERK"),
        &claim("sales:apply_discount"),
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
            !fixture.holds(who, "sales:apply_discount", None).await,
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

// ---------------------------------------------------------------------------
// 9a — shifts
// ---------------------------------------------------------------------------

fn weekdays(days: &[u8], opens: u16, closes: u16) -> erp_recurrence::Availability {
    erp_recurrence::Availability::from_parts(&[], days, &[], opens, closes, None, None)
        .expect("a valid rule")
}

fn span(from: &str, until: &str) -> erp_occupancy::Span {
    erp_occupancy::Span::new(
        format!("{from}:00Z").parse().expect("an instant"),
        format!("{until}:00Z").parse().expect("an instant"),
    )
    .expect("a valid span")
}

/// **A shift says when somebody is scheduled, and refuses nothing.**
///
/// People cover, swap and stay late. A system telling a manager she cannot ask
/// somebody to stay is not a rule it gets to make — unlike a lapsed iqama,
/// where the law does.
#[tokio::test]
async fn a_shift_says_when_somebody_is_in_and_stops_nothing() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    // Nothing recorded: available whenever asked, because a business that has
    // not written anybody's shifts down must not find its rota empty.
    assert!(
        fixture
            .working("EMP-1", span("2026-05-04T06:00", "2026-05-04T07:00"))
            .await,
        "somebody with no pattern recorded was reported as not working"
    );

    // Monday to Friday, 09:00 to 17:00 local — and local is Riyadh, +03:00, so
    // 09:00 there is 06:00 UTC. The offset is the tenant's clock and it is why
    // these two numbers differ.
    hr::record_shifts(
        &fixture.db,
        &code("EMP-1"),
        &[weekdays(&[1, 2, 3, 4, 5], 9 * 60, 17 * 60)],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    // 2026-05-04 is a Monday.
    assert!(
        fixture
            .working("EMP-1", span("2026-05-04T06:00", "2026-05-04T07:00"))
            .await,
        "a Monday morning was not inside a Monday-to-Friday shift"
    );
    assert!(
        !fixture
            .working("EMP-1", span("2026-05-04T20:00", "2026-05-04T21:00"))
            .await,
        "eleven at night was inside a nine-to-five"
    );
    // 2026-05-09 is a Saturday.
    assert!(
        !fixture
            .working("EMP-1", span("2026-05-09T06:00", "2026-05-09T07:00"))
            .await,
        "a Saturday was inside a Monday-to-Friday shift"
    );

    // **And none of that stops them being rostered.** `may_work_on` is what
    // refuses, and it does not consult the pattern.
    let today = chrono::Utc::now().date_naive();
    assert!(
        fixture.may_work("EMP-1", &today.to_string()).await,
        "a shift pattern was allowed to refuse somebody"
    );

    fixture.cleanup().await;
}

/// The same pattern again writes nothing, and the read says whether one is
/// recorded at all — which is what stops `[]` being read as "never works".
#[tokio::test]
async fn a_rota_says_whether_a_pattern_was_recorded() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let none = hr::shifts(&mut conn, "EMP-1").await.expect("reads");
    drop(conn);
    assert!(none.is_empty(), "a pattern appeared from nowhere");

    let pattern = [weekdays(&[6, 7], 10 * 60, 14 * 60)];
    hr::record_shifts(
        &fixture.db,
        &code("EMP-1"),
        &pattern,
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    let again = hr::record_shifts(
        &fixture.db,
        &code("EMP-1"),
        &pattern,
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("a retry is not an error");
    assert!(again.at.is_none(), "the same pattern wrote a second event");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let stored = hr::shifts(&mut conn, "EMP-1").await.expect("reads");
    drop(conn);
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].weekdays(),
        vec![6, 7],
        "the rule came back saying something else"
    );

    fixture.cleanup().await;
}

/// **The rota and the diary read one clock.** `Calendar` moved to
/// `erp-recurrence` with the rule, and its key is `tenant.calendar` rather than
/// any one module's — a business has one timezone.
#[tokio::test]
async fn the_rota_reads_the_tenant_clock_and_not_a_module_s() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;
    hr::record_shifts(
        &fixture.db,
        &code("EMP-1"),
        &[weekdays(&[1, 2, 3, 4, 5], 9 * 60, 17 * 60)],
        on("2026-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    // Riyadh by default: 09:00 local is 06:00 UTC.
    assert!(
        fixture
            .working("EMP-1", span("2026-05-04T06:00", "2026-05-04T07:00"))
            .await
    );

    // Move the business to UTC and the same instant is 06:00 local, before the
    // shift opens.
    let mut conn = fixture.db.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        erp_recurrence::Calendar::KEY,
        &erp_recurrence::Calendar::try_from(0).expect("UTC is an offset"),
        None,
    )
    .await
    .expect("stores");
    drop(conn);

    assert!(
        !fixture
            .working("EMP-1", span("2026-05-04T06:00", "2026-05-04T07:00"))
            .await,
        "the rota did not read the tenant's clock"
    );

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// 9a — attendance and leave
// ---------------------------------------------------------------------------

fn date(s: &str) -> chrono::NaiveDate {
    s.parse().expect("a valid date")
}

/// **A day is recorded whole, and recording it again corrects it.**
///
/// Not a clock-in and a clock-out: a half-recorded day is somebody who forgot,
/// somebody who left early, or a device that lost power, and nothing can tell
/// which.
#[tokio::test]
async fn a_day_is_recorded_whole_and_corrected_in_place() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    hr::record_day(
        &fixture.db,
        &code("EMP-1"),
        date("2026-05-04"),
        8 * 60,
        "",
        on("2026-05-04"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    // The same day, the same minutes: nothing happens.
    let again = hr::record_day(
        &fixture.db,
        &code("EMP-1"),
        date("2026-05-04"),
        8 * 60,
        "",
        on("2026-05-04"),
        &Metadata::default(),
    )
    .await
    .expect("a retry is not an error");
    assert!(again.at.is_none(), "the same day wrote a second event");

    // The same day, different minutes: a correction, and the timesheet takes
    // the latest word rather than showing two rows.
    hr::record_day(
        &fixture.db,
        &code("EMP-1"),
        date("2026-05-04"),
        9 * 60,
        "بقيت ساعة إضافية",
        on("2026-05-05"),
        &Metadata::default(),
    )
    .await
    .expect("corrected");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");
    let sheet = hr::worked(&mut conn, "EMP-1", date("2026-05-01"), date("2026-05-31"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(sheet.len(), 1, "a correction left the old day behind");
    assert_eq!(sheet[0].minutes, 9 * 60);
    assert_eq!(sheet[0].note, "بقيت ساعة إضافية");

    fixture.cleanup().await;
}

/// **Zero minutes is a fact, and no record at all is a different one.**
///
/// A business that marks somebody absent has said something; a day nobody
/// touched has not.
#[tokio::test]
async fn a_recorded_absence_is_not_the_same_as_no_record() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    hr::record_day(
        &fixture.db,
        &code("EMP-1"),
        date("2026-05-04"),
        0,
        "لم تحضر",
        on("2026-05-04"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");
    fixture.project().await;

    let mut conn = fixture.db.acquire().await.expect("connection");
    let sheet = hr::worked(&mut conn, "EMP-1", date("2026-05-01"), date("2026-05-31"))
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(
        sheet.len(),
        1,
        "a deliberate absence was indistinguishable from silence"
    );
    assert_eq!(sheet[0].minutes, 0);

    fixture.cleanup().await;
}

/// A timesheet that says twenty-six hours is a typo, and one that says six
/// hundred is a broken import. Both are better refused than paid.
#[tokio::test]
async fn a_day_longer_than_a_day_is_refused() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    let error = hr::record_day(
        &fixture.db,
        &code("EMP-1"),
        date("2026-05-04"),
        26 * 60,
        "",
        on("2026-05-04"),
        &Metadata::default(),
    )
    .await
    .expect_err("twenty-six hours was accepted");
    assert!(format!("{error:?}").contains("NotADayOfWork"), "{error:?}");

    fixture.cleanup().await;
}

/// **Leave is inclusive at both ends**, so the 3rd to the 5th is three days —
/// and the count is stored rather than recomputed, because an inclusive range
/// is exactly the arithmetic somebody gets wrong by one.
#[tokio::test]
async fn leave_counts_both_ends_and_is_found_by_any_day_it_covers() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    hr::record_leave(
        &fixture.db,
        &code("EMP-1"),
        hr::Leave::Annual,
        date("2026-06-03"),
        date("2026-06-05"),
        "إجازة",
        on("2026-05-20"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    // A fortnight that starts in March and runs into April.
    hr::record_leave(
        &fixture.db,
        &code("EMP-1"),
        hr::Leave::Unpaid,
        date("2026-03-25"),
        date("2026-04-07"),
        "بدون راتب",
        on("2026-03-01"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    fixture.project().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    let june = hr::leave(&mut conn, "EMP-1", date("2026-06-01"), date("2026-06-30"))
        .await
        .expect("reads");
    assert_eq!(june.len(), 1);
    assert_eq!(june[0].days, 3, "the 3rd to the 5th is three days");

    // **Touching, not starting in.** April has no leave beginning in it, and
    // somebody is still away for the first week.
    let april = hr::leave(&mut conn, "EMP-1", date("2026-04-01"), date("2026-04-30"))
        .await
        .expect("reads");
    assert_eq!(
        april.len(),
        1,
        "a rota for April showed somebody who is on a beach"
    );
    assert_eq!(april[0].kind, "unpaid");
    assert_eq!(april[0].days, 14);

    // And the balance half: what has gone, per kind.
    let taken = hr::leave_taken(&mut conn, "EMP-1", date("2026-01-01"), date("2026-12-31"))
        .await
        .expect("reads");
    drop(conn);
    assert_eq!(
        taken,
        vec![("annual".to_owned(), 3), ("unpaid".to_owned(), 14)],
        "the days taken per kind are what a balance is drawn down by"
    );

    fixture.cleanup().await;
}

/// Leave that ends before it starts is refused rather than stored as a
/// negative count.
#[tokio::test]
async fn backwards_leave_is_refused() {
    let fixture = Fixture::new().await;
    fixture.hire("EMP-1", "سارة", None, None).await;

    let error = hr::record_leave(
        &fixture.db,
        &code("EMP-1"),
        hr::Leave::Annual,
        date("2026-06-05"),
        date("2026-06-03"),
        "",
        on("2026-05-20"),
        &Metadata::default(),
    )
    .await
    .expect_err("backwards leave was accepted");
    assert!(format!("{error:?}").contains("BackwardsLeave"), "{error:?}");

    fixture.cleanup().await;
}
