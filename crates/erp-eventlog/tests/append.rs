//! Append behaviour, and the proof of architecture law L1.
//!
//! [`a_tailer_never_skips_an_event`] is the load-bearing test in this file and
//! arguably in the codebase: every projection, every replay, and the whole
//! reproducibility argument assumes a tailer sees an unbroken prefix of the log.
//! It is asserted here under real concurrency rather than reasoned about.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use erp_eventlog::{AppendError, Metadata, NewEvent, append, integrity, read_since, read_stream};
use erp_testkit::{Schema, Template};
use erp_types::{
    AggregateId, DomainName, EventName, LogPosition, SchemaVersion, Sequence, StreamId,
};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

async fn tenant_db() -> erp_testkit::TestDb {
    Template::get(&TENANT)
        .await
        .expect("tenant template builds")
        .fresh()
        .await
        .expect("tenant database clones")
}

fn stream(id: &str) -> StreamId {
    StreamId::new(
        DomainName::new("ledger_account").expect("valid"),
        AggregateId::new(id).expect("valid"),
    )
}

fn event(name: &str) -> NewEvent {
    NewEvent::new(
        EventName::new(name).expect("valid"),
        SchemaVersion::new(1).expect("valid"),
        serde_json::json!({ "n": name }),
    )
}

#[tokio::test]
async fn appended_events_get_contiguous_positions_and_sequences() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let written = append(
        &mut conn,
        &stream("1000"),
        Sequence::ZERO,
        &[event("a"), event("b"), event("c")],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    assert_eq!(written.len(), 3);
    for (i, envelope) in written.iter().enumerate() {
        let expected = i64::try_from(i).unwrap() + 1;
        assert_eq!(envelope.position.get(), expected, "positions start at 1");
        assert_eq!(envelope.sequence.get(), expected, "sequences start at 1");
    }

    // And a second batch continues from where the first stopped.
    let more = append(
        &mut conn,
        &stream("1000"),
        Sequence::new(3).unwrap(),
        &[event("d")],
        &Metadata::default(),
    )
    .await
    .expect("appends");
    assert_eq!(more[0].position.get(), 4);
    assert_eq!(more[0].sequence.get(), 4);
}

/// Two writers who both loaded at the same version: one wins, one is told to
/// retry. Enforced by the database, not by a read-then-write race.
#[tokio::test]
async fn a_concurrent_writer_is_refused_rather_than_overwriting() {
    let db = tenant_db().await;
    let mut first = db.pool().acquire().await.expect("connection");
    let mut second = db.pool().acquire().await.expect("connection");

    append(
        &mut first,
        &stream("1000"),
        Sequence::ZERO,
        &[event("a")],
        &Metadata::default(),
    )
    .await
    .expect("first writer wins");

    let refused = append(
        &mut second,
        &stream("1000"),
        Sequence::ZERO,
        &[event("b")],
        &Metadata::default(),
    )
    .await;

    assert!(
        matches!(refused, Err(AppendError::Conflict { .. })),
        "expected a conflict, got {refused:?}"
    );

    // The winner's event is intact — a conflict must not corrupt the stream.
    let stored = read_stream(&mut first, &stream("1000"))
        .await
        .expect("reads");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].event_name.as_str(), "a");
}

