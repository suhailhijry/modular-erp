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
        rate: None,
        branch: None,
        employee: None,
    }
}

fn place(name: &str, capacity: u16) -> Details {
    Details {
        name: name.to_owned(),
        name_latin: None,
        kind: Kind::Place,
        capacity,
        rate: None,
        branch: None,
        employee: None,
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
        // `hr` is a crate dependency and not an entitlement one, but a
        // fixture that exercises the staff link needs its tables.
        hr::install(&mut conn).await.expect("hr installs");
        ensure_group_schema::<hr::Hr>(&mut conn)
            .await
            .expect("hr checkpoint");
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
///
/// Written out rather than filled in from a template, which is what most of
/// these tests are about — see `a_band_written_from_a_template_prices_a_booking`
/// for the other authoring level.
async fn set_bands(fixture: &Fixture, bands: Vec<booking::Band>) {
    set_tariff(
        fixture,
        bands
            .into_iter()
            .map(|rule| erp_rules::Authored::Raw { rule })
            .collect(),
    )
    .await;
}

async fn set_tariff(fixture: &Fixture, bands: Vec<erp_rules::Authored<booking::Band>>) {
    let mut conn = fixture.pool.acquire().await.expect("connection");
    erp_eventlog::configuration::set(
        &mut conn,
        booking::TariffAsWritten::KEY,
        &booking::TariffAsWritten { bands },
        None,
        None,
    )
    .await
    .expect("the tariff is set");
}

/// The same band, filled into the shipped form instead.
///
/// Percent rather than basis points, an hour rather than minutes past
/// midnight, and a weekday rather than a bitmask — which is the whole
/// difference a form makes.
fn thursday_peak_from_a_form() -> erp_rules::Authored<booking::Band> {
    let answers = [
        ("name", erp_rules::Value::Text("ذروة الخميس".to_owned())),
        ("weekday", erp_rules::Value::Int(4)),
        ("from_hour", erp_rules::Value::Int(17)),
        ("percent", erp_rules::Value::Int(25)),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();

    erp_rules::Authored::written(
        booking::templates::TARIFF_TEMPLATES,
        "weekday_evening",
        answers,
    )
    .expect("the form is filled in")
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

/// **"All producing the same artifact."**
///
/// The same band written two ways — filled into a form, and written out by
/// hand — prices the same booking to the same number, and puts the same name
/// on the line. That is the claim the four authoring levels make, checked here
/// against a real booking rather than against a `Band` literal.
#[tokio::test]
async fn a_band_filled_into_a_form_prices_exactly_as_one_written_out() {
    let fixture = Fixture::new().await;
    fixture.declare("chair-2", &place("كرسي ٢", 1)).await;
    set_tariff(&fixture, vec![thursday_peak_from_a_form()]).await;

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
    .expect("booked");

    // The same band, written out rather than filled in. A second chair and a
    // walk-in, because a customer is a resource too — so the two bookings are
    // the same hour of the same Thursday and differ in nothing that touches
    // the price but how their band was written.
    set_bands(&fixture, vec![thursday_peak()]).await;
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(
            None,
            vec![charged("قص", thursday("18", "19"), "chair-2", 8_000, 1, 0)],
        ),
        &Metadata::default(),
    )
    .await
    .expect("booked");
    fixture.project().await;

    let from_a_form = fixture.get("BK-1").await.expect("there");
    let written_out = fixture.get("BK-2").await.expect("there");
    let filled = from_a_form.lines[0].charge.as_ref().expect("priced");
    let written = written_out.lines[0].charge.as_ref().expect("priced");

    assert_eq!(
        filled.band.as_ref().map(|b| b.name.as_str()),
        Some("ذروة الخميس"),
        "the form's band did not apply"
    );
    assert_eq!(filled.band, written.band, "two ways of writing one band");
    assert_eq!(filled.net, written.net);
    assert_eq!(filled.net, money(10_000));

    fixture.cleanup().await;
}

/// **A template this build no longer ships stops the booking.**
///
/// Not a booking priced without its peak band: that is a month of underbilling
/// nobody notices, and L6 refuses on its behalf. The refusal names the
/// configuration key, so whoever reads the log knows which setting to fix.
#[tokio::test]
async fn a_band_whose_template_is_gone_refuses_the_booking() {
    let fixture = Fixture::new().await;
    set_tariff(
        &fixture,
        vec![erp_rules::Authored::Preset {
            template: "seasonal".to_owned(),
        }],
    )
    .await;

    let refused = reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(
            Some("CUST-1"),
            vec![charged("قص", thursday("18", "19"), "chair-1", 8_000, 1, 0)],
        ),
        &Metadata::default(),
    )
    .await
    .expect_err("there is no such template");

    assert!(
        format!("{refused}").contains(booking::TariffAsWritten::KEY),
        "the refusal does not say which setting is wrong: {refused}"
    );

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

/// Hires somebody and gives them a chair of their own.
///
/// Split out because the test was over the line limit, and because the setup is
/// not the interesting part: what the test is about starts at the first
/// `assign`.
async fn a_stylist_with_her_own_chair(fixture: &Fixture) {
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
            at: at("00"),
        },
        &Metadata::default(),
    )
    .await
    .expect("hired");

    // **Its own id**: the fixture already declares `chair-1`, and re-declaring
    // it is a no-op — which is `try_create` doing exactly its job, and was
    // worth finding here rather than in production.
    fixture
        .declare(
            "sara-chair",
            &Details {
                name: "سارة".to_owned(),
                name_latin: None,
                kind: Kind::Person,
                capacity: 1,
                rate: None,
                branch: None,
                employee: Some(code("EMP-1")),
            },
        )
        .await;

    // The customer books *the service*, and who does it is assigned after —
    // which is the shape a salon actually works in and the one `assign` is for.
    fixture.declare("haircut", &place("قص", 4)).await;
}

/// **A lapsed work document stops the rota, and this is where it stops it.**
///
/// The escalation §9e asks for: an expired iqama is not a warning somebody
/// ignored, it is a person who may not legally be rostered, and `assign` is the
/// moment they would be.
#[tokio::test]
async fn somebody_whose_iqama_has_lapsed_cannot_be_assigned() {
    let fixture = Fixture::new().await;
    a_stylist_with_her_own_chair(&fixture).await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "14", "15", &["haircut"])]),
        &Metadata::default(),
    )
    .await
    .expect("the service is free");

    // No documents recorded: assignable, because a business that has not
    // started recording them must not find its rota refused.
    assign(
        &fixture.db,
        &code("BK-1"),
        0,
        &code("sara-chair"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("nothing has lapsed");

    // Her iqama expired yesterday.
    let yesterday = at("14").date_naive() - chrono::Days::new(1);
    hr::record_document(
        &fixture.db,
        &code("EMP-1"),
        hr::DocumentKind::Identity,
        "2312345678",
        yesterday,
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect("recorded");

    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(Some("CUST-1"), vec![line("قص", "16", "17", &["haircut"])]),
        &Metadata::default(),
    )
    .await
    .expect("the service is still free");

    let error = assign(
        &fixture.db,
        &code("BK-2"),
        0,
        &code("sara-chair"),
        at("16"),
        &Metadata::default(),
    )
    .await
    .expect_err("a lapsed iqama was rostered");
    assert!(
        format!("{error:?}").contains("MayNotWork"),
        "refused for the wrong reason: {error:?}"
    );

    // A chair that names nobody is unaffected, which is what keeps the link
    // optional rather than a migration.
    fixture.declare("anybody-chair", &person("كرسي")).await;
    reserve(
        &fixture.db,
        &code("BK-3"),
        &booking_for(Some("CUST-1"), vec![line("قص", "18", "19", &["haircut"])]),
        &Metadata::default(),
    )
    .await
    .expect("free");
    assign(
        &fixture.db,
        &code("BK-3"),
        0,
        &code("anybody-chair"),
        at("18"),
        &Metadata::default(),
    )
    .await
    .expect("a chair that is nobody has no documents to lapse");

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Deposits at booking
// ---------------------------------------------------------------------------

use erp_types::Money;

fn riyals(major: i64) -> Money {
    Money::from_minor(major * 100, sar())
}

/// A line with a price on it, which is what a deposit is a fraction of.
fn priced_line(what: &str, from: &str, until: &str, takes: &[&str], rate: Money) -> DraftLine {
    DraftLine {
        what: what.to_owned(),
        span: span(from, until),
        takes: takes.iter().map(|r| Held::one(code(r))).collect(),
        charge: Some(booking::Charge {
            rate,
            quantity: 1,
            allowances: Vec::new(),
        }),
    }
}

impl Fixture {
    async fn set_public(&self, settings: booking::PublicBooking) {
        let mut conn = self.db.acquire().await.expect("connection");
        erp_eventlog::configuration::set(
            &mut conn,
            booking::PublicBooking::KEY,
            &settings,
            None,
            None,
        )
        .await
        .expect("stores the setting");
    }

    async fn book_priced(&self, id: &str, rate: Money, from: &str, until: &str) -> AggregateId {
        let draft = booking_for(
            Some("CUST-1"),
            vec![priced_line("قص", from, until, &["stylist-1"], rate)],
        );
        reserve(&self.db, &code(id), &draft, &Metadata::default())
            .await
            .unwrap_or_else(|e| panic!("{id} should book: {e}"));
        code(id)
    }
}

/// **A booking is billed once, and only one that was supplied.** The invoice
/// is whoever's called it; the diary records that it happened, refuses a
/// cancelled or a no-show booking (nothing was supplied), refuses one with no
/// priced line (nothing to raise a document from), and answers nothing the
/// second time it is told.
#[expect(
    clippy::too_many_lines,
    reason = "four bookings — billed, cancelled, unpriced, waiting — against one rule; \
              splitting it would mean four fixtures for one story"
)]
#[tokio::test]
async fn a_booking_is_billed_once_and_only_when_something_was_supplied() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;

    let billed = fixture.book_priced("RES-B1", riyals(200), "10", "11").await;
    for stage in [
        Stage::Confirmed,
        Stage::Arrived,
        Stage::InService,
        Stage::Completed,
    ] {
        booking::move_to(
            &fixture.db,
            &billed,
            stage,
            "",
            at("11"),
            &Metadata::default(),
        )
        .await
        .expect("moves");
    }
    let mut tx = fixture.db.begin().await.expect("transaction");
    let first = booking::bill_in(
        &mut tx,
        &billed,
        &code("bk-RES-B1"),
        at("12"),
        &Metadata::default(),
    )
    .await
    .expect("bills");
    assert!(first.at.is_some(), "the billing was recorded");
    let again = booking::bill_in(
        &mut tx,
        &billed,
        &code("bk-other"),
        at("12"),
        &Metadata::default(),
    )
    .await
    .expect("a second telling is quiet");
    assert!(again.at.is_none(), "billed twice");
    tx.commit().await.expect("commits");

    // Nothing supplied: refused.
    let cancelled = fixture.book_priced("RES-B2", riyals(200), "12", "13").await;
    booking::move_to(
        &fixture.db,
        &cancelled,
        Stage::Cancelled,
        "",
        at("12"),
        &Metadata::default(),
    )
    .await
    .expect("cancels");
    let mut tx = fixture.db.begin().await.expect("transaction");
    let refused = booking::bill_in(
        &mut tx,
        &cancelled,
        &code("bk-RES-B2"),
        at("13"),
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(BookingError::Over { .. }))
        ),
        "{refused:?}"
    );
    tx.rollback().await.expect("rolls back");

    // Nothing priced: refused.
    let unpriced = code("RES-B3");
    reserve(
        &fixture.db,
        &unpriced,
        &booking_for(Some("CUST-1"), vec![line("قص", "14", "15", &["stylist-1"])]),
        &Metadata::default(),
    )
    .await
    .expect("books unpriced");
    let mut tx = fixture.db.begin().await.expect("transaction");
    let refused = booking::bill_in(
        &mut tx,
        &unpriced,
        &code("bk-RES-B3"),
        at("15"),
        &Metadata::default(),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(ExecuteError::Rejected(BookingError::NothingToBill(_)))
        ),
        "{refused:?}"
    );
    tx.rollback().await.expect("rolls back");

    // The read model says which booking was billed, and the worklist of
    // completed, priced, unbilled bookings does not list it.
    let completed_unbilled = fixture.book_priced("RES-B4", riyals(150), "16", "17").await;
    for stage in [
        Stage::Confirmed,
        Stage::Arrived,
        Stage::InService,
        Stage::Completed,
    ] {
        booking::move_to(
            &fixture.db,
            &completed_unbilled,
            stage,
            "",
            at("17"),
            &Metadata::default(),
        )
        .await
        .expect("moves");
    }
    fixture.project().await;
    let mut conn = fixture.db.read().await.expect("connection");
    let detail = booking::reservation(&mut conn, billed.as_str())
        .await
        .expect("reads")
        .expect("there");
    assert_eq!(detail.summary.billed_by.as_deref(), Some("bk-RES-B1"));
    let waiting = booking::unbilled_completions(&mut conn, 10)
        .await
        .expect("reads");
    assert_eq!(
        waiting,
        vec![completed_unbilled],
        "billed, cancelled and unpriced are not work"
    );

    fixture.cleanup().await;
}

