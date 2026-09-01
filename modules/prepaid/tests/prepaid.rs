//! What customers have already paid for, against a real tenant.
//!
//! The test that carries this file is [`a_liability_agrees_with_the_ledger`].
//! Everything else here is a rule that can be reasoned about; that one is the
//! canary, and it is the same class of check as `ledger::imbalances` — if the
//! sum of what customers are owed and the deferred revenue account disagree,
//! the pipeline is broken rather than the arithmetic.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, CurrencyCode, Money, Timestamp};
use prepaid::{
    Card, Earning, Grant, Mechanic, PointsRedemption, PrepaidError, Reason, Redemption, Scheme,
    Term, Tier, cancel_subscription, earn, expire, expire_points, freeze, grant, open_card,
    recognise_through, redeem, redeem_points, renew_subscription, resume, revoke,
    start_subscription,
};

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
            .register_tenant_on("gym", "Gym", "primary", Actor::system())
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
        for (install, group) in [("crm", "crm"), ("ledger", "ledger"), ("prepaid", "prepaid")] {
            let _ = (install, group);
        }
        crm::install(&mut conn).await.expect("crm installs");
        ensure_group_schema::<crm::Crm>(&mut conn)
            .await
            .expect("crm checkpoint");
        ledger::install(&mut conn).await.expect("ledger installs");
        ensure_group_schema::<ledger::Ledger>(&mut conn)
            .await
            .expect("ledger checkpoint");
        prepaid::install(&mut conn).await.expect("prepaid installs");
        ensure_group_schema::<prepaid::Prepaid>(&mut conn)
            .await
            .expect("prepaid checkpoint");
        drop(conn);

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

        // A chart to post into, and somebody to sell to.
        ledger::install_chart(
            &fixture.db,
            ledger::chart("services").expect("the services chart ships"),
            sar(),
            erp_i18n::Locale::English,
            &Metadata::default(),
        )
        .await
        .expect("the chart installs");

        crm::register_customer(
            &fixture.db,
            &code("CUST-1"),
            &crm::Details {
                name: "سارة".to_owned(),
                name_latin: None,
                kind: crm::CustomerKind::Person,
                contact: crm::Contact {
                    phone: Some("+966511111111".to_owned()),
                    email: None,
                },
                address: None,
                tax: None,
            },
            on("2026-01-01"),
            &Metadata::default(),
        )
        .await
        .expect("the customer is on file");

        fixture
    }

    async fn project(&self) {
        let owned = prepaid::projections();
        let refs: Vec<&dyn Projection<Group = prepaid::Prepaid>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<prepaid::Prepaid>(&self.pool, &refs, prepaid::upcasters(), 200)
            .await
            .expect("prepaid projects");

        let owned = ledger::projections();
        let refs: Vec<&dyn Projection<Group = ledger::Ledger>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<ledger::Ledger>(&self.pool, &refs, ledger::upcasters(), 200)
            .await
            .expect("ledger projects");
    }

    /// What the books say is held in deferred revenue.
    async fn deferred(&self) -> Money {
        let mut conn = self.pool.acquire().await.expect("connection");
        let balances = ledger::account_balances(&mut conn)
            .await
            .expect("the ledger answers");
        // A liability grows by credit, which is a negative balance in this
        // ledger's sign convention. Negated here so the two numbers being
        // compared are both "what is owed", which is what the assertion means.
        balances
            .iter()
            .find(|b| b.code == "2400")
            .and_then(|b| b.balance.checked_neg().ok())
            .unwrap_or_else(|| money(0))
    }

    /// What this module says customers are owed.
    async fn owed(&self) -> Money {
        let mut conn = self.pool.acquire().await.expect("connection");
        prepaid::outstanding(&mut conn)
            .await
            .expect("the read answers")
            .first()
            .copied()
            .unwrap_or_else(|| money(0))
    }

    async fn held(&self, id: &str) -> Option<prepaid::EntitlementSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        prepaid::entitlement(&mut conn, id).await.expect("reads")
    }

    /// A point per riyal, worth ten halalas, and a rank at five hundred.
    ///
    /// Configured rather than defaulted because there is no default: what a
    /// point is worth is a business decision, and `earn` refuses without one.
    async fn scheme(&self) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            Scheme::KEY,
            &Scheme {
                worth: money(10),
                rate_bp: 10_000,
                tiers: vec![Tier {
                    name: "ذهبي".to_owned(),
                    from: 500,
                    rate_bp: 15_000,
                }],
            },
            None,
        )
        .await
        .expect("the scheme is set");
    }

    async fn card(&self, id: &str) -> Option<prepaid::CardSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        prepaid::card(&mut conn, id).await.expect("reads")
    }

    async fn term(&self, id: &str) -> Option<prepaid::SubscriptionSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        prepaid::subscription(&mut conn, id).await.expect("reads")
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **The canary, asserted.** What the books hold against what customers are
/// owed, by two routes that share nothing but the log.
async fn agrees(fixture: &Fixture, note: &str) {
    let deferred = fixture.deferred().await;
    let owed = fixture.owed().await;
    assert_eq!(
        deferred, owed,
        "{note}: the books and the balances disagree"
    );
}

