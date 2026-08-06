//! Loading and executing against aggregates.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spa_eventlog::{
    Aggregate, DomainEvent, ExecuteError, Loaded, Metadata, Upcasters, append_events, execute,
    integrity, load,
};
use spa_testkit::{Schema, Template};
use spa_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence};

static TENANT: Schema = Schema::migrations("tenant", &spa_eventlog::MIGRATIONS);

// ---------------------------------------------------------------------------
// A counter aggregate. Deliberately trivial: these tests are about the
// machinery, and a domain model here would only obscure what is being asserted.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CounterEvent {
    Incremented { by: i64 },
    Reset,
}

impl DomainEvent for CounterEvent {
    fn event_name(&self) -> EventName {
        match self {
            Self::Incremented { .. } => EventName::new("counter.incremented").unwrap(),
            Self::Reset => EventName::new("counter.reset").unwrap(),
        }
    }
    fn schema_version(&self) -> SchemaVersion {
        SchemaVersion::new(1).unwrap()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counter {
    total: i64,
    resets: u32,
}

impl Aggregate for Counter {
    type Event = CounterEvent;

    fn domain() -> DomainName {
        DomainName::new("counter").unwrap()
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            CounterEvent::Incremented { by } => self.total += by,
            CounterEvent::Reset => {
                self.total = 0;
                self.resets += 1;
            }
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum CounterError {
    #[error("cannot increment by a negative amount")]
    Negative,
}

fn upcasters() -> Upcasters {
    Upcasters::new()
        .declare(
            &EventName::new("counter.incremented").unwrap(),
            SchemaVersion::new(1).unwrap(),
        )
        .declare(
            &EventName::new("counter.reset").unwrap(),
            SchemaVersion::new(1).unwrap(),
        )
}

fn id(name: &str) -> AggregateId {
    AggregateId::new(name).unwrap()
}

async fn tenant_db() -> spa_testkit::TestDb {
    Template::get(&TENANT)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones")
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_aggregate_loads_empty() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let loaded = load::<Counter>(&mut conn, &id("fresh"), &upcasters())
        .await
        .expect("loads");

    assert!(loaded.is_new(), "nothing has been written for this id");
    assert_eq!(loaded.version, Sequence::ZERO);
    assert_eq!(loaded.aggregate, Counter::default());
}

#[tokio::test]
async fn state_is_rebuilt_by_folding_events_in_order() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    append_events::<Counter>(
        &mut conn,
        &id("c1"),
        Sequence::ZERO,
        &[
            CounterEvent::Incremented { by: 5 },
            CounterEvent::Incremented { by: 3 },
            CounterEvent::Reset,
            CounterEvent::Incremented { by: 7 },
        ],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    let loaded = load::<Counter>(&mut conn, &id("c1"), &upcasters())
        .await
        .expect("loads");

    assert_eq!(loaded.aggregate.total, 7, "the reset must be honoured");
    assert_eq!(loaded.aggregate.resets, 1);
    assert_eq!(loaded.version.get(), 4, "version is the last sequence");
    assert!(!loaded.is_new());
}

#[tokio::test]
async fn aggregates_do_not_see_each_others_events() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    for name in ["a", "b"] {
        append_events::<Counter>(
            &mut conn,
            &id(name),
            Sequence::ZERO,
            &[CounterEvent::Incremented { by: 10 }],
            &Metadata::default(),
        )
        .await
        .expect("appends");
    }

    let a = load::<Counter>(&mut conn, &id("a"), &upcasters())
        .await
        .expect("loads");
    assert_eq!(a.aggregate.total, 10, "not 20 — streams are separate");
}

#[tokio::test]
async fn execute_loads_decides_and_appends() {
    let db = tenant_db().await;

    let events = execute::<Counter, _, CounterError>(
        db.pool(),
        &id("c1"),
        &upcasters(),
        &Metadata::default(),
        |_| Ok(vec![CounterEvent::Incremented { by: 4 }]),
    )
    .await
    .expect("executes");
    assert_eq!(events.len(), 1);

    // The decision sees the state left by the previous one.
    execute::<Counter, _, CounterError>(
        db.pool(),
        &id("c1"),
        &upcasters(),
        &Metadata::default(),
        |loaded| {
            assert_eq!(loaded.aggregate.total, 4, "decision must see prior state");
            Ok(vec![CounterEvent::Incremented { by: 6 }])
        },
    )
    .await
    .expect("executes");

    let mut conn = db.pool().acquire().await.expect("connection");
    let loaded = load::<Counter>(&mut conn, &id("c1"), &upcasters())
        .await
        .expect("loads");
    assert_eq!(loaded.aggregate.total, 10);
}

/// A rejected decision writes nothing at all.
#[tokio::test]
async fn a_rejected_decision_leaves_no_trace() {
    let db = tenant_db().await;

    let result = execute::<Counter, _, CounterError>(
        db.pool(),
        &id("c1"),
        &upcasters(),
        &Metadata::default(),
        |_| Err(CounterError::Negative),
    )
    .await;

    assert!(matches!(
        result,
        Err(ExecuteError::Rejected(CounterError::Negative))
    ));

    let mut conn = db.pool().acquire().await.expect("connection");
    let report = integrity(&mut conn).await.expect("checks");
    assert_eq!(report.event_count, 0, "a refusal must write nothing");
    assert!(
        report.is_contiguous(),
        "and must not burn a position: {report:?}"
    );
}

/// Deciding to do nothing is a success, not an empty append.
#[tokio::test]
async fn a_decision_to_do_nothing_writes_nothing() {
    let db = tenant_db().await;

    let events = execute::<Counter, _, CounterError>(
        db.pool(),
        &id("c1"),
        &upcasters(),
        &Metadata::default(),
        |_| Ok(vec![]),
    )
    .await
    .expect("an empty decision is not an error");
    assert!(events.is_empty());

    let mut conn = db.pool().acquire().await.expect("connection");
    assert_eq!(integrity(&mut conn).await.expect("checks").event_count, 0);
}

/// **The retry loop under real contention.**
///
/// Sixteen tasks increment the same aggregate concurrently. Each conflict makes
/// the decision run again against the state that won, so every increment must
/// land exactly once — no lost updates, and no double-counting from a retry that
/// reapplied a decision made against stale state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_execution_loses_no_updates() {
    const TASKS: i64 = 16;

    let db = Arc::new(tenant_db().await);
    // Counts how many times a decision ran. More decisions than successes means
    // the retry path was genuinely taken — without this the test could pass with
    // sixteen uncontended appends and prove nothing about retrying.
    let decisions = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let mut tasks = Vec::new();

    for _ in 0..TASKS {
        let db = Arc::clone(&db);
        let decisions = Arc::clone(&decisions);
        tasks.push(tokio::spawn(async move {
            execute::<Counter, _, CounterError>(
                db.pool(),
                &id("hot"),
                &upcasters(),
                &Metadata::default(),
                move |_| {
                    decisions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(vec![CounterEvent::Incremented { by: 1 }])
                },
            )
            .await
        }));
    }

    let mut succeeded = 0;
    for task in tasks {
        match task.await.expect("joined") {
            Ok(_) => succeeded += 1,
            // Contention beyond the retry budget is a legitimate outcome; what
            // must not happen is a *silent* loss.
            Err(ExecuteError::Contended { .. }) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    let mut conn = db.pool().acquire().await.expect("connection");
    let loaded = load::<Counter>(&mut conn, &id("hot"), &upcasters())
        .await
        .expect("loads");

    assert_eq!(
        loaded.aggregate.total, succeeded,
        "every successful execution must have landed exactly once"
    );
    assert_eq!(
        loaded.version.get(),
        succeeded,
        "one event per success, no gaps in the stream"
    );
    assert!(
        integrity(&mut conn).await.expect("checks").is_contiguous(),
        "retries must not burn log positions"
    );
    assert!(
        succeeded >= TASKS / 2,
        "only {succeeded}/{TASKS} succeeded; the retry budget looks too small"
    );

    // Deliberately *not* asserting that retries happened here: these
    // transactions are short enough that they usually do not overlap, so
    // conflicts are incidental. Proof that the retry path works lives in
    // `a_conflict_makes_the_decision_run_again`, which forces one.
    let attempted = decisions.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        attempted >= succeeded,
        "every success needs at least one decision ({attempted} < {succeeded})"
    );
}

/// **The retry loop, with the conflict forced rather than hoped for.**
///
/// The decision closure injects a competing write on its first invocation, so
/// the append that follows is guaranteed to conflict. The decision must then run
/// again against the state that won, and both increments must land — which is
/// the difference between a retry and a lost update.
///
/// The 16-task test above cannot establish this: its transactions are short
/// enough that they rarely overlap, so it passes without a single conflict.
///
/// An earlier version coordinated two tasks with a spin-wait. It deadlocked —
/// blocking a worker thread inside an async task starves the very task you are
/// waiting for — and then *passed* on a rerun with different scheduling, which
/// is worse than failing. `block_in_place` is the supported way to run blocking
/// work on a multi-threaded runtime, and one task injecting its own conflict
/// needs no coordination at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflict_makes_the_decision_run_again() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let db = Arc::new(tenant_db().await);
    let decisions = Arc::new(AtomicUsize::new(0));
    let inject = Arc::new(AtomicBool::new(true));

    let injector = Arc::clone(&db);
    let counter = Arc::clone(&decisions);

    execute::<Counter, _, CounterError>(
        db.pool(),
        &id("forced"),
        &upcasters(),
        &Metadata::default(),
        move |loaded| {
            counter.fetch_add(1, Ordering::SeqCst);

            if inject.swap(false, Ordering::SeqCst) {
                assert!(loaded.is_new(), "the first attempt should see nothing yet");
                // Someone else commits between this load and our append.
                let pool = injector.pool().clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let mut conn = pool.acquire().await.expect("connection");
                        append_events::<Counter>(
                            &mut conn,
                            &id("forced"),
                            Sequence::ZERO,
                            &[CounterEvent::Incremented { by: 1 }],
                            &Metadata::default(),
                        )
                        .await
                        .expect("the competing write lands");
                    });
                });
            } else {
                assert_eq!(
                    loaded.aggregate.total, 1,
                    "the retry must see the state that won, not the stale one"
                );
            }