/// **What holding the slot costs, worked out when the slot is taken.** A
/// fraction of what the booking was priced at, before tax — and stamped on the
/// booking, so a business that changes what it asks for next month has not
/// changed what this booking asked for.
#[tokio::test]
async fn a_booking_records_the_deposit_it_was_asked_for() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;

    let id = fixture.book_priced("RES-D1", riyals(200), "10", "11").await;
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    let owed = booking::awaiting_deposit(&mut conn, id.as_str())
        .await
        .expect("reads")
        .expect("a deposit was asked for");
    // Twenty per cent of 200, before tax.
    assert_eq!(owed.deposit, riyals(40));
    assert!(owed.due_by > at("08"), "the hold has a deadline");

    fixture.cleanup().await;
}

/// A business that asks for nothing gets nothing, and no hold can lapse.
#[tokio::test]
async fn a_business_that_asks_for_no_deposit_records_none() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 0,
            hold_minutes: 30,
        })
        .await;

    let id = fixture.book_priced("RES-D2", riyals(200), "10", "11").await;
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    assert!(
        booking::awaiting_deposit(&mut conn, id.as_str())
            .await
            .expect("reads")
            .is_none(),
        "a deposit was invented"
    );
    let lapsed = booking::lapsed_holds(&mut conn, at("08") + chrono::Duration::days(3_650), 10)
        .await
        .expect("reads");
    assert!(lapsed.is_empty(), "{lapsed:?}");
    drop(conn);

    fixture.cleanup().await;
}

