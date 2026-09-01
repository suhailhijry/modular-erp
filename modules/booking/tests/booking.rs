//! Bookings, against a real tenant.
//!
//! The tests that carry this file are
//! [`a_customer_cannot_be_in_two_chairs_at_once`] and
//! [`booking_the_same_id_twice_takes_the_capacity_once`]. The first is the
//! payoff for holding the customer in the same engine as the chair — no second
//! table, no special case — and the second is the one that would silently
//! double-book a resource for a client whose request timed out.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use booking::{
    Availability, BookingError, Details, Draft, DraftLine, Held, Kind, Stage, assign,
    declare_resource, move_to, reschedule, reserve, restore_resource, schedule_resource,
    withdraw_resource,
};
use erp_control::{
    Actor, ClusterRegistry, CommandError, ControlPlane, PoolConfig, TenantDb, TenantPools,
};
use erp_eventlog::{ExecuteError, Metadata};
use erp_occupancy::Span;
use erp_projection::{Projection, ensure_group_schema, replay_shadow, run_to_head};
use erp_testkit::{Schema, TestDb};
use erp_types::{AggregateId, Timestamp};

static CONTROL: Schema = Schema::migrations("control", &erp_control::MIGRATIONS);
static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

fn code(s: &str) -> AggregateId {
    AggregateId::new(s).expect("valid")
}

/// A local hour on the one Wednesday most of these tests happen on.
///
/// The tenant is at `+03:00` by default, so `at("10")` is 10:00 in Riyadh and
/// 07:00 UTC. Written this way on purpose: the availability rules below are in
/// local time and reading them against UTC instants is the mistake this helper
/// exists to stop.
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

/// One line, one hour, taking whatever it is given.
fn line(what: &str, from: &str, until: &str, takes: &[&str]) -> DraftLine {
    DraftLine {
        what: what.to_owned(),
        span: span(from, until),
        takes: takes.iter().map(|r| Held::one(code(r))).collect(),
        charge: None,
    }
}

fn booking_for(customer: Option<&str>, lines: Vec<DraftLine>) -> Draft {
    Draft {
        customer: booking::Customer {
            id: customer.map(code),
            name: "سارة".to_owned(),
            phone: Some("+966511111111".to_owned()),
        },
        lines,
        note: String::new(),
        at: at("08"),
    }
}

fn person(name: &str) -> Details {
    Details {
        name: name.to_owned(),
        name_latin: None,
        kind: Kind::Person,
        capacity: 1,
    }
}

fn place(name: &str, capacity: u16) -> Details {
    Details {
        name: name.to_owned(),
        name_latin: None,
        kind: Kind::Place,
        capacity,
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

        // A stylist, a chair, and a customer to book. Every test needs at least
        // one of each and none of them are what is being measured.
        fixture.declare("noura", &person("نورة")).await;
        fixture.declare("chair-1", &place("كرسي ١", 1)).await;
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
            at("00"),
            &Metadata::default(),
        )
        .await
        .expect("the customer is on file");

        fixture
    }

    async fn declare(&self, id: &str, details: &Details) {
        declare_resource(&self.db, &code(id), details, at("00"), &Metadata::default())
            .await
            .unwrap_or_else(|e| panic!("{id} should be declarable: {e}"));
    }

    async fn project(&self) {
        let owned = booking::projections();
        let refs: Vec<&dyn Projection<Group = booking::Booking>> =
            owned.iter().map(AsRef::as_ref).collect();
        run_to_head::<booking::Booking>(&self.pool, &refs, booking::upcasters(), 200)
            .await
            .expect("booking projects");
    }

    async fn diary(&self) -> Vec<booking::ReservationSummary> {
        let mut conn = self.pool.acquire().await.expect("connection");
        booking::reservations(&mut conn, None, None, None, 50, None)
            .await
            .expect("reads")
            .items
    }

    async fn get(&self, id: &str) -> Option<booking::ReservationDetail> {
        let mut conn = self.pool.acquire().await.expect("connection");
        booking::reservation(&mut conn, id).await.expect("reads")
    }

    /// How much of a resource is free for a span, straight from the engine.
    async fn free(&self, resource: &str, from: &str, until: &str) -> u16 {
        let mut conn = self.pool.acquire().await.expect("connection");
        erp_occupancy::free(&mut conn, &code(resource), span(from, until))
            .await
            .expect("the engine answers")
    }

    async fn cleanup(self) {
        drop(self.db);
        self.pool.close().await;
        let _ = erp_testkit::drop_named_database(&self.database).await;
    }
}