fn rejection(error: &CommandError<PrepaidError>) -> Option<&PrepaidError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

/// A ten-session package for 300, bought.
fn package(uses: u32, value: i64) -> Grant {
    Grant {
        customer: code("CUST-1"),
        what: "قص".to_owned(),
        uses: Some(uses),
        value: money(value),
        reason: Reason::Bought,
        against: None,
        expires_at: None,
        at: on("2026-01-02"),
    }
}

/// **A package recognises per session, and the last one takes the remainder.**
///
/// Three sessions of a 100 riyal package is 33.33 each, and three of those is
/// 99.99 — a halala stranded in a liability account for ever, on a package the
/// customer has finished. The value of one use is recomputed each time, so the
/// last is worth whatever is left.
#[tokio::test]
async fn a_package_recognises_per_session_and_closes_at_exactly_zero() {
    let fixture = Fixture::new().await;

    grant(
        &fixture.db,
        &code("PKG-1"),
        &package(3, 10_000),
        &Metadata::default(),
    )
    .await
    .expect("granted");
    fixture.project().await;
    assert_eq!(fixture.deferred().await, money(10_000));
    assert_eq!(fixture.owed().await, money(10_000));

    let mut recognised = money(0);
    // 10,000 over three: 3,333 then 3,334 then the remaining 3,333. The middle
    // one is larger because each use is worth what is *left* over what is left
    // to use — 6,667 over two is 3,333.50, which rounds away from zero.
    for (n, expected_left) in [(1, 6_667), (2, 3_333), (3, 0)] {
        redeem(
            &fixture.db,
            &code("PKG-1"),
            &Redemption {
                reference: format!("VISIT-{n}"),
                uses: 1,
                at: on("2026-02-01"),
            },
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("session {n}: {e}"));
        fixture.project().await;

        let held = fixture.held("PKG-1").await.expect("there");
        assert_eq!(
            held.outstanding,
            money(expected_left),
            "after session {n} the balance is wrong"
        );
        recognised = held.deferred.checked_sub(held.outstanding).expect("valid");
    }

    assert_eq!(recognised, money(10_000), "the package did not fully earn");
    assert_eq!(
        fixture.deferred().await,
        money(0),
        "a halala was stranded in deferred revenue"
    );
    assert_eq!(
        fixture
            .held("PKG-1")
            .await
            .expect("there")
            .closed
            .as_deref(),
        Some("spent")
    );

    fixture.cleanup().await;
}