/// **A slot nobody paid for is released; one that was paid for is not.** That
/// is the whole point of asking: a held slot is one nobody else can take.
#[tokio::test]
async fn an_unpaid_hold_lapses_and_a_paid_one_does_not() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;

    let unpaid = fixture.book_priced("RES-D3", riyals(200), "10", "11").await;
    let paid = fixture.book_priced("RES-D4", riyals(200), "12", "13").await;

    let mut tx = fixture.db.begin().await.expect("transaction");
    booking::secure_in(
        &mut tx,
        &paid,
        &code("pay-1"),
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("secures");
    tx.commit().await.expect("commits");
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    let lapsed = booking::lapsed_holds(&mut conn, at("08") + chrono::Duration::hours(2), 10)
        .await
        .expect("reads");
    drop(conn);

    assert_eq!(lapsed.len(), 1, "{lapsed:?}");
    assert_eq!(lapsed[0].id, unpaid, "the paid one was released");

    fixture.cleanup().await;
}

/// **Told, not read.** Whether the money arrived is a fact this module is
/// given, which is what lets a diary answer "has this been paid for" without
/// reading another projection group (L3).
#[tokio::test]
async fn securing_a_booking_is_recorded_and_the_second_payment_changes_nothing() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;
    let id = fixture.book_priced("RES-D5", riyals(200), "10", "11").await;

    for payment in ["pay-1", "pay-2"] {
        let mut tx = fixture.db.begin().await.expect("transaction");
        booking::secure_in(&mut tx, &id, &code(payment), at("09"), &Metadata::default())
            .await
            .expect("secures");
        tx.commit().await.expect("commits");
    }
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    assert!(
        booking::awaiting_deposit(&mut conn, id.as_str())
            .await
            .expect("reads")
            .is_none(),
        "it still looks unpaid"
    );
    drop(conn);

    // **The first payment holds it.** A second is money to give back, not a
    // fact about this booking — so the id recorded is the first one.
    let held: Option<String> =
        sqlx::query_scalar("SELECT secured_by FROM proj_booking.reservation WHERE id = $1")
            .bind(id.as_str())
            .fetch_one(&fixture.pool)
            .await
            .expect("reads");
    assert_eq!(held.as_deref(), Some("pay-1"));

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Proving a phone number, when the business asks for one
// ---------------------------------------------------------------------------

