//! The occupancy engine, against a real database.
//!
//! Two tests carry this file, and they are the phase's exit criterion:
//! [`only_one_of_two_bookings_racing_for_the_last_place_gets_it`] and
//! [`a_deadlock_is_not_reachable`]. Everything else here is a rule that can be
//! reasoned about; those two are properties that only hold under concurrency,
//! which is where an availability check is actually used and where the system
//! this phase was read against got both of them wrong.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use erp_occupancy::{
    BadSpan, Claim, OccupancyError, Span, declare, free, release, reschedule, take,
};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::{AggregateId, Timestamp};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

async fn tenant_db() -> TestDb {
    Template::get(&TENANT)
        .await
        .expect("tenant template builds")
        .fresh()
        .await
        .expect("tenant database clones")
}

fn id(s: &str) -> AggregateId {
    AggregateId::new(s).expect("a valid id")
}

/// An hour of the one day most of these tests happen on.
fn at(hour: &str) -> Timestamp {
    day("2026-03-01", hour)
}

/// `10:00` to `11:00` on the one day most of these tests happen on.
fn span(from: &str, until: &str) -> Span {
    Span::new(at(from), at(until)).expect("a valid span")
}

fn day(date: &str, hour: &str) -> Timestamp {
    format!("{date}T{hour}:00:00Z")
        .parse()
        .expect("a valid instant")
}

/// **Capacity one is the salon case, and it is the one everybody gets right.**
///
/// It is here because it is the floor: if a second claim on a booked stylist
/// can land, nothing else in this file matters.
#[tokio::test]
async fn capacity_one_takes_one_and_refuses_the_second() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");

    declare(&mut tx, &id("stylist-noura"), 1)
        .await
        .expect("a stylist is declared");

    take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("stylist-noura"), span("10", "11"))],
    )
    .await
    .expect("the first booking fits");

    let refused = take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("stylist-noura"), span("10", "11"))],
    )
    .await
    .expect_err("the second booking must not fit");

    match refused {
        OccupancyError::Overbooked {
            capacity,
            held,
            wanted,
            ..
        } => {
            assert_eq!((capacity, held, wanted), (1, 1, 1));
        }
        other => panic!("expected Overbooked, got {other:?}"),
    }
}

/// **Half-open, so a salon can run appointments end to end.**
///
/// The alternative — closed intervals, and a one-second gap between every
/// appointment to make the arithmetic work — is a fudge that survives until
/// somebody books on the boundary.
#[tokio::test]
async fn back_to_back_claims_do_not_overlap() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("chair-1"), 1).await.expect("declared");

    take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-1"), span("10", "11"))],
    )
    .await
    .expect("the first hour fits");

    take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("chair-1"), span("11", "12"))],
    )
    .await
    .expect("the hour that starts where the last one ended fits too");

    // And the one that reaches back across the boundary does not.
    take(
        &mut tx,
        &id("res-3"),
        &[Claim::one(id("chair-1"), span("10", "12"))],
    )
    .await
    .expect_err("an hour that overlaps both must not fit");
}

/// **Capacity counts places, not bookings.**
///
/// A class of three takes three separate customers and refuses the fourth, and
/// one customer bringing two friends takes three places in one claim. That
/// system has no capacity at all, which is why it fits salons and nothing else.
#[tokio::test]
async fn capacity_counts_places_and_not_bookings() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("pilates-0900"), 3)
        .await
        .expect("declared");

    for who in ["res-1", "res-2", "res-3"] {
        take(
            &mut tx,
            &id(who),
            &[Claim::one(id("pilates-0900"), span("09", "10"))],
        )
        .await
        .unwrap_or_else(|e| panic!("{who} should fit in a class of three: {e}"));
    }

    take(
        &mut tx,
        &id("res-4"),
        &[Claim::one(id("pilates-0900"), span("09", "10"))],
    )
    .await
    .expect_err("a fourth in a class of three must not fit");

    assert_eq!(
        free(&mut tx, &id("pilates-0900"), span("09", "10"))
            .await
            .expect("free reads"),
        0
    );

    // And the same three places, taken by one booking for three people.
    let mut other = db.pool().begin().await.expect("transaction");
    declare(&mut other, &id("pilates-1000"), 3)
        .await
        .expect("declared");
    take(
        &mut other,
        &id("res-5"),
        &[Claim::many(id("pilates-1000"), span("10", "11"), 3)],
    )
    .await
    .expect("three places in one claim fit");
    take(
        &mut other,
        &id("res-6"),
        &[Claim::one(id("pilates-1000"), span("10", "11"))],
    )
    .await
    .expect_err("the class is full");
}