/// A rolled-back append must return its positions rather than burning them.
///
/// This is the difference between the counter row and a sequence, and it is why
/// the contiguity check can be an integrity assertion rather than a warning.
#[tokio::test]
async fn a_rolled_back_append_does_not_burn_a_position() {
    let db = tenant_db().await;

    let mut tx = db.pool().begin().await.expect("transaction");
    append(
        &mut tx,
        &stream("1000"),
        Sequence::ZERO,
        &[event("doomed"), event("also-doomed")],
        &Metadata::default(),
    )
    .await
    .expect("appends");
    tx.rollback().await.expect("rolls back");

    let mut conn = db.pool().acquire().await.expect("connection");
    let survivor = append(
        &mut conn,
        &stream("1000"),
        Sequence::ZERO,
        &[event("kept")],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    assert_eq!(
        survivor[0].position.get(),
        1,
        "a sequence would have handed out 3 here; the counter row must not"
    );
    assert!(
        integrity(&mut conn).await.expect("checks").is_contiguous(),
        "the log must remain contiguous after a rollback"
    );
}

/// **The proof of L1.**
///
/// A tailer runs concurrently with many writers, repeatedly reading
/// `position > checkpoint`. If positions could be observed out of commit order —
/// which is what a plain `GENERATED ALWAYS AS IDENTITY` allows — the tailer
/// would advance past an uncommitted position and that event would never be
/// delivered. Here it must see every position exactly once, in order, with no
/// holes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tailer_never_skips_an_event() {
    const WRITERS: i64 = 16;
    const PER_WRITER: i64 = 25;
    const EXPECTED: i64 = WRITERS * PER_WRITER;

    let db = Arc::new(tenant_db().await);
    let writing = Arc::new(AtomicBool::new(true));

    // Writers: each owns its own stream, so they contend on the position
    // counter rather than on the per-stream unique constraint.
    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let db = Arc::clone(&db);
        writers.push(tokio::spawn(async move {
            let target = stream(&format!("acct-{w}"));
            for i in 0..PER_WRITER {
                let mut tx = db.pool().begin().await.expect("transaction");
                append(
                    &mut tx,
                    &target,
                    Sequence::new(i).unwrap(),
                    &[event("tick")],
                    &Metadata::default(),
                )
                .await
                .expect("appends");
                // Work *after* the append, holding the counter lock — the
                // window in which a naive implementation loses events.
                tokio::task::yield_now().await;
                tx.commit().await.expect("commits");
            }
        }));
    }

    // Tailer: exactly what a projection runner does.
    let tailer = {
        let db = Arc::clone(&db);
        let writing = Arc::clone(&writing);
        tokio::spawn(async move {
            let mut checkpoint = LogPosition::ZERO;
            let mut seen: Vec<i64> = Vec::new();
            loop {
                let mut conn = db.pool().acquire().await.expect("connection");
                let batch = read_since(&mut conn, checkpoint, 100).await.expect("reads");
                drop(conn);

                if batch.is_empty() {
                    if !writing.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    continue;
                }
                for envelope in &batch {
                    seen.push(envelope.position.get());
                }
                checkpoint = batch.last().expect("non-empty").position;
            }
            seen
        })
    };

    for writer in writers {
        writer.await.expect("writer finished");
    }
    writing.store(false, Ordering::Relaxed);
    let seen = tailer.await.expect("tailer finished");

    // Every position, exactly once, in order. Any skip is a lost event.
    let expected: Vec<i64> = (1..=EXPECTED).collect();
    assert_eq!(
        seen.len(),
        expected.len(),
        "tailer saw {} positions, expected {EXPECTED}",
        seen.len()
    );
    assert_eq!(
        seen, expected,
        "the tailer must observe an unbroken, ordered prefix of the log"
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    let report = integrity(&mut conn).await.expect("checks");
    assert!(
        report.is_contiguous(),
        "log is not contiguous after concurrent appends: {report:?}"
    );
    assert_eq!(report.event_count, EXPECTED);
}

/// Concurrent appends to the *same* stream: the unique constraint means exactly
/// one writer per version wins, and the log stays contiguous regardless.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contention_on_one_stream_leaves_the_log_contiguous() {
    let db = Arc::new(tenant_db().await);
    let target = stream("hot");

    let mut tasks = Vec::new();
    for _ in 0..12 {
        let db = Arc::clone(&db);
        let target = target.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = db.pool().acquire().await.expect("connection");
            append(
                &mut conn,
                &target,
                Sequence::ZERO,
                &[event("race")],
                &Metadata::default(),
            )
            .await
        }));
    }

    let mut winners = 0;
    let mut conflicts = 0;
    for task in tasks {
        match task.await.expect("joined") {
            Ok(_) => winners += 1,
            Err(AppendError::Conflict { .. }) => conflicts += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    assert_eq!(winners, 1, "exactly one writer may take version 1");
    assert_eq!(conflicts, 11);

    let mut conn = db.pool().acquire().await.expect("connection");
    let report = integrity(&mut conn).await.expect("checks");
    assert!(
        report.is_contiguous(),
        "eleven failed appends must not leave holes: {report:?}"
    );
    assert_eq!(report.event_count, 1);
}

#[tokio::test]
async fn the_log_refuses_to_be_edited() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    append(
        &mut conn,
        &stream("1000"),
        Sequence::ZERO,
        &[event("a")],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    // A log that can be rewritten is not a log. Enforced by the database.
    assert!(
        sqlx::query("UPDATE event SET payload = '{}'")
            .execute(&mut *conn)
            .await
            .is_err(),
        "events must not be updatable"
    );
    assert!(
        sqlx::query("DELETE FROM event")
            .execute(&mut *conn)
            .await
            .is_err(),
        "events must not be deletable"
    );
}

#[tokio::test]
async fn metadata_round_trips() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let metadata = Metadata {
        actor: Some("019fd3b1-0000-7000-8000-000000000001".into()),
        on_behalf_of: None,
        correlation_id: Some("req-42".into()),
        config_version: Some(7),
        extra: serde_json::Map::new(),
    };

    append(
        &mut conn,
        &stream("1000"),
        Sequence::ZERO,
        &[event("a")],
        &metadata,
    )
    .await
    .expect("appends");

    let stored = read_stream(&mut conn, &stream("1000"))
        .await
        .expect("reads");
    assert_eq!(stored[0].metadata, metadata);
    // config_version is what makes L5 checkable a year later.
    assert_eq!(stored[0].metadata.config_version, Some(7));
}