/// **A code is spent once**, whatever races for it.
#[tokio::test]
async fn a_verification_code_holds_one_booking_and_no_more() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    let issued = booking::verification::issue(&mut conn, "+966 50 000 0000", at("08"))
        .await
        .expect("issues");
    assert_eq!(issued.handle, "+966500000000", "the number was not tidied");

    booking::verification::claim(&mut conn, "+966500000000", &issued.code, at("08"))
        .await
        .expect("the code is good");

    let again =
        booking::verification::claim(&mut conn, "+966500000000", &issued.code, at("08")).await;
    assert!(again.is_err(), "one code held two bookings");

    fixture.cleanup().await;
}

/// **Wrong, expired and never-issued are one answer.** Telling them apart tells
/// somebody guessing which half of the pair they got right.
#[tokio::test]
async fn a_code_that_is_wrong_or_stale_is_refused_the_same_way() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    let issued = booking::verification::issue(&mut conn, "+966500000001", at("08"))
        .await
        .expect("issues");

    // The wrong code.
    assert!(
        booking::verification::claim(&mut conn, "+966500000001", "000000", at("08"))
            .await
            .is_err()
    );
    // The right code, for a different number.
    assert!(
        booking::verification::claim(&mut conn, "+966500000002", &issued.code, at("08"))
            .await
            .is_err()
    );
    // Never issued at all.
    assert!(
        booking::verification::claim(&mut conn, "+966500000003", "123456", at("08"))
            .await
            .is_err()
    );

    fixture.cleanup().await;
}