/// **The peak, not the sum, and a hotel is the case that proves it.**
///
/// Eight one-night stays spread across a week never coexist: one room is taken
/// on any given night. `SUM(quantity) over overlaps` counts all eight and turns
/// away a guest asking for the week, in a room type that has seven rooms free
/// every single night of it. The running total gets it right.
#[tokio::test]
async fn a_long_claim_fits_around_short_ones_that_never_coexist() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("double-room"), 8)
        .await
        .expect("declared");

    // One guest a night, Sunday to Saturday. Never two at once.
    for (n, night) in ["01", "02", "03", "04", "05", "06", "07", "08"]
        .iter()
        .enumerate()
    {
        let next = format!("2026-03-{:02}", n + 2);
        let stay = Span::new(day(&format!("2026-03-{night}"), "15"), day(&next, "11"))
            .expect("a night is a span");
        take(
            &mut tx,
            &id(&format!("res-{n}")),
            &[Claim::one(id("double-room"), stay)],
        )
        .await
        .unwrap_or_else(|e| panic!("night {night} should fit: {e}"));
    }

    // Eight claims overlap this week. Only ever one at a time.
    let week = Span::new(day("2026-03-01", "15"), day("2026-03-09", "11")).expect("a week");
    assert_eq!(
        free(&mut tx, &id("double-room"), week)
            .await
            .expect("free reads"),
        7,
        "a sum would say nothing is free; seven rooms are"
    );
    take(
        &mut tx,
        &id("res-week"),
        &[Claim::one(id("double-room"), week)],
    )
    .await
    .expect("a whole-week stay fits in a room type that is one-eighth full");
}

/// **The batch is checked against itself.**
///
/// That system probed the whole request and then wrote the whole request. One
/// request naming the same chair twice at the same hour found nothing already
/// held, wrote both claims, and double-booked the chair against itself. Its own
/// source records it.
#[tokio::test]
async fn a_batch_is_checked_against_itself() {
    let db = tenant_db().await;
    let mut setup = db.pool().begin().await.expect("transaction");
    declare(&mut setup, &id("chair-1"), 1)
        .await
        .expect("declared");
    setup.commit().await.expect("committed");

    // Each attempt gets its own transaction and is dropped unrolled back,
    // which is what a caller does with a refusal and what makes a half-written
    // batch never reach the database.
    let mut first = db.pool().begin().await.expect("transaction");
    take(
        &mut first,
        &id("res-1"),
        &[
            Claim::one(id("chair-1"), span("10", "11")),
            Claim::one(id("chair-1"), span("10", "11")),
        ],
    )
    .await
    .expect_err("one request must not double-book a chair against itself");
    drop(first);

    // Partly overlapping counts too — it does not have to be the same hour.
    let mut second = db.pool().begin().await.expect("transaction");
    take(
        &mut second,
        &id("res-2"),
        &[
            Claim::one(id("chair-1"), span("10", "12")),
            Claim::one(id("chair-1"), span("11", "13")),
        ],
    )
    .await
    .expect_err("overlapping lines in one request must not both land");
    drop(second);

    let mut after = db.pool().begin().await.expect("transaction");
    assert_eq!(
        free(&mut after, &id("chair-1"), span("09", "14"))
            .await
            .expect("free reads"),
        1,
        "a refused batch left something behind"
    );
}