/// What a rejection was, when there was one.
fn rejection(error: &CommandError<BookingError>) -> Option<&BookingError> {
    match error {
        CommandError::Execute(ExecuteError::Rejected(e)) => Some(e),
        _ => None,
    }
}

/// **The module in one pass**: book it, read it back, walk it to completion.
#[tokio::test]
async fn a_booking_is_taken_and_read_back() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![line("قص", "10", "11", &["noura", "chair-1"])],
        ),
        &Metadata::default(),
    )
    .await
    .expect("the chair is free");
    fixture.project().await;

    let detail = fixture.get("BK-1").await.expect("it is in the diary");
    assert_eq!(detail.summary.customer_id.as_deref(), Some("CUST-1"));
    assert_eq!(detail.summary.customer_name, "سارة");
    assert_eq!(detail.summary.stage, "reserved");
    assert_eq!(detail.summary.starts_at, at("10"));
    assert_eq!(detail.summary.ends_at, at("11"));
    assert_eq!(detail.lines.len(), 1);
    assert_eq!(detail.lines[0].what, "قص");
    assert_eq!(detail.lines[0].takes.len(), 2);

    // And the engine is holding both, which is the half a projection cannot say.
    assert_eq!(fixture.free("noura", "10", "11").await, 0);
    assert_eq!(fixture.free("chair-1", "10", "11").await, 0);

    for stage in [
        Stage::Confirmed,
        Stage::Arrived,
        Stage::InService,
        Stage::Completed,
    ] {
        move_to(
            &fixture.db,
            &code("BK-1"),
            stage,
            "",
            at("12"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{stage} should be reachable: {e}"));
    }
    fixture.project().await;
    assert_eq!(fixture.diary().await[0].stage, "completed");

    fixture.cleanup().await;
}

/// **Two bookings, one chair.** The floor: if the second one lands, nothing
/// else in this module matters.
#[tokio::test]
async fn the_second_booking_for_a_taken_chair_is_refused() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("the first one fits");

    let refused = reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect_err("the chair is taken");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Occupancy(_))),
        "expected an occupancy refusal, got {refused}"
    );

    // Back to back is not a clash, which is the half-open rule reaching all the
    // way up from the engine.
    reserve(
        &fixture.db,
        &code("BK-3"),
        &booking_for(None, vec![line("قص", "11", "12", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("the hour that starts where the last one ended fits");

    fixture.cleanup().await;
}

/// **The customer is a resource, so being in two chairs at once is refused by
/// the same machinery that refuses two people in one chair.**
///
/// No second table, no query over the diary, and the same concurrency
/// guarantee. This is the payoff for the reserved `customer.` prefix.
#[tokio::test]
async fn a_customer_cannot_be_in_two_chairs_at_once() {
    let fixture = Fixture::new().await;
    fixture.declare("chair-2", &place("كرسي ٢", 1)).await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("the first chair is free");

    // A different chair, entirely free, at the same hour. The customer is not.
    let refused = reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(Some("CUST-1"), vec![line("صبغة", "10", "11", &["chair-2"])]),
        &Metadata::default(),
    )
    .await
    .expect_err("one person cannot be in two chairs");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Occupancy(_))),
        "expected an occupancy refusal, got {refused}"
    );
    assert_eq!(
        fixture.free("chair-2", "10", "11").await,
        1,
        "the second chair was held by a booking that was refused"
    );

    // A walk-in with no record is not held, so two of them at once is fine.
    reserve(
        &fixture.db,
        &code("BK-3"),
        &booking_for(None, vec![line("قص", "10", "11", &["chair-2"])]),
        &Metadata::default(),
    )
    .await
    .expect("a walk-in has no diary of their own");

    fixture.cleanup().await;
}