/// **Guessing costs the code.** Twenty bits against unlimited attempts is
/// minutes; against a handful it is one in two hundred thousand.
#[tokio::test]
async fn a_code_dies_after_a_handful_of_wrong_guesses() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    let issued = booking::verification::issue(&mut conn, "+966500000004", at("08"))
        .await
        .expect("issues");

    for _ in 0..booking::verification::MAX_ATTEMPTS {
        let _ = booking::verification::claim(&mut conn, "+966500000004", "000000", at("08")).await;
    }

    // **Even the right one**, because the code is dead and not merely wrong.
    assert!(
        booking::verification::claim(&mut conn, "+966500000004", &issued.code, at("08"))
            .await
            .is_err(),
        "a code survived being guessed at"
    );

    fixture.cleanup().await;
}

/// **A resend button that works instantly is one somebody holds down**, and
/// every text costs the business money.
#[tokio::test]
async fn a_second_code_is_refused_until_the_cooldown_passes() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    booking::verification::issue(&mut conn, "+966500000005", at("08"))
        .await
        .expect("issues");
    assert!(
        booking::verification::issue(&mut conn, "+966500000005", at("08"))
            .await
            .is_err(),
        "a second code went out immediately"
    );

    // And after the cooldown it is allowed again.
    let later =
        at("08") + chrono::Duration::seconds(booking::verification::REQUEST_INTERVAL_SECONDS + 1);
    booking::verification::issue(&mut conn, "+966500000005", later)
        .await
        .expect("the cooldown passed");

    fixture.cleanup().await;
}

/// A code past its lifetime is no code at all.
#[tokio::test]
async fn a_code_stops_working_when_it_expires() {
    let fixture = Fixture::new().await;
    let mut conn = fixture.db.acquire().await.expect("connection");

    let issued = booking::verification::issue(&mut conn, "+966500000006", at("08"))
        .await
        .expect("issues");
    let later =
        at("08") + chrono::Duration::seconds(booking::verification::CODE_LIFETIME_SECONDS + 1);

    assert!(
        booking::verification::claim(&mut conn, "+966500000006", &issued.code, later)
            .await
            .is_err(),
        "an expired code was accepted"
    );

    // And the sweep takes it away, because an expired code is evidence of
    // nothing.
    let gone = booking::verification::sweep(&mut conn, later)
        .await
        .expect("sweeps");
    assert!(gone > 0);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Bars — a specialist a customer must not be booked with
// ---------------------------------------------------------------------------

impl Fixture {
    async fn bar(&self, customer: &str, resource: &str) {
        booking::raise_bar(
            &self.db,
            &code(customer),
            &code(resource),
            "شكوى",
            at("00"),
            &Metadata::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{customer} should be barrable from {resource}: {e}"));
    }

    async fn bars(&self, customer: &str) -> Vec<booking::Bar> {
        let mut conn = self.pool.acquire().await.expect("connection");
        booking::bars(&mut conn, customer).await.expect("reads")
    }
}

/// **The bar refuses the booking, and takes nothing.**
///
/// The second half is the one that would rot quietly: `erp_occupancy` writes as
/// it goes, so a refusal that left a claim behind would hold a chair nobody
/// could see or free.
#[tokio::test]
async fn a_barred_specialist_cannot_be_booked_and_the_refusal_holds_nothing() {
    let fixture = Fixture::new().await;
    fixture.bar("CUST-1", "noura").await;

    let refused = reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["noura"])]),
        &Metadata::default(),
    )
    .await
    .expect_err("a barred stylist was booked");

    assert!(
        matches!(
            rejection(&refused),
            Some(BookingError::Barred { resource }) if resource == "noura"
        ),
        "{refused:?}"
    );
    assert_eq!(
        fixture.free("noura", "10", "11").await,
        1,
        "the refused booking left a claim behind"
    );
    // Somebody else's booking with the same stylist is untouched: a bar is
    // between two named things and not a withdrawal.
    crm::register_customer(
        &fixture.db,
        &code("CUST-2"),
        &crm::Details {
            name: "هدى".to_owned(),
            name_latin: None,
            kind: crm::CustomerKind::Person,
            contact: crm::Contact {
                phone: Some("+966522222222".to_owned()),
                email: None,
            },
            address: None,
            tax: None,
        },
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect("on file");
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(Some("CUST-2"), vec![line("قص", "10", "11", &["noura"])]),
        &Metadata::default(),
    )
    .await
    .expect("a bar against one customer refused another's booking");

    fixture.cleanup().await;
}