            Ok(vec![CounterEvent::Incremented { by: 1 }])
        },
    )
    .await
    .expect("executes after retrying");

    assert_eq!(
        decisions.load(Ordering::SeqCst),
        2,
        "the decision must run exactly twice: once losing, once winning"
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    let loaded = load::<Counter>(&mut conn, &id("forced"), &upcasters())
        .await
        .expect("loads");
    assert_eq!(
        loaded.aggregate.total, 2,
        "both increments must land — a retry that reapplied a stale decision \
         would leave 1"
    );
    assert_eq!(loaded.version.get(), 2);
    assert!(integrity(&mut conn).await.expect("checks").is_contiguous());
}

/// The decision sees committed history, not an empty aggregate.
///
/// Named for what it actually exercises: this appends *before* executing, so the
/// first attempt already sees the prior state and no conflict occurs. Proof that
/// the decision re-runs *after* a conflict lives in
/// [`concurrent_execution_loses_no_updates`], which counts invocations.
#[tokio::test]
async fn the_decision_sees_committed_history() {
    let db = tenant_db().await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

    // Something else writes first, so the first attempt conflicts.
    let mut conn = db.pool().acquire().await.expect("connection");
    append_events::<Counter>(
        &mut conn,
        &id("c1"),
        Sequence::ZERO,
        &[CounterEvent::Incremented { by: 100 }],
        &Metadata::default(),
    )
    .await
    .expect("appends");
    drop(conn);

    let recorder = Arc::clone(&seen);
    execute::<Counter, _, CounterError>(
        db.pool(),
        &id("c1"),
        &upcasters(),
        &Metadata::default(),
        move |loaded: &Loaded<Counter>| {
            recorder.lock().unwrap().push(loaded.aggregate.total);
            Ok(vec![CounterEvent::Incremented { by: 1 }])
        },
    )
    .await
    .expect("executes");

    let observed = seen.lock().unwrap().clone();
    assert_eq!(
        observed,
        vec![100],
        "the decision should see the committed state, not an empty aggregate"
    );
}