/// **One customer, several places, one hour.**
///
/// A parent booking three seats is one person at one time and must be allowed.
/// The customer is held once per *distinct* span, which is what tells that
/// apart from a haircut and a massage that overlap.
#[tokio::test]
async fn one_customer_may_take_several_places_in_the_same_hour() {
    let fixture = Fixture::new().await;
    fixture.declare("class-1000", &place("صف اليوغا", 10)).await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![
                line("يوغا", "10", "11", &["class-1000"]),
                line("يوغا", "10", "11", &["class-1000"]),
                line("يوغا", "10", "11", &["class-1000"]),
            ],
        ),
        &Metadata::default(),
    )
    .await
    .expect("three places for one family at one hour");
    assert_eq!(fixture.free("class-1000", "10", "11").await, 7);

    // Two lines that overlap without matching is one person in two places, and
    // that is still refused.
    let refused = reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(
            Some("CUST-1"),
            vec![
                line("يوغا", "12", "13", &["class-1000"]),
                line("يوغا", "12:30", "13:30", &["class-1000"]),
            ],
        ),
        &Metadata::default(),
    )
    .await
    .expect_err("one person cannot be in two overlapping classes");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::Occupancy(_))
    ));

    fixture.cleanup().await;
}

/// **Booking the same id twice takes the capacity once.**
///
/// A client whose request timed out retries, and the second attempt must be a
/// no-op all the way down. Taking the claims again would either collide with
/// the booking's own rows or, on a class with room, silently seat the same
/// person twice.
#[tokio::test]
async fn booking_the_same_id_twice_takes_the_capacity_once() {
    let fixture = Fixture::new().await;
    fixture.declare("class-1000", &place("صف اليوغا", 10)).await;

    let draft = booking_for(
        Some("CUST-1"),
        vec![line("يوغا", "10", "11", &["class-1000"])],
    );
    let first = reserve(&fixture.db, &code("BK-1"), &draft, &Metadata::default())
        .await
        .expect("the first one lands");
    assert!(first.at.is_some(), "the first call wrote nothing");

    let again = reserve(&fixture.db, &code("BK-1"), &draft, &Metadata::default())
        .await
        .expect("the retry is quiet");
    assert!(again.at.is_none(), "the retry wrote a second event");

    assert_eq!(
        fixture.free("class-1000", "10", "11").await,
        9,
        "the retry took a second place"
    );

    fixture.cleanup().await;
}

/// **The lifecycle only goes forwards, and only out of a stage it can leave.**
#[tokio::test]
async fn the_lifecycle_only_goes_one_way() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked");

    // Skipping forwards is allowed: a walk-in arrives without confirming.
    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Arrived,
        "",
        at("10"),
        &Metadata::default(),
    )
    .await
    .expect("a walk-in arrives without ever being confirmed");

    // Backwards is not.
    let refused = move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Reserved,
        "",
        at("10"),
        &Metadata::default(),
    )
    .await
    .expect_err("a booking cannot go back to reserved");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::CannotMove { .. })
    ));

    // And somebody who is standing in front of you is not a no-show.
    let refused = move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::NoShow,
        "",
        at("10"),
        &Metadata::default(),
    )
    .await
    .expect_err("they are here");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::CannotMove { .. })
    ));

    // Moving to where it already is is a no-op, so a retried "mark them
    // arrived" is harmless.
    let again = move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Arrived,
        "",
        at("10"),
        &Metadata::default(),
    )
    .await
    .expect("the retry is quiet");
    assert!(again.at.is_none());

    // Once it ends, nothing more happens.
    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Completed,
        "",
        at("11"),
        &Metadata::default(),
    )
    .await
    .expect("done");
    let refused = move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Cancelled,
        "",
        at("11"),
        &Metadata::default(),
    )
    .await
    .expect_err("a finished booking cannot be cancelled");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::Over { .. })
    ));

    fixture.cleanup().await;
}

