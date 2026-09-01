//! **Six trades, one engine.**
//!
//! The claim `booking` makes is that a salon, a restaurant, a hotel, a class
//! studio, a gym and a museum are the same code. This file is where that claim
//! is either true or it is not.
//!
//! Each fixture fits the tenant out from a blueprint — no hand-built rota — and
//! then books the one thing that is characteristic of that trade. A stylist
//! takes one person at a time; a table takes covers; a hotel books the type and
//! gives out the room later; a class takes twelve separate people in one hour;
//! a museum sells hundreds of places with nobody assigned to them; and a gym
//! books its classes and not its door.
//!
//! # The rule this file exists to enforce
//!
//! **If a fixture needs a code change, the module is not finished.** Every
//! trade below is a `const` in `src/trades.rs` and nothing in the module reads
//! a trade's id. The day one of these needs a branch, whatever it needed has to
//! be generalised instead.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use booking::{
    BookingError, Draft, DraftLine, Held, Stage, Trade, fit_out, move_to, reserve, trade,
};
use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_i18n::Locale;
use erp_occupancy::Span;
use erp_projection::{Projection, ensure_group_schema, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

/// A local clock time on the one Wednesday every fixture happens on.
///
/// Local, because every timetable in `trades.rs` is. The tenant is at `+03:00`,
/// which is where this ships.
fn at(clock: &str) -> Timestamp {
    let hhmm = if clock.contains(':') {
        clock.to_owned()
    } else {
        format!("{clock}:00")
    };
    format!("2026-09-02T{hhmm}:00+03:00")
        .parse()
        .expect("a valid instant")
}

fn span(from: &str, until: &str) -> Span {
    Span::new(at(from), at(until)).expect("a valid span")
}

/// One line taking one of everything named.
fn line(what: &str, from: &str, until: &str, takes: &[&str]) -> DraftLine {
    DraftLine {
        what: what.to_owned(),
        span: span(from, until),
        takes: takes.iter().map(|r| Held::one(code(r))).collect(),
        charge: None,
    }
}

/// One line taking `quantity` of a single resource.
fn places(what: &str, from: &str, until: &str, resource: &str, quantity: u16) -> DraftLine {
    DraftLine {
        what: what.to_owned(),
        span: span(from, until),
        takes: vec![Held {
            resource: code(resource),
            quantity,
        }],
        charge: None,
    }
}

fn booking_for(customer: &str, lines: Vec<DraftLine>) -> Draft {
    Draft {
        customer: booking::Customer {
            id: Some(code(customer)),
            name: customer.to_owned(),
            phone: None,
        },
        lines,
        note: String::new(),
        at: at("08"),
    }
}

struct Fixture {
    db: TenantDb,
    pool: sqlx::PgPool,
    _control: Arc<ControlPlane>,
    _control_db: TestDb,
    database: String,
}

impl Fixture {
    /// A tenant fitted out for one trade, with four customers on file.
    async fn for_trade(slug: &str, id: &str) -> (Self, &'static Trade) {
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
            .register_tenant_on(slug, slug, "primary", Actor::system())
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
        crm::install(&mut conn).await.expect("crm installs");
        ensure_group_schema::<crm::Crm>(&mut conn)
            .await
            .expect("crm checkpoint");
        booking::install(&mut conn).await.expect("booking installs");
        ensure_group_schema::<booking::Booking>(&mut conn)
            .await
            .expect("booking checkpoint");
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

        let trade = trade(id).unwrap_or_else(|| panic!("no trade called {id}"));
        // **The whole rota, from the blueprint.** Nothing below hand-declares a
        // resource; if a fixture needed one, that is the finding.
        let fitted = fit_out(
            &fixture.db,
            trade,
            Locale::Arabic,
            at("00"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{id} should fit out: {e}"));
        assert_eq!(
            fitted.declared,
            trade.resources.len(),
            "{id} did not declare its whole rota"
        );

        for who in ["c1", "c2", "c3", "c4"] {
            fixture.customer(who).await;
        }
        (fixture, trade)
    }

    async fn customer(&self, id: &str) {
        crm::register_customer(
            &self.db,
            &code(id),
            &crm::Details {
                name: id.to_owned(),
                name_latin: None,
                kind: crm::CustomerKind::Person,
                contact: crm::Contact {
                    phone: Some("+966500000000".to_owned()),
                    email: None,
                },
                address: None,
                tax: None,
            },
            at("00"),
            &Metadata::default(),
        )
        .await
        .expect("the customer is on file");
    }

    async fn book(
        &self,
        id: &str,
        customer: &str,
        lines: Vec<DraftLine>,
    ) -> Result<(), BookingError> {
        match reserve(
            &self.db,
            &code(id),
            &booking_for(customer, lines),
            &Metadata::default(),
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(CommandError::Execute(ExecuteError::Rejected(e))) => Err(e),
            Err(other) => panic!("{id}: {other}"),
        }
    }

    async fn free(&self, resource: &str, from: &str, until: &str) -> u16 {
        let mut conn = self.pool.acquire().await.expect("connection");
        erp_occupancy::free(&mut conn, &code(resource), span(from, until))
            .await
            .expect("the engine answers")
    }

    async fn project(&self) {
        let owned = booking::projections();
        let refs: Vec<&dyn Projection<Group = booking::Booking>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<booking::Booking>(&self.pool, &refs, booking::upcasters(), 200)
            .await
            .expect("booking projects");
    }

    async fn rota(&self) -> Vec<booking::ResourceSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        booking::resources(&mut conn, false, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn diary(&self) -> Vec<booking::ReservationSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        booking::reservations(&mut conn, None, None, None, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// **Salon** — a named person and the chair they work in, one booking each.
///
/// The shape everybody builds a booking system for, and the one that hides how
/// specific it is: two resources on one line, both at capacity one, and the
/// customer is a third.
#[tokio::test]
async fn a_salon_books_a_person_and_the_chair_they_work_in() {
    let (fixture, _) = Fixture::for_trade("salon", "salon").await;

    fixture
        .book(
            "BK-1",
            "c1",
            vec![line("قص", "10", "11", &["stylist-1", "chair-1"])],
        )
        .await
        .expect("the stylist and the chair are both free");

    // The other stylist is free, and so is the other chair.
    fixture
        .book(
            "BK-2",
            "c2",
            vec![line("قص", "10", "11", &["stylist-2", "chair-2"])],
        )
        .await
        .expect("a second stylist in a second chair");

    // But there is no third chair, so the third booking has nowhere to sit even
    // though nobody has said "a salon has two chairs" anywhere in the code.
    let refused = fixture
        .book(
            "BK-3",
            "c3",
            vec![line("قص", "10", "11", &["stylist-1", "chair-1"])],
        )
        .await
        .expect_err("both are taken");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    // And the salon is shut at eight in the morning.
    let refused = fixture
        .book(
            "BK-4",
            "c4",
            vec![line("قص", "08", "09", &["stylist-1", "chair-1"])],
        )
        .await
        .expect_err("the salon opens at nine");
    assert!(matches!(refused, BookingError::NotOffered { .. }));

    fixture.cleanup().await;
}

/// **Restaurant** — covers are the capacity and a sitting is the booking.
///
/// The fixture that proves capacity had to be a number. A table for six takes
/// one party of six or two of three, and nothing in the engine knows the word
/// "cover".
#[tokio::test]
async fn a_restaurant_seats_parties_up_to_the_covers_on_the_table() {
    let (fixture, _) = Fixture::for_trade("restaurant", "restaurant").await;

    fixture
        .book("BK-1", "c1", vec![places("عشاء", "20", "22", "table-6", 4)])
        .await
        .expect("four at a table for six");
    assert_eq!(fixture.free("table-6", "20", "22").await, 2);

    // Two more fit. Three do not.
    fixture
        .book("BK-2", "c2", vec![places("عشاء", "20", "22", "table-6", 2)])
        .await
        .expect("and two more");
    let refused = fixture
        .book("BK-3", "c3", vec![places("عشاء", "20", "22", "table-6", 1)])
        .await
        .expect_err("the table is full");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    // A party larger than the table is refused rather than seated badly.
    let refused = fixture
        .book("BK-4", "c4", vec![places("عشاء", "20", "22", "table-2", 4)])
        .await
        .expect_err("four will not sit at a table for two");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    // The second sitting takes the same table, because the first has ended.
    fixture
        .book(
            "BK-5",
            "c3",
            vec![places("عشاء", "22", "23:30", "table-6", 6)],
        )
        .await
        .expect("the later sitting");

    fixture.cleanup().await;
}

/// **Hotel** — the type is booked, the room is given out later.
///
/// The fixture that proves the pool and the unit are two different resources.
/// The count lives on the type and the identity lives on the room, so assigning
/// one does not count against the pool twice.
#[tokio::test]
async fn a_hotel_books_the_room_type_and_gives_out_the_room_at_check_in() {
    let (fixture, _) = Fixture::for_trade("hotel", "hotel").await;

    // Three nights, across midnight twice. No opening hours, because a guest
    // checks in at any hour.
    let stay = |id: &str| DraftLine {
        what: "إقامة".to_owned(),
        span: Span::new(
            "2026-09-02T15:00:00+03:00".parse().expect("valid"),
            "2026-09-05T11:00:00+03:00".parse().expect("valid"),
        )
        .expect("three nights"),
        takes: vec![Held::one(code(id))],
        charge: None,
    };

    for (n, who) in ["c1", "c2", "c3"].into_iter().enumerate() {
        fixture
            .book(&format!("BK-{n}"), who, vec![stay("double")])
            .await
            .unwrap_or_else(|e| panic!("three doubles are available: {e}"));
    }
    let refused = fixture
        .book("BK-4", "c4", vec![stay("double")])
        .await
        .expect_err("there is no fourth double");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    // The rooms themselves are untouched until somebody is checked in.
    for room in ["room-101", "room-102", "room-103"] {
        assert_eq!(
            fixture.free(room, "15", "18").await,
            1,
            "{room} was taken by a booking of the type"
        );
    }

    booking::assign(
        &fixture.db,
        &code("BK-0"),
        0,
        &code("room-101"),
        at("15"),
        &Metadata::default(),
    )
    .await
    .expect("101 at check-in");
    assert_eq!(fixture.free("room-101", "15", "18").await, 0);
    assert_eq!(
        fixture.free("double", "15", "18").await,
        0,
        "assigning a room counted against the pool a second time"
    );

    fixture.cleanup().await;
}

/// **Class** — one instructor, one room, twelve separate people in one hour.
///
/// The fixture a salon's chair cannot express, and the reason capacity is a
/// number rather than a flag.
///
/// # What this fixture found
///
/// Written first with one customer booked twelve times, and refused on the
/// second: `customer.c1 holds 1 of 1 then`. That is not a bug, it is the
/// "already in another chair" rule doing its job, and it constrains what a
/// class booking is allowed to look like.
///
/// **Twelve places in a class is either twelve customers, or one customer on
/// one booking.** A parent bringing four children is one reservation with four
/// places; twelve strangers are twelve reservations. What is refused is one
/// person holding twelve *separate simultaneous* bookings, and it has to be:
/// a system that allowed it would have no way left to catch the salon
/// double-booking, because they are the same query.
#[tokio::test]
async fn a_studio_puts_twelve_separate_people_in_one_hour() {
    let (fixture, _) = Fixture::for_trade("studio", "studio").await;

    // Twelve mats, twelve people.
    for n in 0..12 {
        let who = format!("m{n}");
        fixture.customer(&who).await;
        fixture
            .book(
                &format!("BK-{n}"),
                &who,
                vec![line("يوغا", "18", "19", &["studio-hall"])],
            )
            .await
            .unwrap_or_else(|e| panic!("place {n} of twelve: {e}"));
    }
    assert_eq!(fixture.free("studio-hall", "18", "19").await, 0);

    let refused = fixture
        .book(
            "BK-13",
            "c2",
            vec![line("يوغا", "18", "19", &["studio-hall"])],
        )
        .await
        .expect_err("the class is full");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    // The other legal shape: one person, one booking, several places. A parent
    // bringing three children is not three people who each need a record.
    fixture
        .book(
            "BK-14",
            "c1",
            vec![places("يوغا", "19", "20", "studio-hall", 4)],
        )
        .await
        .expect("a family of four on one booking");
    assert_eq!(fixture.free("studio-hall", "19", "20").await, 8);

    // The instructor is the thing that cannot be in two classes at once.
    fixture
        .book(
            "BK-15",
            "c3",
            vec![line("بيلاتس", "20", "21", &["instructor-1", "studio-hall"])],
        )
        .await
        .expect("the next hour");
    let refused = fixture
        .book(
            "BK-16",
            "c4",
            vec![line("بيلاتس", "20", "21", &["instructor-1"])],
        )
        .await
        .expect_err("one instructor cannot teach two classes at once");
    assert!(matches!(refused, BookingError::Occupancy(_)));

    fixture.cleanup().await;
}

/// **Museum** — pure capacity, nobody assigned.
///
/// Five hundred places at an hour with no named person and no unit to give out.
/// Rekaz sells to museums, event ticketing and horse stables; this is the shape
/// all three need, and it is the same code as the stylist.
#[tokio::test]
async fn a_museum_sells_hundreds_of_places_with_nobody_assigned() {
    let (fixture, _) = Fixture::for_trade("museum", "museum").await;

    // A family of four, and then a coach party of two hundred.
    fixture
        .book(
            "BK-1",
            "c1",
            vec![places("دخول", "10", "11", "entry-slot", 4)],
        )
        .await
        .expect("four tickets");
    fixture
        .book(
            "BK-2",
            "c2",
            vec![places("دخول", "10", "11", "entry-slot", 200)],
        )
        .await
        .expect("a coach party");
    assert_eq!(fixture.free("entry-slot", "10", "11").await, 296);

    // The slot fills, and the hour after it is empty.
    fixture
        .book(
            "BK-3",
            "c3",
            vec![places("دخول", "10", "11", "entry-slot", 296)],
        )
        .await
        .expect("the last of them");
    let refused = fixture
        .book(
            "BK-4",
            "c4",
            vec![places("دخول", "10", "11", "entry-slot", 1)],
        )
        .await
        .expect_err("the ten o'clock is sold out");
    assert!(matches!(refused, BookingError::Occupancy(_)));
    assert_eq!(fixture.free("entry-slot", "11", "12").await, 500);

    // Nothing was assigned to anybody, because there is nothing to assign.
    fixture.project().await;
    let detail = booking::reservation(
        &mut fixture.pool.acquire().await.expect("connection"),
        "BK-1",
    )
    .await
    .expect("reads")
    .expect("there");
    assert!(
        detail.lines[0].unit.is_none(),
        "a timed-entry ticket does not name a unit"
    );

    fixture.cleanup().await;
}

/// **Gym** — the fixture that proves occupancy is optional.
///
/// A member does not book. They hold a membership and walk in, so the gym floor
/// is deliberately not on the rota: declaring it would put a resource there
/// that nothing ever claims and would suggest that turning up is something to
/// reserve.
///
/// What a gym does book is its classes, and those are the same code as
/// everything else. The membership itself is
/// [Phase 14](../../../docs/IMPLEMENTATION.md) and is not modelled here.
#[tokio::test]
async fn a_gym_books_its_classes_and_not_its_door() {
    let (fixture, trade) = Fixture::for_trade("gym", "gym").await;

    // A gym that has just opened has a rota and an empty diary, and that is a
    // gym operating normally rather than one that is not set up.
    fixture.project().await;
    assert_eq!(fixture.rota().await.len(), trade.resources.len());
    assert!(
        fixture.diary().await.is_empty(),
        "a member walking in should not be a reservation"
    );

    // Nothing on the rota stands for the floor, the door or the changing rooms.
    for r in fixture.rota().await {
        assert!(
            !["floor", "door", "gym", "entry"].contains(&r.id.as_str()),
            "{} is on the rota; a member does not book it",
            r.id
        );
    }

    // The spin class is booked like any other class.
    fixture
        .book(
            "BK-1",
            "c1",
            vec![line("سبينينغ", "07", "08", &["trainer-1", "spin-studio"])],
        )
        .await
        .expect("a class at seven");
    fixture.project().await;
    assert_eq!(fixture.diary().await.len(), 1);

    // Cancelling it gives the bike back and leaves the gym running.
    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Cancelled,
        "نامت",
        at("06"),
        &Metadata::default(),
    )
    .await
    .expect("cancelled");
    assert_eq!(fixture.free("spin-studio", "07", "08").await, 20);

    fixture.cleanup().await;
}

/// **Every trade fits out, and fitting out twice changes nothing.**
///
/// The blueprint is a list of commands (D8), so this is also the check that
/// none of the six declares something the domain would refuse — a name too
/// long, a capacity out of range, an id that is not one.
#[tokio::test]
async fn every_trade_installs_as_written_and_twice_is_harmless() {
    for t in booking::TRADES {
        let (fixture, trade) = Fixture::for_trade(&format!("fit{}", t.id), t.id).await;

        let again = fit_out(
            &fixture.db,
            trade,
            Locale::Arabic,
            at("00"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{} should fit out twice: {e}", t.id));
        assert_eq!(again.declared, 0, "{} declared something twice", t.id);
        assert_eq!(again.skipped, trade.resources.len());

        fixture.project().await;
        let rota = fixture.rota().await;
        assert_eq!(
            rota.len(),
            trade.resources.len(),
            "{} has a rota that does not match its blueprint",
            t.id
        );
        // The names came out in Arabic, which is what the locale asked for.
        for r in &rota {
            assert!(
                !r.name.is_empty(),
                "{} declared {} with no name",
                t.id,
                r.id
            );
        }

        fixture.cleanup().await;
    }
}