#[tokio::test]
async fn an_empty_batch_is_refused() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    let result = append(
        &mut conn,
        &stream("1000"),
        Sequence::ZERO,
        &[],
        &Metadata::default(),
    )
    .await;
    assert!(matches!(result, Err(AppendError::Empty)));
}

#[tokio::test]
async fn reads_are_paged_in_position_order() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");
    for i in 0..10 {
        append(
            &mut conn,
            &stream("1000"),
            Sequence::new(i).unwrap(),
            &[event("tick")],
            &Metadata::default(),
        )
        .await
        .expect("appends");
    }

    let first = read_since(&mut conn, LogPosition::ZERO, 4)
        .await
        .expect("reads");
    assert_eq!(first.len(), 4);
    assert_eq!(first[0].position.get(), 1);
    assert_eq!(first[3].position.get(), 4);

    let next = read_since(&mut conn, first[3].position, 4)
        .await
        .expect("reads");
    assert_eq!(next[0].position.get(), 5);

    // Past the end is empty, not an error — that is the tailer's idle case.
    let end = read_since(&mut conn, LogPosition::new(100).unwrap(), 4)
        .await
        .expect("reads");
    assert!(end.is_empty());
}

/// Demonstrates the bug the counter row exists to prevent, so that
/// [`a_tailer_never_skips_an_event`] cannot pass vacuously.
///
/// Here the same tailer logic runs against a table using
/// `GENERATED ALWAYS AS IDENTITY` — the obvious implementation — and **loses an
/// event**. Positions are handed out at INSERT time, so a later position can
/// commit first, and a tailer that advances to it steps over one that had not
/// committed yet.
///
/// If this test ever stops failing to find the skip, either Postgres changed
/// how sequences interact with visibility, or the harness stopped reproducing
/// the interleaving — and in the second case the real test above is no longer
/// proving anything either.
#[tokio::test]
async fn the_naive_implementation_really_does_lose_events() {
    static NAIVE: Schema = Schema::sql(
        "naive-log",
        &[
            "CREATE TABLE naive (position BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, tag TEXT NOT NULL)",
        ],
    );

    let db = Template::get(&NAIVE)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones");

    // Two writers, interleaved exactly as they can be under load.
    let mut slow = db.pool().begin().await.expect("transaction");
    let mut fast = db.pool().begin().await.expect("transaction");

    let slow_position: i64 =
        sqlx::query_scalar("INSERT INTO naive (tag) VALUES ('slow') RETURNING position")
            .fetch_one(&mut *slow)
            .await
            .expect("insert");
    let fast_position: i64 =
        sqlx::query_scalar("INSERT INTO naive (tag) VALUES ('fast') RETURNING position")
            .fetch_one(&mut *fast)
            .await
            .expect("insert");

    assert_eq!(slow_position, 1);
    assert_eq!(
        fast_position, 2,
        "a sequence hands out positions at insert time, so the second writer \
         gets a higher position while the first is still uncommitted"
    );

    // The later position commits first.
    fast.commit().await.expect("commits");

    // A tailer reads, and sees only position 2.
    let mut conn = db.pool().acquire().await.expect("connection");
    let visible: Vec<i64> = sqlx::query_scalar("SELECT position FROM naive ORDER BY position")
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(
        visible,
        vec![2],
        "position 1 is still uncommitted and therefore invisible"
    );
    let checkpoint = *visible.last().expect("non-empty"); // the tailer advances to 2

    // Now the first writer commits.
    slow.commit().await.expect("commits");

    // And its event is gone forever: the tailer is past it.
    let after: Vec<i64> = sqlx::query_scalar("SELECT position FROM naive WHERE position > $1")
        .bind(checkpoint)
        .fetch_all(&mut *conn)
        .await
        .expect("reads");
    assert!(
        after.is_empty(),
        "the skipped event is unreachable from the checkpoint — this is the \
         silent data loss the counter row prevents"
    );

    // For contrast: everything is in the table. The event was written, and a
    // full scan finds it. It is only *unreachable to a tailer*, which is
    // exactly why the failure is silent.
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM naive")
        .fetch_one(&mut *conn)
        .await
        .expect("reads");
    assert_eq!(total, 2);
}