/// **Both ways round it.** A booking taken on a clear stylist and *moved* onto
/// a barred one, and a pool booked by the type with the barred unit named
/// afterwards — each is a door the check in `reserve` alone leaves open.
#[tokio::test]
async fn a_bar_cannot_be_walked_round_by_rescheduling_or_by_assigning() {
    let fixture = Fixture::new().await;
    fixture.declare("noura-2", &person("نورة ٢")).await;
    fixture.declare("stylists", &place("أي مصففة", 2)).await;

    // In through the front door: booked on a stylist who is not barred.
    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["noura-2"])]),
        &Metadata::default(),
    )
    .await
    .expect("noura-2 is clear");

    fixture.bar("CUST-1", "noura").await;

    let refused = reschedule(
        &fixture.db,
        &code("BK-1"),
        &[line("قص", "12", "13", &["noura"])],
        at("12"),
        &Metadata::default(),
    )
    .await
    .expect_err("rescheduled onto a barred stylist");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Barred { .. })),
        "{refused:?}"
    );
    assert_eq!(
        fixture.free("noura-2", "10", "11").await,
        0,
        "the refused reschedule gave the old claim away"
    );

    // And out through the pool: "any stylist" names nothing barred.
    reserve(
        &fixture.db,
        &code("BK-2"),
        &booking_for(Some("CUST-1"), vec![line("صبغ", "14", "15", &["stylists"])]),
        &Metadata::default(),
    )
    .await
    .expect("the pool is not barred");

    let refused = assign(
        &fixture.db,
        &code("BK-2"),
        0,
        &code("noura"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect_err("the barred stylist was picked out of the pool");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Barred { .. })),
        "{refused:?}"
    );
    assert_eq!(
        fixture.free("noura", "14", "15").await,
        1,
        "the refused assignment took the unit anyway"
    );

    // The clear one out of the same pool goes through.
    assign(
        &fixture.db,
        &code("BK-2"),
        0,
        &code("noura-2"),
        at("14"),
        &Metadata::default(),
    )
    .await
    .expect("noura-2 is clear");

    fixture.cleanup().await;
}

/// **Raised twice is one bar, and one lift ends it.**
///
/// The projection is checked alongside, because a screen that still shows a
/// lifted bar is how somebody comes to explain a refusal that is not happening.
#[tokio::test]
async fn a_bar_is_raised_once_however_often_it_is_asked_for_and_lifting_ends_it() {
    let fixture = Fixture::new().await;

    fixture.bar("CUST-1", "noura").await;
    let again = booking::raise_bar(
        &fixture.db,
        &code("CUST-1"),
        &code("noura"),
        "شكوى أخرى",
        at("01"),
        &Metadata::default(),
    )
    .await
    .expect("the retry is quiet");
    assert!(again.at.is_none(), "the second raise wrote an event");

    fixture.project().await;
    let listed = fixture.bars("CUST-1").await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].resource, "noura");
    assert_eq!(listed[0].name.as_deref(), Some("نورة"));
    assert_eq!(
        listed[0].why, "شكوى",
        "the second reason overwrote the first"
    );

    booking::lift_bar(
        &fixture.db,
        &code("CUST-1"),
        &code("noura"),
        at("02"),
        &Metadata::default(),
    )
    .await
    .expect("lifts");

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["noura"])]),
        &Metadata::default(),
    )
    .await
    .expect("the bar was lifted and the booking should go through");

    fixture.project().await;
    assert!(
        fixture.bars("CUST-1").await.is_empty(),
        "the lifted bar is still on the screen"
    );

    // Lifting one that is not there is a no-op, not a refusal.
    let noop = booking::lift_bar(
        &fixture.db,
        &code("CUST-1"),
        &code("noura"),
        at("03"),
        &Metadata::default(),
    )
    .await
    .expect("quiet");
    assert!(noop.at.is_none());

    fixture.cleanup().await;
}