/// **Cancelling gives the chair back. Completing does not.**
///
/// A finished appointment held that chair, and deleting its claim would make
/// the past look free.
#[tokio::test]
async fn cancelling_frees_the_chair_and_completing_keeps_it() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    assert_eq!(fixture.free("chair-1", "10", "11").await, 0);

    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Cancelled,
        "غيّرت رأيها",
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("cancelled");
    assert_eq!(
        fixture.free("chair-1", "10", "11").await,
        1,
        "cancelling did not give the chair back"
    );

    // And somebody else can have it.
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("the hour is free again");
    move_to(
        &fixture.db,
        &code("BK-2"),
        Stage::Completed,
        "",
        at("11"),
        &Metadata::default(),
    )
    .await
    .expect("done");
    assert_eq!(
        fixture.free("chair-1", "10", "11").await,
        0,
        "a finished appointment stopped having used the chair"
    );

    fixture.cleanup().await;
}

/// **A booking never conflicts with where it already was.**
///
/// Nudging an appointment half an hour later overlaps its own claim, so a
/// reschedule that probed before releasing would refuse every small move and
/// allow only the large ones.
#[tokio::test]
async fn a_booking_can_be_nudged_without_conflicting_with_itself() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked");

    reschedule(
        &fixture.db,
        &code("BK-1"),
        &[line("قص", "10:30", "11:30", &["chair-1"])],
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("a booking must not conflict with itself");
    fixture.project().await;

    assert_eq!(
        fixture.get("BK-1").await.expect("there").summary.starts_at,
        at("10:30")
    );
    assert_eq!(fixture.free("chair-1", "10", "10:30").await, 1);
    assert_eq!(fixture.free("chair-1", "11", "11:30").await, 0);

    // A move onto an hour somebody else holds leaves it where it was.
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("قص", "14", "15", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("an unrelated afternoon booking");
    let refused = reschedule(
        &fixture.db,
        &code("BK-1"),
        &[line("قص", "14", "15", &["chair-1"])],
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect_err("the afternoon is taken");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::Occupancy(_))
    ));
    assert_eq!(
        fixture.free("chair-1", "10:30", "11:30").await,
        0,
        "a refused reschedule gave up the slot it already had"
    );

    fixture.cleanup().await;
}