/// **A refusal anywhere in a batch writes nothing anywhere.**
///
/// The engine writes each claim as it goes, so half a batch is genuinely in the
/// transaction when the second half is refused. The rollback is what makes the
/// booking all-or-nothing, which is why `take` says so twice.
#[tokio::test]
async fn a_batch_that_cannot_be_taken_whole_is_not_taken_at_all() {
    let db = tenant_db().await;
    let mut setup = db.pool().begin().await.expect("transaction");
    declare(&mut setup, &id("room-1"), 1)
        .await
        .expect("declared");
    declare(&mut setup, &id("nurse-1"), 1)
        .await
        .expect("declared");
    take(
        &mut setup,
        &id("res-0"),
        &[Claim::one(id("nurse-1"), span("10", "11"))],
    )
    .await
    .expect("the nurse is free");
    setup.commit().await.expect("committed");

    // The room is free, the nurse is not. The room must stay free.
    let mut attempt = db.pool().begin().await.expect("transaction");
    let refused = take(
        &mut attempt,
        &id("res-1"),
        &[
            Claim::one(id("room-1"), span("10", "11")),
            Claim::one(id("nurse-1"), span("10", "11")),
        ],
    )
    .await
    .expect_err("half a booking is no booking");
    assert!(matches!(refused, OccupancyError::Overbooked { .. }));
    drop(attempt);

    let mut after = db.pool().begin().await.expect("transaction");
    assert_eq!(
        free(&mut after, &id("room-1"), span("10", "11"))
            .await
            .expect("free reads"),
        1,
        "the room was claimed by a booking that was refused"
    );
}

/// **Release is by owner and idempotent**, so a retried cancellation is
/// harmless (L8) and a caller can tell "cancelled" from "already cancelled"
/// without asking first.
#[tokio::test]
async fn releasing_is_idempotent_and_gives_the_place_back() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("chair-1"), 1).await.expect("declared");
    take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-1"), span("10", "11"))],
    )
    .await
    .expect("booked");

    assert_eq!(release(&mut tx, &id("res-1")).await.expect("released"), 1);
    assert_eq!(
        release(&mut tx, &id("res-1"))
            .await
            .expect("released again"),
        0,
        "a second release must be a no-op, not an error"
    );
    assert_eq!(
        release(&mut tx, &id("res-never")).await.expect("released"),
        0
    );

    take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("chair-1"), span("10", "11"))],
    )
    .await
    .expect("the place came back");
}

/// **A booking never conflicts with where it already was.**
///
/// Moving an appointment ten minutes later overlaps its own current claim, so
/// a reschedule that probed before releasing would refuse every small move and
/// allow only the large ones.
#[tokio::test]
async fn a_reschedule_does_not_collide_with_where_it_was() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("chair-1"), 1).await.expect("declared");
    take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-1"), span("10", "11"))],
    )
    .await
    .expect("booked");

    reschedule(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-1"), span("10", "12"))],
    )
    .await
    .expect("a booking must not conflict with itself");

    assert_eq!(
        free(&mut tx, &id("chair-1"), span("11", "12"))
            .await
            .expect("free reads"),
        0,
        "the move did not take the new hour"
    );

    // And a reschedule that cannot fit leaves the booking where it was, which
    // is why the release and the take have to be one step.
    take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("chair-1"), span("14", "15"))],
    )
    .await
    .expect("an unrelated afternoon booking");
    let refused = reschedule(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-1"), span("14", "15"))],
    )
    .await
    .expect_err("the afternoon is taken");
    assert!(matches!(refused, OccupancyError::Overbooked { .. }));
}