/// **A bar needs two things the system can name, and a reason.**
///
/// A typo in either id would otherwise be written and enforce nothing, and
/// whoever typed it would go away believing a rule was in place.
#[tokio::test]
async fn a_bar_against_nobody_or_nothing_is_refused() {
    let fixture = Fixture::new().await;

    let refused = booking::raise_bar(
        &fixture.db,
        &code("CUST-404"),
        &code("noura"),
        "شكوى",
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect_err("barred a customer who does not exist");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NoSuchCustomer(_))),
        "{refused:?}"
    );

    let refused = booking::raise_bar(
        &fixture.db,
        &code("CUST-1"),
        &code("nobody"),
        "شكوى",
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect_err("barred a resource that does not exist");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NoSuchResource(_))),
        "{refused:?}"
    );

    let refused = booking::raise_bar(
        &fixture.db,
        &code("CUST-1"),
        &code("noura"),
        "   ",
        at("00"),
        &Metadata::default(),
    )
    .await
    .expect_err("barred with no reason");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NoReasonToBar)),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **A walk-in cannot be barred, and this says so out loud.**
///
/// Bars are keyed on a `crm` record, so a booking that carries only a typed-in
/// name goes through however many bars stand against the person it is probably
/// for. That is the honest limit of recognising somebody, and it is written
/// down here rather than discovered.
#[tokio::test]
async fn a_booking_with_no_customer_record_is_not_barred_from_anything() {
    let fixture = Fixture::new().await;
    fixture.bar("CUST-1", "noura").await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(None, vec![line("قص", "10", "11", &["noura"])]),
        &Metadata::default(),
    )
    .await
    .expect("a walk-in is nobody the system can bar");

    fixture.cleanup().await;
}

/// **A bar is about the next booking, not the ones already in the diary.**
///
/// Raising one does not cancel Tuesday's appointment, and it should not: what
/// to do about a booking that already exists is a conversation somebody has to
/// have, and a system that silently emptied the diary would be making that
/// decision for them. Written down here because the surprise is otherwise
/// discovered on Tuesday.
#[tokio::test]
async fn raising_a_bar_leaves_the_bookings_that_were_already_made() {
    let fixture = Fixture::new().await;

    reserve(
        &fixture.db,
        &code("BK-1"),
        &booking_for(Some("CUST-1"), vec![line("قص", "10", "11", &["noura"])]),
        &Metadata::default(),
    )
    .await
    .expect("nothing is barred yet");

    fixture.bar("CUST-1", "noura").await;

    fixture.project().await;
    let detail = fixture.get("BK-1").await.expect("still in the diary");
    assert_eq!(detail.summary.stage, Stage::Reserved.as_str());
    assert_eq!(
        fixture.free("noura", "10", "11").await,
        0,
        "raising a bar gave the chair back"
    );

    // Cancelling it is the ordinary command, and it still works.
    move_to(
        &fixture.db,
        &code("BK-1"),
        Stage::Cancelled,
        "حاجز",
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("a barred pair can still be cancelled");
    assert_eq!(fixture.free("noura", "10", "11").await, 1);

    fixture.cleanup().await;
}

// ---------------------------------------------------------------------------
// Lapsing is a decision against the log, not against a read model
// ---------------------------------------------------------------------------

/// **A paid booking cannot lapse, whatever the read model says.**
///
/// The hold-expiry job decides *which* bookings to look at from the
/// projection. The first version then cancelled through `move_to`, which does
/// what it is told; a deposit that settled between the projection being read
/// and the cancellation being written was cancelled anyway — with the money
/// taken. `lapse` re-asks the log and refuses.
#[tokio::test]
async fn a_paid_hold_cannot_be_lapsed_even_by_a_job_that_thinks_it_is_unpaid() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;
    let id = fixture.book_priced("RES-L1", riyals(200), "10", "11").await;

    // Paid — but the projection has NOT been run, so a job reading it would
    // still list this hold as unpaid and past due.
    let mut tx = fixture.db.begin().await.expect("transaction");
    booking::secure_in(&mut tx, &id, &code("pay-1"), at("09"), &Metadata::default())
        .await
        .expect("secures");
    tx.commit().await.expect("commits");

    let long_after = at("08") + chrono::Duration::hours(3);
    let refused = booking::lapse(&fixture.db, &id, long_after, &Metadata::default())
        .await
        .expect_err("a paid booking was lapsed");
    assert!(
        matches!(rejection(&refused), Some(BookingError::Secured(_))),
        "{refused:?}"
    );
    assert_eq!(
        fixture.free("stylist-1", "10", "11").await,
        0,
        "the paid slot was released"
    );

    fixture.cleanup().await;
}

