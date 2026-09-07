//! **How fast one tenant can write**, measured rather than feared.
//!
//! L1 gives every tenant gapless, commit-ordered positions by taking one row
//! lock per append and holding it to commit, so a tenant's writes serialise at
//! roughly one per commit latency. That is the design (see `ARCHITECTURE.md`,
//! L1) and this is the number it comes to on the machine running the tests:
//! the test prints it, and asserts only a floor low enough never to flake, so
//! a change that made appends wait on something new would show up here.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use erp_eventlog::{Metadata, NewEvent, append, integrity};
use erp_testkit::{Schema, Template};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Sequence, StreamId};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

/// A floor, not a target: well under anything a laptop does, so the assertion
/// is about the mechanism not having gained a new wait, never about hardware.
const FLOOR_PER_SECOND: f64 = 20.0;

#[tokio::test]
async fn one_tenant_appends_from_many_writers_stay_gapless_and_the_rate_is_measured() {
    let db = Arc::new(
        Template::get(&TENANT)
            .await
            .expect("tenant template builds")
            .fresh()
            .await
            .expect("tenant database clones"),
    );
    let writers = 8;
    let for_how_long = Duration::from_secs(2);
    let written = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    let mut tasks = Vec::new();
    for n in 0..writers {
        let db = Arc::clone(&db);
        let written = Arc::clone(&written);
        tasks.push(tokio::spawn(async move {
            let stream = StreamId::new(
                DomainName::new("ledger_account").expect("valid"),
                AggregateId::new(format!("writer-{n}")).expect("valid"),
            );
            let mut version = Sequence::ZERO;
            while started.elapsed() < for_how_long {
                let mut tx = db.pool().begin().await.expect("begins");
                let appended = append(
                    &mut tx,
                    &stream,
                    version,
                    &[NewEvent::new(
                        EventName::new("ledger.account.opened").expect("valid"),
                        SchemaVersion::new(1).expect("valid"),
                        serde_json::json!({ "writer": n }),
                    )],
                    &Metadata::default(),
                )
                .await
                .expect("appends");
                tx.commit().await.expect("commits");
                version = appended.last().expect("one event").sequence;
                written.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for task in tasks {
        task.await.expect("a writer finished");
    }
    let elapsed = started.elapsed();
    let total = written.load(Ordering::Relaxed);
    #[allow(clippy::cast_precision_loss)]
    let per_second = total as f64 / elapsed.as_secs_f64();
    eprintln!(
        "append throughput: {total} events from {writers} writers in {elapsed:.2?} = {per_second:.0}/s on one tenant"
    );

    // What the throughput was paid for: every position handed out was used, in
    // order, with nothing skipped.
    let mut conn = db.pool().acquire().await.expect("connection");
    let health = integrity(&mut conn).await.expect("reads");
    let total = i64::try_from(total).expect("fits");
    assert_eq!(health.event_count, total);
    assert_eq!(health.highest_position, total);
    assert_eq!(health.next_position, total + 1);

    assert!(
        per_second > FLOOR_PER_SECOND,
        "{per_second:.0} appends/s is below the floor; appends have started waiting on something new"
    );
}