/// **A subscription recognises ratably, attended or not.**
///
/// A year at 1,200 is a hundred a month whether the member comes or not, and
/// the last day brings it to exactly the price.
#[tokio::test]
async fn a_subscription_recognises_over_the_term_whether_or_not_anybody_comes() {
    let fixture = Fixture::new().await;

    start_subscription(
        &fixture.db,
        &code("SUB-1"),
        &Term {
            customer: code("CUST-1"),
            plan: "سنوي".to_owned(),
            price: money(120_000),
            from: on("2026-01-01"),
            until: on("2027-01-01"),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("started");
    fixture.project().await;
    assert_eq!(fixture.deferred().await, money(120_000));

    // A quarter of the year, and nobody has been through the door.
    recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("recognised");
    fixture.project().await;
    let term = fixture.term("SUB-1").await.expect("there");
    // 1 January to 2 April is 91 days; 2026 is not a leap year.
    assert_eq!(term.recognised, money(29_918), "91 of 365 days at 1,200");
    assert_eq!(term.outstanding, money(90_082));

    // **Running it again posts nothing.** The month-end job is safe to repeat.
    let again = recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("quiet");
    assert!(again.at.is_none(), "a repeat recognised something twice");

    // The end of the term brings it to exactly the price.
    recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2027-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("recognised");
    fixture.project().await;
    let term = fixture.term("SUB-1").await.expect("there");
    assert_eq!(term.recognised, money(120_000));
    assert_eq!(term.outstanding, money(0));
    assert_eq!(fixture.deferred().await, money(0));

    fixture.cleanup().await;
}

/// **A freeze stops the earning and moves the term.**
///
/// The exit criterion for this phase: a gym sells a frozen-then-resumed annual
/// membership and it reconciles.
#[tokio::test]
async fn a_frozen_membership_earns_nothing_and_comes_back_owing_the_same() {
    let fixture = Fixture::new().await;

    start_subscription(
        &fixture.db,
        &code("SUB-1"),
        &Term {
            customer: code("CUST-1"),
            plan: "سنوي".to_owned(),
            price: money(120_000),
            from: on("2026-01-01"),
            until: on("2027-01-01"),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("started");

    // Frozen on the first of April, which earns the quarter and stops the clock.
    freeze(
        &fixture.db,
        &code("SUB-1"),
        "سفر",
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("frozen");
    fixture.project().await;
    let frozen = fixture.term("SUB-1").await.expect("there");
    assert_eq!(frozen.recognised, money(29_589), "90 of 365 days");
    assert!(frozen.frozen_since.is_some());

    // A month away earns nothing.
    recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2026-05-01"),
        &Metadata::default(),
    )
    .await
    .expect("quiet");
    fixture.project().await;
    assert_eq!(
        fixture.term("SUB-1").await.expect("there").recognised,
        money(29_589),
        "a frozen month earned revenue"
    );

    // Back on the first of May: the term moves out by exactly the thirty days.
    resume(
        &fixture.db,
        &code("SUB-1"),
        on("2026-05-01"),
        &Metadata::default(),
    )
    .await
    .expect("resumed");
    fixture.project().await;
    let live = fixture.term("SUB-1").await.expect("there");
    assert_eq!(live.ends_at, on("2027-01-31"), "the term did not move");
    assert!(live.frozen_since.is_none());

    // And the new end of the term still brings it to exactly the price.
    recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2027-01-31"),
        &Metadata::default(),
    )
    .await
    .expect("recognised");
    fixture.project().await;
    assert_eq!(
        fixture.term("SUB-1").await.expect("there").outstanding,
        money(0)
    );
    assert_eq!(fixture.deferred().await, money(0));

    fixture.cleanup().await;
}

/// **A coupon is a discount, not a liability.**
///
/// That system has a full coupon model and no coupon liability account, which
/// is correct and worth not undoing. Nothing was received, so nothing is
/// deferred, nothing posts, and delivering it recognises nothing.
#[tokio::test]
async fn a_grant_nobody_paid_for_creates_no_liability() {
    let fixture = Fixture::new().await;

    let refused = grant(
        &fixture.db,
        &code("FREE-1"),
        &Grant {
            reason: Reason::FreeFromCoupon,
            ..package(1, 5_000)
        },
        &Metadata::default(),
    )
    .await
    .expect_err("a coupon with a value is a misunderstanding");
    assert!(matches!(
        rejection(&refused),
        Some(PrepaidError::FreeGrantWithValue)
    ));

    grant(
        &fixture.db,
        &code("FREE-1"),
        &Grant {
            reason: Reason::GrantedByBusiness,
            value: money(0),
            ..package(1, 0)
        },
        &Metadata::default(),
    )
    .await
    .expect("a goodwill session");
    fixture.project().await;
    assert_eq!(
        fixture.deferred().await,
        money(0),
        "a grant nobody paid for created a liability"
    );

    redeem(
        &fixture.db,
        &code("FREE-1"),
        &Redemption {
            reference: "VISIT-1".to_owned(),
            uses: 1,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("delivered");
    fixture.project().await;
    assert_eq!(fixture.deferred().await, money(0));
    assert_eq!(
        fixture
            .held("FREE-1")
            .await
            .expect("there")
            .closed
            .as_deref(),
        Some("spent")
    );

    fixture.cleanup().await;
}

/// **Breakage is revenue, and a revocation is not.**
///
/// Both clear the liability and both post the same entry, because a ledger sees
/// debits and credits. What tells them apart is the event, and the refund that
/// follows a revocation is a credit note in `sales`.
#[tokio::test]
async fn what_lapses_is_earned_and_what_is_taken_back_is_not() {
    let fixture = Fixture::new().await;

    for (id, expires) in [("PKG-1", Some(on("2026-06-01"))), ("PKG-2", None)] {
        grant(
            &fixture.db,
            &code(id),
            &Grant {
                expires_at: expires,
                ..package(4, 20_000)
            },
            &Metadata::default(),
        )
        .await
        .expect("granted");
    }
    fixture.project().await;
    assert_eq!(fixture.deferred().await, money(40_000));

    // Not yet lapsed, so nothing happens and nothing posts.
    let early = expire(
        &fixture.db,
        &code("PKG-1"),
        on("2026-05-01"),
        &Metadata::default(),
    )
    .await
    .expect("quiet");
    assert!(early.at.is_none(), "it expired before its date");

    expire(
        &fixture.db,
        &code("PKG-1"),
        on("2026-06-01"),
        &Metadata::default(),
    )
    .await
    .expect("lapsed");
    revoke(
        &fixture.db,
        &code("PKG-2"),
        "استرداد",
        on("2026-06-02"),
        &Metadata::default(),
    )
    .await
    .expect("taken back");
    fixture.project().await;

    assert_eq!(fixture.deferred().await, money(0));
    assert_eq!(fixture.owed().await, money(0));
    assert_eq!(
        fixture
            .held("PKG-1")
            .await
            .expect("there")
            .closed
            .as_deref(),
        Some("expired")
    );
    assert_eq!(
        fixture
            .held("PKG-2")
            .await
            .expect("there")
            .closed
            .as_deref(),
        Some("revoked")
    );

    // And a lapsed package takes no more sessions.
    let refused = redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-9".to_owned(),
            uses: 1,
            at: on("2026-07-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect_err("it is over");
    assert!(matches!(
        rejection(&refused),
        Some(PrepaidError::NotLive(_))
    ));

    fixture.cleanup().await;
}

/// **Redeeming the same reference twice takes nothing twice.**
#[tokio::test]
async fn a_retried_redemption_is_harmless() {
    let fixture = Fixture::new().await;
    grant(
        &fixture.db,
        &code("PKG-1"),
        &package(10, 30_000),
        &Metadata::default(),
    )
    .await
    .expect("granted");

    let first = redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-1".to_owned(),
            uses: 1,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("delivered");
    assert!(first.at.is_some());

    let again = redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-1".to_owned(),
            uses: 1,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("quiet");
    assert!(again.at.is_none(), "the retry drew the package down twice");

    fixture.project().await;
    assert_eq!(fixture.deferred().await, money(27_000));
    assert_eq!(
        fixture.held("PKG-1").await.expect("there").uses_left,
        Some(9)
    );

    // And asking for more than is left is refused rather than overdrawn.
    let refused = redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-BULK".to_owned(),
            uses: 20,
            at: on("2026-02-02"),
        },
        &Metadata::default(),
    )
    .await
    .expect_err("nine left");
    assert!(matches!(
        rejection(&refused),
        Some(PrepaidError::NothingLeft { .. })
    ));

    fixture.cleanup().await;
}

/// **A renewal earns the whole of the term that ended.**
///
/// A term that ended is a term that was delivered, however few times the member
/// turned up, so leaving a remainder in the liability would understate revenue
/// for ever.
#[tokio::test]
async fn a_renewal_earns_the_whole_of_the_term_that_ended() {
    let fixture = Fixture::new().await;
    start_subscription(
        &fixture.db,
        &code("SUB-1"),
        &Term {
            customer: code("CUST-1"),
            plan: "شهري".to_owned(),
            price: money(20_000),
            from: on("2026-01-01"),
            until: on("2026-02-01"),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("started");

    // Renewing before the term is over is refused.
    let refused = renew_subscription(
        &fixture.db,
        &code("SUB-1"),
        money(22_000),
        on("2026-03-01"),
        on("2026-01-15"),
        &Metadata::default(),
    )
    .await
    .expect_err("the term is still running");
    assert!(matches!(
        rejection(&refused),
        Some(PrepaidError::TermNotOver { .. })
    ));

    renew_subscription(
        &fixture.db,
        &code("SUB-1"),
        money(22_000),
        on("2026-03-01"),
        on("2026-02-01"),
        &Metadata::default(),
    )
    .await
    .expect("renewed");
    fixture.project().await;

    let term = fixture.term("SUB-1").await.expect("there");
    assert_eq!(term.price, money(22_000), "the new price");
    assert_eq!(term.recognised, money(0), "the new term has earned nothing");
    assert_eq!(term.outstanding, money(22_000));
    // The old term's 200 was earned; the new term's 220 is deferred.
    assert_eq!(fixture.deferred().await, money(22_000));

    fixture.cleanup().await;
}

/// Two sessions off the package, the deposit applied, and half a year of the
/// membership earned. Split out only because the canary was one function of a
/// hundred and thirty lines.
async fn drawn_down_every_way(fixture: &Fixture) {
    redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-1".to_owned(),
            uses: 2,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("two sessions");
    redeem(
        &fixture.db,
        &code("DEP-1"),
        &Redemption {
            reference: "ARRIVED".to_owned(),
            uses: 1,
            at: on("2026-02-05"),
        },
        &Metadata::default(),
    )
    .await
    .expect("the deposit is applied");
    recognise_through(
        &fixture.db,
        &code("SUB-1"),
        on("2026-07-01"),
        &Metadata::default(),
    )
    .await
    .expect("half a year");
    fixture.project().await;
    agrees(fixture, "after redemptions and recognition").await;
}

/// **The canary.**
///
/// The sum of what every customer is owed, against the deferred revenue account
/// in the books. Two numbers built by two different routes — one from this
/// module's projection, one from the ledger's — and they have to agree after
/// every kind of movement this module can make.
///
/// The comparison lives here rather than in the module because it needs both
/// `proj_prepaid` and `proj_ledger`, and L3 forbids a module from reading
/// across projection groups. `prepaid::outstanding` is one half of it; this is
/// the other.
#[tokio::test]
async fn a_liability_agrees_with_the_ledger() {
    let fixture = Fixture::new().await;

    grant(
        &fixture.db,
        &code("PKG-1"),
        &package(7, 70_000),
        &Metadata::default(),
    )
    .await
    .expect("granted");
    grant(
        &fixture.db,
        &code("DEP-1"),
        &Grant {
            what: "عربون".to_owned(),
            uses: None,
            value: money(15_000),
            against: Some(code("BK-0001")),
            ..package(1, 15_000)
        },
        &Metadata::default(),
    )
    .await
    .expect("a deposit");
    start_subscription(
        &fixture.db,
        &code("SUB-1"),
        &Term {
            customer: code("CUST-1"),
            plan: "سنوي".to_owned(),
            price: money(120_000),
            from: on("2026-01-01"),
            until: on("2027-01-01"),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("started");

    fixture.project().await;
    agrees(&fixture, "after three sales").await;
    assert_eq!(fixture.owed().await, money(205_000));

    drawn_down_every_way(&fixture).await;

    // A card, which defers a fraction of a sale rather than the whole of one
    // and is the only shape here whose liability is computed from a price it
    // does not hold.
    opened(&fixture, "CARD-1", Mechanic::Points).await;
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 10_000, "2026-06-02"),
        &Metadata::default(),
    )
    .await
    .expect("earned");
    redeem_points(
        &fixture.db,
        &code("CARD-1"),
        &PointsRedemption {
            reference: "RED-1".to_owned(),
            count: 30,
            toward: None,
            at: on("2026-07-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("spent");
    fixture.project().await;
    agrees(&fixture, "after points earned and spent").await;

    // A renewal, which moves value in both directions at once and is where
    // this canary was weakest: it posted the release and not the deferral, and
    // the read model carried a liability the books did not.
    renew_subscription(
        &fixture.db,
        &code("SUB-1"),
        money(130_000),
        on("2028-01-01"),
        on("2027-01-01"),
        &Metadata::default(),
    )
    .await
    .expect("renewed");
    fixture.project().await;
    agrees(&fixture, "after a renewal").await;

    ended_every_way(&fixture).await;

    // The cancelled subscription still owes the rest of its term, because the
    // business still owes it. What happens to it is decided elsewhere.
    let term = fixture.term("SUB-1").await.expect("there");
    assert!(
        term.outstanding.is_positive(),
        "a mid-term cancellation left nothing owed"
    );

    fixture.cleanup().await;
}

/// A freeze, a resume, a revocation and a cancellation. Split out for the same
/// reason `drawn_down_every_way` is.
async fn ended_every_way(fixture: &Fixture) {
    freeze(
        &fixture.db,
        &code("SUB-1"),
        "إصابة",
        on("2027-08-01"),
        &Metadata::default(),
    )
    .await
    .expect("frozen");
    resume(
        &fixture.db,
        &code("SUB-1"),
        on("2027-09-01"),
        &Metadata::default(),
    )
    .await
    .expect("resumed");
    revoke(
        &fixture.db,
        &code("PKG-1"),
        "استرداد",
        on("2027-09-02"),
        &Metadata::default(),
    )
    .await
    .expect("taken back");
    cancel_subscription(
        &fixture.db,
        &code("SUB-1"),
        "انتقل",
        on("2027-10-01"),
        &Metadata::default(),
    )
    .await
    .expect("cancelled");
    expire_points(
        &fixture.db,
        &code("CARD-1"),
        on("2027-10-02"),
        &Metadata::default(),
    )
    .await
    .expect("the rest of the points lapsed");
    fixture.project().await;
    agrees(
        fixture,
        "after a freeze, a resume, a revocation, a cancellation and breakage",
    )
    .await;
}

/// **A balance belongs to somebody**, checked against the log so a customer
/// registered a moment ago can be sold to immediately.
#[tokio::test]
async fn a_grant_to_a_customer_who_is_not_there_is_refused() {
    let fixture = Fixture::new().await;

    let refused = grant(
        &fixture.db,
        &code("PKG-1"),
        &Grant {
            customer: code("CUST-NOBODY"),
            ..package(1, 5_000)
        },
        &Metadata::default(),
    )
    .await
    .expect_err("there is no such customer");
    assert!(matches!(
        rejection(&refused),
        Some(PrepaidError::NoSuchCustomer(_))
    ));

    fixture.project().await;
    assert_eq!(
        fixture.deferred().await,
        money(0),
        "a refused grant posted to the ledger"
    );

    fixture.cleanup().await;
}

/// **An open-value gift card is refused, and a deposit is not.**
///
/// A card spendable on anything is a multi-purpose voucher: what it buys is not
/// known when it is sold, so neither is the rate it should have been taxed at.
/// The two differ by one field — a deposit names the booking it secures — and
/// that field is the whole of what keeps this module out of tax.
#[tokio::test]
async fn an_amount_that_names_no_purpose_is_refused() {
    let fixture = Fixture::new().await;

    let refused = grant(
        &fixture.db,
        &code("CARD-1"),
        &Grant {
            what: "رصيد".to_owned(),
            uses: None,
            against: None,
            ..package(1, 50_000)
        },
        &Metadata::default(),
    )
    .await
    .expect_err("a card spendable on anything");
    assert!(matches!(rejection(&refused), Some(PrepaidError::OpenValue)));

    // The same amount, against a booking, is a deposit and is allowed.
    grant(
        &fixture.db,
        &code("DEP-9"),
        &Grant {
            what: "عربون".to_owned(),
            uses: None,
            against: Some(code("BK-0009")),
            ..package(1, 50_000)
        },
        &Metadata::default(),
    )
    .await
    .expect("a deposit names what it is held against");

    fixture.project().await;
    assert_eq!(
        fixture.deferred().await,
        money(50_000),
        "the refused card deferred value, or the deposit did not"
    );

    fixture.cleanup().await;
}

/// A card and a customer, ready to earn on.
async fn opened(fixture: &Fixture, id: &str, mechanic: Mechanic) {
    fixture.scheme().await;
    open_card(
        &fixture.db,
        &code(id),
        &Card {
            customer: code("CUST-1"),
            mechanic,
            at: on("2026-01-02"),
        },
        &Metadata::default(),
    )
    .await
    .expect("the card opens");
}

fn earning(reference: &str, spend: i64, at: &str) -> Earning {
    Earning {
        reference: reference.to_owned(),
        spend: money(spend),
        count: None,
        from: Some(code("INV-0001")),
        at: on(at),
    }
}

/// **The allocation is a fraction of the sale, not the reward's value.**
///
/// A hundred riyals awarding a hundred points worth ten halalas each defers
/// 9.09 and not 10: IFRS 15 splits the price the customer actually paid between
/// the goods and the points by their standalone prices, and the points' share
/// of 110 riyals of standalone value is ten elevenths of a riyal short of ten.
/// The SMB shortcut would accrue the whole ten, overstate the liability and
/// charge the difference to expense. Saudi requires IFRS, so this is the only
/// treatment here.
#[tokio::test]
async fn points_defer_a_fraction_of_the_sale_and_not_the_reward() {
    let fixture = Fixture::new().await;
    opened(&fixture, "CARD-1", Mechanic::Points).await;

    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 10_000, "2026-01-03"),
        &Metadata::default(),
    )
    .await
    .expect("earned");

    fixture.project().await;
    let card = fixture.card("CARD-1").await.expect("there");
    assert_eq!(card.counts, 100, "a point per riyal on a hundred riyals");
    assert_eq!(card.lifetime, 100);
    assert_eq!(card.deferred, Some(money(909)), "9.09 and not 10.00");
    agrees(&fixture, "after an earning").await;

    // Half of them, spent. Each point is worth what is left divided by what is
    // left to spend, which is the drawdown a package already uses.
    redeem_points(
        &fixture.db,
        &code("CARD-1"),
        &PointsRedemption {
            reference: "RED-1".to_owned(),
            count: 50,
            toward: Some(code("INV-0002")),
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("spent");

    fixture.project().await;
    let card = fixture.card("CARD-1").await.expect("there");
    assert_eq!(card.counts, 50);
    assert_eq!(card.lifetime, 100, "spending points cost a rank");
    assert_eq!(card.deferred, Some(money(459)), "909 less the 450 honoured");
    agrees(&fixture, "after a redemption").await;

    fixture.cleanup().await;
}

/// **Breakage is revenue, and the card survives it.**
///
/// A balance running out is not the end of the card, which is the difference
/// between this and an entitlement expiring. The rank survives too: it is what
/// was earned, and it was earned.
#[tokio::test]
async fn points_that_lapse_are_earned_and_the_card_lives_on() {
    let fixture = Fixture::new().await;
    opened(&fixture, "CARD-1", Mechanic::Points).await;

    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 10_000, "2026-01-03"),
        &Metadata::default(),
    )
    .await
    .expect("earned");
    expire_points(
        &fixture.db,
        &code("CARD-1"),
        on("2027-01-03"),
        &Metadata::default(),
    )
    .await
    .expect("lapsed");

    fixture.project().await;
    let card = fixture
        .card("CARD-1")
        .await
        .expect("the card is still there");
    assert_eq!(card.counts, 0);
    assert_eq!(card.deferred, Some(money(0)), "the liability is gone");
    assert_eq!(card.lifetime, 100, "breakage cost a rank");
    assert_eq!(
        fixture.owed().await,
        money(0),
        "nothing is owed once the points have gone"
    );
    agrees(&fixture, "after breakage").await;

    // And it can earn again, which is what "the card survives" means.
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-2", 10_000, "2027-02-01"),
        &Metadata::default(),
    )
    .await
    .expect("earned again");
    fixture.project().await;
    assert_eq!(fixture.card("CARD-1").await.expect("there").counts, 100);
    agrees(&fixture, "after earning on a lapsed card").await;

    fixture.cleanup().await;
}

/// **A rank is reached by lifetime count and changes the rate.**
///
/// The movement that crosses a threshold earns at the old rate and the next one
/// at the new: any other reading makes the award depend on itself. Spending
/// does not cost the rank, which is the whole reason `lifetime` is separate
/// from the balance.
#[tokio::test]
async fn a_rank_changes_the_rate_and_spending_does_not_cost_it() {
    let fixture = Fixture::new().await;
    opened(&fixture, "CARD-1", Mechanic::Points).await;

    // Six hundred riyals at the base rate. It crosses five hundred, and earns
    // at the rate that applied before it did.
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 60_000, "2026-01-03"),
        &Metadata::default(),
    )
    .await
    .expect("earned");
    // A hundred riyals at the rank it has now reached: one and a half a riyal.
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-2", 10_000, "2026-01-04"),
        &Metadata::default(),
    )
    .await
    .expect("earned at the rank");

    fixture.project().await;
    let card = fixture.card("CARD-1").await.expect("there");
    assert_eq!(card.counts, 750, "600 at one, then 150 at one and a half");
    assert_eq!(card.lifetime, 750);

    redeem_points(
        &fixture.db,
        &code("CARD-1"),
        &PointsRedemption {
            reference: "RED-1".to_owned(),
            count: 700,
            toward: None,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("spent");
    // Down to fifty redeemable, and still gold.
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-3", 10_000, "2026-03-01"),
        &Metadata::default(),
    )
    .await
    .expect("still earning at the rank");

    fixture.project().await;
    let card = fixture.card("CARD-1").await.expect("there");
    assert_eq!(card.counts, 200, "fifty left, and a hundred and fifty more");
    assert_eq!(card.lifetime, 900, "a rank is not spent");
    agrees(&fixture, "after a rank change and a redemption").await;

    fixture.cleanup().await;
}

/// **Stamps and visits count what the caller counts.**
///
/// The three mechanics differ in what produces the count and in nothing after
/// it. A visit that cost nothing is not a contract with a customer, so it
/// defers nothing — the same conclusion `Reason::was_paid_for` reaches about a
/// coupon.
#[tokio::test]
async fn stamps_and_visits_count_what_the_caller_counts() {
    let fixture = Fixture::new().await;
    opened(&fixture, "STAMP-1", Mechanic::Stamps).await;
    open_card(
        &fixture.db,
        &code("VISIT-1"),
        &Card {
            customer: code("CUST-1"),
            mechanic: Mechanic::Visits,
            at: on("2026-01-02"),
        },
        &Metadata::default(),
    )
    .await
    .expect("a visits card");

    // One stamp for one coffee, whatever the scheme's rate says.
    earn(
        &fixture.db,
        &code("STAMP-1"),
        &Earning {
            count: Some(1),
            ..earning("COFFEE-1", 1_500, "2026-01-03")
        },
        &Metadata::default(),
    )
    .await
    .expect("stamped");
    // A visit nobody paid for.
    earn(
        &fixture.db,
        &code("VISIT-1"),
        &Earning {
            count: Some(1),
            spend: money(0),
            ..earning("DOOR-1", 0, "2026-01-03")
        },
        &Metadata::default(),
    )
    .await
    .expect("counted");

    fixture.project().await;
    let stamps = fixture.card("STAMP-1").await.expect("there");
    assert_eq!(stamps.counts, 1, "the rate did not overrule the caller");
    assert_eq!(stamps.deferred, Some(money(10)), "15.00 split with a stamp");

    let visits = fixture.card("VISIT-1").await.expect("there");
    assert_eq!(visits.counts, 1);
    assert_eq!(
        visits.deferred,
        Some(money(0)),
        "a visit nobody paid for deferred something"
    );
    agrees(&fixture, "after a stamp and a visit").await;

    fixture.cleanup().await;
}

/// **A retried till awards nothing twice** (L8), and earning needs a scheme.
#[tokio::test]
async fn a_retried_earning_is_harmless_and_a_scheme_is_required() {
    let fixture = Fixture::new().await;

    // Before any scheme exists. Nothing to allocate against, so nothing runs.
    open_card(
        &fixture.db,
        &code("CARD-1"),
        &Card {
            customer: code("CUST-1"),
            mechanic: Mechanic::Points,
            at: on("2026-01-02"),
        },
        &Metadata::default(),
    )
    .await
    .expect("the card opens without one");
    let refused = earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 10_000, "2026-01-03"),
        &Metadata::default(),
    )
    .await
    .expect_err("no scheme");
    assert!(matches!(rejection(&refused), Some(PrepaidError::NoScheme)));

    fixture.scheme().await;
    for _ in 0..3 {
        earn(
            &fixture.db,
            &code("CARD-1"),
            &earning("INV-1", 10_000, "2026-01-03"),
            &Metadata::default(),
        )
        .await
        .expect("earned");
    }

    fixture.project().await;
    let card = fixture.card("CARD-1").await.expect("there");
    assert_eq!(card.counts, 100, "the same reference earned more than once");
    assert_eq!(card.deferred, Some(money(909)));
    agrees(&fixture, "after three tries at one earning").await;

    fixture.cleanup().await;
}

/// **Everything here is a pure function of the log.**
#[tokio::test]
async fn a_rebuild_reproduces_what_is_held() {
    let fixture = Fixture::new().await;

    grant(
        &fixture.db,
        &code("PKG-1"),
        &package(5, 50_000),
        &Metadata::default(),
    )
    .await
    .expect("granted");
    redeem(
        &fixture.db,
        &code("PKG-1"),
        &Redemption {
            reference: "VISIT-1".to_owned(),
            uses: 1,
            at: on("2026-02-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("delivered");
    start_subscription(
        &fixture.db,
        &code("SUB-1"),
        &Term {
            customer: code("CUST-1"),
            plan: "سنوي".to_owned(),
            price: money(120_000),
            from: on("2026-01-01"),
            until: on("2027-01-01"),
            at: on("2026-01-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("started");
    freeze(
        &fixture.db,
        &code("SUB-1"),
        "سفر",
        on("2026-03-01"),
        &Metadata::default(),
    )
    .await
    .expect("frozen");
    resume(
        &fixture.db,
        &code("SUB-1"),
        on("2026-04-01"),
        &Metadata::default(),
    )
    .await
    .expect("resumed");
    opened(&fixture, "CARD-1", Mechanic::Points).await;
    earn(
        &fixture.db,
        &code("CARD-1"),
        &earning("INV-1", 10_000, "2026-04-02"),
        &Metadata::default(),
    )
    .await
    .expect("earned");
    redeem_points(
        &fixture.db,
        &code("CARD-1"),
        &PointsRedemption {
            reference: "RED-1".to_owned(),
            count: 40,
            toward: None,
            at: on("2026-05-01"),
        },
        &Metadata::default(),
    )
    .await
    .expect("spent");
    expire_points(
        &fixture.db,
        &code("CARD-1"),
        on("2027-05-01"),
        &Metadata::default(),
    )
    .await
    .expect("lapsed");

    fixture.project().await;
    assert!(
        fixture.owed().await.is_positive(),
        "nothing to compare against"
    );

    let owned = prepaid::projections();
    let refs: Vec<&dyn Projection<Group = prepaid::Prepaid>> =
        owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<prepaid::Prepaid>(&fixture.pool, &refs, prepaid::upcasters(), 200)
        .await
        .expect("the shadow replays");
    assert!(
        report.is_reproducible(),
        "a rebuild must reproduce what is held exactly: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Every message this module can produce has a translation in every locale.
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&prepaid::CATALOG);
}