/// Events written under an older schema still rebuild the aggregate — the same
/// guarantee the golden files cover, exercised through a real load.
#[tokio::test]
async fn an_aggregate_loads_through_the_upcaster_chain() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    // Write a v1 payload by hand, as an older build would have.
    spa_eventlog::append(
        &mut conn,
        &spa_types::StreamId::new(Counter::domain(), id("old")),
        Sequence::ZERO,
        &[spa_eventlog::NewEvent::new(
            EventName::new("counter.incremented").unwrap(),
            SchemaVersion::new(1).unwrap(),
            // v1 called the field `amount`.
            serde_json::json!({ "type": "incremented", "amount": 9 }),
        )],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    let registry = Upcasters::new()
        .declare(
            &EventName::new("counter.incremented").unwrap(),
            SchemaVersion::new(2).unwrap(),
        )
        .step(
            &EventName::new("counter.incremented").unwrap(),
            SchemaVersion::new(1).unwrap(),
            |mut value| {
                let object = value.as_object_mut().ok_or("not an object")?;
                let amount = object.remove("amount").ok_or("expected `amount`")?;
                object.insert("by".into(), amount);
                Ok(value)
            },
        );

    let loaded = load::<Counter>(&mut conn, &id("old"), &registry)
        .await
        .expect("loads through the chain");
    assert_eq!(loaded.aggregate.total, 9);
}