/// **A claim across midnight is one claim, and it locks both days.**
#[tokio::test]
async fn a_claim_across_midnight_conflicts_with_the_next_morning() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("hall-1"), 1).await.expect("declared");

    let overnight =
        Span::new(day("2026-03-01", "22"), day("2026-03-02", "02")).expect("a valid span");
    take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("hall-1"), overnight)],
    )
    .await
    .expect("the night fits");

    let small_hours =
        Span::new(day("2026-03-02", "01"), day("2026-03-02", "03")).expect("a valid span");
    take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("hall-1"), small_hours)],
    )
    .await
    .expect_err("one in the morning is still last night's booking");
}

/// **Capacity can be lowered, and what is already held stands.**
///
/// A room that loses a bed does not evict the guest in it. Nothing more fits
/// until the claims end, which is the whole of what "lowered" can mean here.
#[tokio::test]
async fn lowering_capacity_leaves_the_claims_already_taken_standing() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("table-6"), 6).await.expect("declared");
    take(
        &mut tx,
        &id("res-1"),
        &[Claim::many(id("table-6"), span("19", "21"), 4)],
    )
    .await
    .expect("four covers fit at a table for six");

    declare(&mut tx, &id("table-6"), 2)
        .await
        .expect("the table is cut down to two");

    assert_eq!(
        free(&mut tx, &id("table-6"), span("19", "21"))
            .await
            .expect("free reads"),
        0,
        "a table for two holding four has nothing free, and nothing negative"
    );
    take(
        &mut tx,
        &id("res-2"),
        &[Claim::one(id("table-6"), span("19", "21"))],
    )
    .await
    .expect_err("nothing more fits");

    // Zero is how a resource is taken out of service.
    declare(&mut tx, &id("table-6"), 0)
        .await
        .expect("out of service");
    take(
        &mut tx,
        &id("res-3"),
        &[Claim::one(id("table-6"), span("22", "23"))],
    )
    .await
    .expect_err("a table out of service takes nothing, even when it is empty");
}

#[tokio::test]
async fn a_resource_nobody_declared_is_refused_by_name() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");

    let refused = take(
        &mut tx,
        &id("res-1"),
        &[Claim::one(id("chair-nobody-has"), span("10", "11"))],
    )
    .await
    .expect_err("an undeclared resource cannot be claimed");
    match refused {
        OccupancyError::NoSuchResource(name) => assert_eq!(name, "chair-nobody-has"),
        other => panic!("expected NoSuchResource, got {other:?}"),
    }

    let refused = free(&mut tx, &id("chair-nobody-has"), span("10", "11"))
        .await
        .expect_err("and it has no capacity to report");
    assert!(matches!(refused, OccupancyError::NoSuchResource(_)));
}

#[tokio::test]
async fn a_claim_for_none_of_something_is_refused() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");
    declare(&mut tx, &id("chair-1"), 1).await.expect("declared");

    let refused = take(
        &mut tx,
        &id("res-1"),
        &[Claim::many(id("chair-1"), span("10", "11"), 0)],
    )
    .await
    .expect_err("a booking for nobody is not a booking");
    assert!(matches!(refused, OccupancyError::NothingClaimed));
}