/// **Book the type, assign the unit later.**
///
/// A hotel books "a double" and gives out room 302 at check-in. The pool holds
/// the count and the unit holds the identity, so nothing is counted twice — and
/// reassigning gives the first room back.
#[tokio::test]
async fn a_pool_is_booked_by_the_type_and_the_unit_comes_later() {
    let fixture = Fixture::new().await;
    fixture.declare("double", &place("غرفة مزدوجة", 2)).await;
    fixture.declare("room-302", &place("٣٠٢", 1)).await;
    fixture.declare("room-305", &place("٣٠٥", 1)).await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("إقامة", "14", "18", &["double"])]),
        &Metadata::default(),
    )
    .await
    .expect("a double is free");
    assert_eq!(fixture.free("double", "14", "18").await, 1);
    assert_eq!(
        fixture.free("room-302", "14", "18").await,
        1,
        "booking the type should not have taken a unit"
    );

    assign(
        &fixture.db,
        &code("BK-1"),
        0,
        &code("room-302"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("302 is free");
    assert_eq!(fixture.free("room-302", "14", "18").await, 0);
    assert_eq!(
        fixture.free("double", "14", "18").await,
        1,
        "assigning a unit counted against the pool a second time"
    );

    // Assigning the same room again is a no-op.
    let again = assign(
        &fixture.db,
        &code("BK-1"),
        0,
        &code("room-302"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("the retry is quiet");
    assert!(again.at.is_none());

    // A different room replaces it, and the first one goes back to the floor.
    assign(
        &fixture.db,
        &code("BK-1"),
        0,
        &code("room-305"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("305 instead");
    assert_eq!(fixture.free("room-302", "14", "18").await, 1);
    assert_eq!(fixture.free("room-305", "14", "18").await, 0);

    fixture.project().await;
    let detail = fixture.get("BK-1").await.expect("there");
    assert_eq!(detail.lines[0].unit.as_deref(), Some("room-305"));

    fixture.cleanup().await;
}

/// **A resource is only booked when it is open.**
///
/// The timetable is local, the span is UTC, and the tenant is at `+03:00`. That
/// conversion is the whole reason this test books at the edges of the window
/// rather than in the middle of it.
#[tokio::test]
async fn a_resource_is_only_booked_inside_its_opening_hours() {
    let fixture = Fixture::new().await;

    // Wednesday, and open 09:00 to 17:00 local.
    schedule_resource(
        &fixture.db,
        &code("chair-1"),
        &[Availability::from_parts(&[], &[3], &[], 9 * 60, 17 * 60, None, None).expect("a rule")],
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect("the rota is set");

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "16", "17", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("the last hour before closing is open");

    // Half an hour over the end.
    let refused = reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("قص", "16:30", "17:30", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect_err("half of it is after closing");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NotOffered { .. })),
        "expected NotOffered, got {refused}"
    );

    // And the following day is a Thursday, which the rule does not name.
    let thursday = Span::new(
        "2026-09-03T10:00:00+03:00".parse().expect("valid"),
        "2026-09-03T11:00:00+03:00".parse().expect("valid"),
    )
    .expect("a span");
    let refused = reserve(
        &fixture.db,
        &code("BK-3"),
        &booking_for(
            None,
            vec![DraftLine {
                what: "قص".to_owned(),
                span: thursday,
                takes: vec![Held::one(code("chair-1"))],
                charge: None,
            }],
        ),
        &Metadata::default(),
    )
    .await
    .expect_err("Thursday is not in the rota");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::NotOffered { .. })
    ));

    fixture.cleanup().await;
}

/// **Withdrawing stops new bookings and keeps the old ones.**
///
/// A chair that broke on Tuesday was still booked on Monday.
#[tokio::test]
async fn withdrawing_stops_new_bookings_and_keeps_the_old_ones() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked while it worked");

    withdraw_resource(
        &fixture.db,
        &code("chair-1"),
        "انكسر",
        at("12"),
        &Metadata::default(),
    )
    .await
    .expect("out of service");

    let refused = reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("قص", "14", "15", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect_err("a broken chair takes nothing");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Withdrawn(_))),
        "expected Withdrawn, got {refused}"
    );

    fixture.project().await;
    let detail = fixture.get("BK-1").await.expect("still in the diary");
    assert_eq!(detail.summary.stage, "reserved");

    restore_resource(
        &fixture.db,
        &code("chair-1"),
        at("13"),
        &Metadata::default(),
    )
    .await
    .expect("mended");
    reserve(
        &fixture.db,
        &code("BK-3"),
        &booking_for(None, vec![line("قص", "14", "15", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("and it takes bookings again");

    fixture.cleanup().await;
}

/// **A booking for somebody who is not on file is refused**, and it is refused
/// against the log rather than the projection — so a customer registered a
/// moment ago can be booked immediately.
#[tokio::test]
async fn a_booking_for_a_customer_who_is_not_there_is_refused() {
    let fixture = Fixture::new().await;

    let refused = reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-NOBODY"),
            vec![line("قص", "10", "11", &["chair-1"])],
        ),
        &Metadata::default(),
    )
    .await
    .expect_err("there is no such customer");
    assert!(matches!(
        rejection(&refused),
        Some(BookingError::NoSuchCustomer(_))
    ));
    assert_eq!(
        fixture.free("chair-1", "10", "11").await,
        1,
        "the chair was held by a booking that was refused"
    );

    // Registered and booked without a projection run in between.
    crm::register_customer(
        &fixture.db,
        &code("CUST-2"),
        &crm::Details {
            name: "مريم".to_owned(),
            name_latin: None,
            kind: crm::CustomerKind::Person,
            contact: crm::Contact {
                phone: Some("+966522222222".to_owned()),
                email: None,
            },
            address: None,
            tax: None,
        },
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("registers");
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(Some("CUST-2"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("a customer created a moment ago can be booked");

    fixture.cleanup().await;
}

/// **The diary is a pure function of the log.**
///
/// Replayed into a shadow schema, every row has to come out identical. The
/// witness assertion is there because a differ that compares two empty schemas
/// passes for the wrong reason.
#[tokio::test]
async fn a_rebuild_reproduces_the_diary() {
    let fixture = Fixture::new().await;
    fixture.declare("double", &place("غرفة مزدوجة", 2)).await;
    fixture.declare("room-302", &place("٣٠٢", 1)).await;

    schedule_resource(
        &fixture.db,
        &code("chair-1"),
        &[Availability::daily(9 * 60, 17 * 60).expect("a rule")],
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect("the rota is set");

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["chair-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(None, vec![line("إقامة", "14", "16", &["double"])]),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    assign(
        &fixture.db,
        &code("BK-2"),
        0,
        &code("room-302"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("assigned");
    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Cancelled,
        "اعتذرت",
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("cancelled");
    withdraw_resource(
        &fixture.db,
        &code("room-302"),
        "صيانة",
        at("18"),
        &Metadata::default(),
    )
    .await
    .expect("withdrawn");

    fixture.project().await;
    assert_eq!(fixture.diary().await.len(), 2, "nothing to compare");

    let owned = booking::projections();
    let refs: Vec<&dyn Projection<Group = booking::Booking>> =
        owned.iter().map(AsRef::as_ref).collect();
    let report = replay_shadow::<booking::Booking>(&fixture.pool, &refs, booking::upcasters(), 200)
        .await
        .expect("the shadow replays");
    assert!(
        report.is_reproducible(),
        "a rebuild must reproduce the diary exactly: {:?}",
        report.differences()
    );

    fixture.cleanup().await;
}

/// Sets the tenant's price bands. Configuration, like the VAT rate.
async fn set_bands(fixture: &Fixture, bands: Vec<booking::Band>) {
    let mut conn = fixture.pool.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        booking::Tariff::KEY,
        &booking::Tariff { bands },
        None,
    )
    .await
    .expect("the tariff is set");
}

/// Thursday evening costs a quarter more.
fn thursday_peak() -> booking::Band {
    booking::Band {
        name: "ذروة الخميس".to_owned(),
        when: Availability::from_parts(&[], &[4], &[], 17 * 60, 21 * 60, None, None)
            .expect("a rule"),
        uplift: 2_500,
    }
}

fn sar() -> erp_types::CurrencyCode {
    erp_types::CurrencyCode::new("SAR").expect("a real code")
}

fn money(minor: i64) -> erp_types::Money {
    erp_types::Money::from_minor(minor, sar())
}

/// A line with a price on it.
fn charged(
    what: &str,
    span: Span,
    resource: &str,
    rate: i64,
    quantity: u16,
    off: i64,
) -> DraftLine {
    DraftLine {
        what: what.to_owned(),
        span,
        takes: vec![Held::one(code(resource))],
        charge: Some(booking::Charge {
            rate: money(rate),
            quantity,
            allowances: if off == 0 {
                Vec::new()
            } else {
                vec![booking::Allowance {
                    reason: "عرض الافتتاح".to_owned(),
                    amount: money(off),
                }]
            },
        }),
    }
}

/// An hour on the Thursday after the Wednesday everything else happens on.
fn thursday(from: &str, until: &str) -> Span {
    Span::new(
        format!("2026-09-03T{from}:00:00+03:00")
            .parse()
            .expect("valid"),
        format!("2026-09-03T{until}:00:00+03:00")
            .parse()
            .expect("valid"),
    )
    .expect("an hour")
}

/// **A booking is priced when it is taken, and the band is frozen onto it.**
///
/// The whole of 8d in one pass: the tenant's bands are configuration, they are
/// resolved inside the transaction that writes the booking, and moving them
/// afterwards changes what the *next* booking costs and nothing that was
/// already agreed (L5).
#[tokio::test]
async fn a_booking_is_priced_against_the_tenants_bands() {
    let fixture = Fixture::new().await;
    set_bands(&fixture, vec![thursday_peak()]).await;

    // Wednesday at ten: the base rate, and a discount off the net.
    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", span("10", "11"), "chair-1", 8_000, 2, 2_500)],
        ),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    fixture.project().await;

    let detail = fixture.get("BK-1").await.expect("there");
    let priced = detail.lines[0].charge.as_ref().expect("it was priced");
    assert!(priced.band.is_none(), "Wednesday is not a peak band");
    assert_eq!(priced.gross, money(16_000));
    assert_eq!(priced.net, money(13_500), "the discount came off the net");

    // Thursday evening: a quarter more, and the band's name is on the line.
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", thursday("18", "19"), "chair-1", 8_000, 1, 0)],
        ),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    fixture.project().await;

    let detail = fixture.get("BK-2").await.expect("there");
    let priced = detail.lines[0].charge.as_ref().expect("it was priced");
    assert_eq!(
        priced.band.as_ref().map(|b| b.name.as_str()),
        Some("ذروة الخميس"),
        "the peak band did not apply"
    );
    assert_eq!(priced.rate, money(8_000), "the list rate is kept beside it");
    assert_eq!(priced.net, money(10_000));

    fixture.cleanup().await;
}

/// **Moving the bands does not restate a booking already taken.**
///
/// The band is resolved in the transaction that writes the booking and frozen
/// onto the line (L5), for the same reason a VAT rate is frozen onto an
/// invoice. A tenant who puts their peak hours up next month has not made last
/// month's appointments more expensive.
#[tokio::test]
async fn moving_the_bands_does_not_restate_what_was_already_agreed() {
    let fixture = Fixture::new().await;
    set_bands(&fixture, vec![thursday_peak()]).await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", thursday("18", "19"), "chair-1", 8_000, 1, 0)],
        ),
        &Metadata::default(),
    )
    .await
    .expect("booked at peak");
    fixture.project().await;
    assert_eq!(
        fixture.get("BK-1").await.expect("there").lines[0]
            .charge
            .as_ref()
            .expect("priced")
            .net,
        money(10_000)
    );

    set_bands(&fixture, Vec::new()).await;
    fixture.project().await;
    assert_eq!(
        fixture.get("BK-1").await.expect("still there").lines[0]
            .charge
            .as_ref()
            .expect("still priced")
            .net,
        money(10_000),
        "clearing the tariff restated a booking that was already agreed"
    );

    // And the next booking at that hour is now the base rate.
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", thursday("19", "20"), "chair-1", 8_000, 1, 0)],
        ),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    fixture.project().await;
    assert_eq!(
        fixture.get("BK-2").await.expect("there").lines[0]
            .charge
            .as_ref()
            .expect("priced")
            .net,
        money(8_000)
    );

    fixture.cleanup().await;
}

/// **A client cannot send its own idea of what a booking costs.**
///
/// The rate is the caller's, the band is the tenant's. A refused price leaves
/// nothing behind — no event, and no claim on the chair.
#[tokio::test]
async fn a_price_that_is_not_one_is_refused_and_takes_no_capacity() {
    let fixture = Fixture::new().await;

    let refused = reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", span("10", "11"), "chair-1", 8_000, 1, 9_000)],
        ),
        &Metadata::default(),
    )
    .await
    .expect_err("a discount larger than the line");
    assert!(
        matches!(
            rejection(&refused),
            Some(BookingError::Price(booking::PriceError::AllowanceTooLarge))
        ),
        "expected AllowanceTooLarge, got {refused}"
    );
    assert_eq!(
        fixture.free("chair-1", "10", "11").await,
        1,
        "the chair was held by a booking that was refused"
    );

    fixture.cleanup().await;
}

/// Every message this module can produce has a translation in every locale.
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&booking::CATALOG);
}