/// **A hold lapses only after its deadline, and only while it is a hold.**
#[tokio::test]
async fn a_hold_lapses_only_after_its_deadline() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;
    let id = fixture.book_priced("RES-L2", riyals(200), "10", "11").await;

    // Ten minutes in: not yet.
    let too_early = at("08") + chrono::Duration::minutes(10);
    let refused = booking::lapse(&fixture.db, &id, too_early, &Metadata::default())
        .await
        .expect_err("lapsed before the deadline");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NotLapsed(_))),
        "{refused:?}"
    );
    assert_eq!(fixture.free("stylist-1", "10", "11").await, 0);

    // Past the deadline: released, reason written down, chair back.
    let past_due = at("08") + chrono::Duration::hours(2);
    let lapsed = booking::lapse(&fixture.db, &id, past_due, &Metadata::default())
        .await
        .expect("lapses");
    assert!(lapsed.at.is_some());
    assert_eq!(fixture.free("stylist-1", "10", "11").await, 1);

    // And again is nothing: the booking is over.
    let again = booking::lapse(&fixture.db, &id, past_due, &Metadata::default())
        .await
        .expect("quiet");
    assert!(again.at.is_none());

    fixture.project().await;
    let detail = fixture.get("RES-L2").await.expect("there");
    assert_eq!(detail.summary.stage, Stage::Cancelled.as_str());

    fixture.cleanup().await;
}

/// A booking the business has **confirmed** is a promise the business made; a
/// deposit that never arrived is then a conversation, not an automatic release.
#[tokio::test]
async fn a_confirmed_booking_does_not_lapse() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    fixture
        .set_public(booking::PublicBooking {
            verify_phone: false,
            open: true,
            deposit_bp: 2_000,
            hold_minutes: 30,
        })
        .await;
    let id = fixture.book_priced("RES-L3", riyals(200), "10", "11").await;
    move_to(
        &fixture.db,
        &id,
        Stage::Confirmed,
        "",
        at("08"),
        &Metadata::default(),
    )
    .await
    .expect("confirms");

    let refused = booking::lapse(
        &fixture.db,
        &id,
        at("08") + chrono::Duration::hours(2),
        &Metadata::default(),
    )
    .await
    .expect_err("a confirmed booking lapsed");
    assert!(
        matches!(rejection(&refused), Some(BookingError::NotLapsed(_))),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// **Money for a booking that is over is refused, not recorded.** The worker
/// logs the refusal, which is the one place somebody will see that a customer
/// paid for a booking they no longer have.
#[tokio::test]
async fn a_deposit_arriving_for_a_cancelled_booking_is_refused() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    let id = fixture.book_priced("RES-L4", riyals(200), "10", "11").await;
    move_to(
        &fixture.db,
        &id,
        Stage::Cancelled,
        "changed their mind",
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("cancels");

    let mut tx = fixture.db.begin().await.expect("transaction");
    let refused = booking::secure_in(
        &mut tx,
        &id,
        &code("pay-late"),
        at("10"),
        &Metadata::default(),
    )
    .await
    .expect_err("a cancelled booking was marked paid");
    tx.rollback().await.expect("rolls back");
    assert!(
        matches!(refused, ExecuteError::Rejected(BookingError::Over { .. })),
        "{refused:?}"
    );

    fixture.cleanup().await;
}

/// The half of the durable repair that `booking` answers: of these bookings,
/// which have not been told their deposit arrived.
#[tokio::test]
async fn unsecured_among_names_the_bookings_not_yet_told() {
    let fixture = Fixture::new().await;
    fixture.declare("stylist-1", &person("نورة")).await;
    let told = fixture.book_priced("RES-L5", riyals(200), "10", "11").await;
    let untold = fixture.book_priced("RES-L6", riyals(200), "12", "13").await;

    let mut tx = fixture.db.begin().await.expect("transaction");
    booking::secure_in(
        &mut tx,
        &told,
        &code("pay-1"),
        at("09"),
        &Metadata::default(),
    )
    .await
    .expect("secures");
    tx.commit().await.expect("commits");
    fixture.project().await;

    let mut conn = fixture.db.read().await.expect("connection");
    let answer = booking::unsecured_among(
        &mut conn,
        &[told.to_string(), untold.to_string(), "RES-NOPE".to_owned()],
    )
    .await
    .expect("reads");
    assert_eq!(
        answer,
        vec![untold],
        "only the untold one, and nothing that does not exist"
    );

    fixture.cleanup().await;
}