/// **The exit criterion, and the reason the guard rows exist.**
///
/// Two bookings arrive at the same instant for the last place. The probe is a
/// read, and two concurrent reads both see it free — so without a lock taken
/// before the probe, both commit and the resource is overbooked by exactly one.
/// That is the failure this whole design is arranged around, and it cannot be
/// found by reasoning about a single transaction.
///
/// Run at capacity 1 and capacity 3, because they fail differently: at 1 the
/// engine could get away with an existence check, and at 3 it cannot.
#[tokio::test]
async fn only_one_of_two_bookings_racing_for_the_last_place_gets_it() {
    // Four and no more: the test pool holds four connections, every contender
    // takes one before it waits at the gate, and a fifth would wait for a
    // connection the other four only give back after the gate opens.
    for (capacity, contenders) in [(1_u16, 2_usize), (3, 4)] {
        let db = tenant_db().await;
        let mut setup = db.pool().begin().await.expect("transaction");
        declare(&mut setup, &id("chair-1"), capacity)
            .await
            .expect("declared");
        setup.commit().await.expect("committed");

        let gate = Arc::new(tokio::sync::Barrier::new(contenders));
        let mut racing = Vec::new();
        for n in 0..contenders {
            let pool = db.pool().clone();
            let gate = Arc::clone(&gate);
            racing.push(tokio::spawn(async move {
                let mut tx = pool.begin().await.expect("transaction");
                gate.wait().await;
                let outcome = take(
                    &mut tx,
                    &id(&format!("res-{n}")),
                    &[Claim::one(id("chair-1"), span("10", "11"))],
                )
                .await;
                match outcome {
                    Ok(()) => {
                        tx.commit().await.expect("committed");
                        true
                    }
                    Err(OccupancyError::Overbooked { .. }) => false,
                    Err(other) => panic!("unexpected failure: {other}"),
                }
            }));
        }

        let mut won = 0;
        for task in racing {
            if task.await.expect("the task did not panic") {
                won += 1;
            }
        }
        assert_eq!(
            won,
            usize::from(capacity),
            "capacity {capacity} against {contenders} bookings at once"
        );

        let mut check = db.pool().begin().await.expect("transaction");
        assert_eq!(
            free(&mut check, &id("chair-1"), span("10", "11"))
                .await
                .expect("free reads"),
            0
        );
    }
}

/// **The locks are taken in sorted order, so two multi-resource bookings
/// cannot hold what each other wants.**
///
/// One request naming a room and a nurse, another naming the nurse and the
/// room: unsorted, each takes its first and waits forever on the other, and
/// Postgres kills one with `40P01`. It is that system's recorded bug and the
/// only fix is a total order.
///
/// The pool is deliberately larger than the pair and the rounds are repeated,
/// because a deadlock is a race and one round can miss it by luck. Removing
/// either sort in `lock_guards` fails this.
#[tokio::test]
async fn a_deadlock_is_not_reachable() {
    let db = tenant_db().await;
    let mut setup = db.pool().begin().await.expect("transaction");
    // Two, so both bookings can succeed and a failure means a real deadlock
    // rather than an ordinary refusal.
    declare(&mut setup, &id("room-a"), 2)
        .await
        .expect("declared");
    declare(&mut setup, &id("nurse-b"), 2)
        .await
        .expect("declared");
    setup.commit().await.expect("committed");

    for round in 0..25 {
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let mut racing = Vec::new();
        for (n, order) in [["room-a", "nurse-b"], ["nurse-b", "room-a"]]
            .into_iter()
            .enumerate()
        {
            let pool = db.pool().clone();
            let gate = Arc::clone(&gate);
            racing.push(tokio::spawn(async move {
                let mut tx = pool.begin().await.expect("transaction");
                gate.wait().await;
                let outcome = take(
                    &mut tx,
                    &id(&format!("res-{round}-{n}")),
                    &[
                        Claim::one(id(order[0]), span("10", "11")),
                        Claim::one(id(order[1]), span("10", "11")),
                    ],
                )
                .await;
                match outcome {
                    Ok(()) => tx.commit().await.expect("committed"),
                    Err(e) => panic!("round {round} booking {n} failed: {e}"),
                }
            }));
        }
        for task in racing {
            task.await.expect("neither booking deadlocked");
        }

        let mut clean = db.pool().begin().await.expect("transaction");
        for n in 0..2 {
            release(&mut clean, &id(&format!("res-{round}-{n}")))
                .await
                .expect("released");
        }
        clean.commit().await.expect("committed");
    }
}

/// **A span that is not one cannot be built**, so nothing downstream has to
/// check for it.
#[test]
fn a_span_refuses_what_would_make_an_overlap_check_meaningless() {
    assert_eq!(
        Span::new(at("11"), at("10")).expect_err("backwards"),
        BadSpan::Empty
    );
    assert_eq!(
        Span::new(day("2026-03-01", "10"), day("2028-03-01", "10")).expect_err("two years"),
        BadSpan::TooLong
    );
}
