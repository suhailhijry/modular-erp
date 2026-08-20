//! What a crash leaves behind in the log and the outbox.
//!
//! The connection is severed with `pg_terminate_backend`, so the rollback is
//! Postgres's rather than the code's. That matters: the code's own error paths
//! are the ones it was written to have, and they are not where the interesting
//! failures live.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use erp_eventlog::{
    Aggregate, Decision, DeliveryError, Dispatcher, DomainEvent, Effect, EffectHandler, Metadata,
    PendingEffect, RetryPolicy, Upcasters, append_events, enqueue, execute, integrity,
};
use erp_testkit::{Schema, Template, TestDb, kill_connection};
use erp_types::{AggregateId, DomainName, EffectKind, EventName, SchemaVersion, Sequence};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Charged {
    amount: i64,
}

impl DomainEvent for Charged {
    fn event_name(&self) -> EventName {
        EventName::new("account.charged").unwrap()
    }
    fn schema_version(&self) -> SchemaVersion {
        SchemaVersion::new(1).unwrap()
    }
}

#[derive(Debug, Default)]
struct Account {
    balance: i64,
}

impl Aggregate for Account {
    type Event = Charged;

    fn domain() -> DomainName {
        DomainName::new("account").unwrap()
    }

    fn apply(&mut self, event: &Self::Event) {
        self.balance += event.amount;
    }
}

#[derive(Debug, thiserror::Error)]
enum AccountError {
    #[error("refused")]
    #[expect(dead_code, reason = "the type needs a variant; no test refuses")]
    Refused,
}

fn upcasters() -> Upcasters {
    Upcasters::new().declare(
        &EventName::new("account.charged").unwrap(),
        SchemaVersion::new(1).unwrap(),
    )
}

fn kind() -> EffectKind {
    EffectKind::new("email.send").unwrap()
}

async fn tenant_db() -> TestDb {
    Template::get(&TENANT)
        .await
        .expect("template builds")
        .fresh()
        .await
        .expect("clones")
}

// ---------------------------------------------------------------------------
// The command side
// ---------------------------------------------------------------------------

/// A crash between the append and the enqueue leaves neither.
///
/// The two writes are in one transaction precisely so this ordering cannot
/// produce a half-state — an event with no promise, or a promise with no event.
/// The kill lands between them, which is the window that would exist if they
/// were separate commits.
#[tokio::test]
async fn a_crash_between_the_append_and_the_promise_leaves_neither() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");

    let envelopes = append_events::<Account>(
        &mut tx,
        &AggregateId::new("a1").unwrap(),
        Sequence::ZERO,
        &[Charged { amount: 100 }],
        &Metadata::default(),
    )
    .await
    .expect("appends");
    let position = envelopes.first().map(|e| e.position);

    // The window a two-commit design would have.
    kill_connection(&mut tx).await.expect("kills");

    let after = enqueue(
        &mut tx,
        position,
        &[Effect::new(kind(), serde_json::json!({}))],
    )
    .await;
    assert!(after.is_err(), "the connection is gone");
    drop(tx);

    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(db.pool())
        .await
        .expect("counts");
    let effects: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(db.pool())
        .await
        .expect("counts");

    assert_eq!(events, 0, "the append rolled back with the transaction");
    assert_eq!(effects, 0);
}

/// **L1 under a crash.** A rolled-back append returns its position.
///
/// The counter row is ordinary transactional data, so a crash gives the number
/// back. A sequence would have burned it, and the contiguity check — which is a
/// per-tenant health assertion — would have to be downgraded to a warning.
#[tokio::test]
async fn a_crash_during_an_append_burns_no_position() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");

    append_events::<Account>(
        &mut tx,
        &AggregateId::new("a1").unwrap(),
        Sequence::ZERO,
        &[Charged { amount: 1 }, Charged { amount: 2 }],
        &Metadata::default(),
    )
    .await
    .expect("appends");
    kill_connection(&mut tx).await.expect("kills");
    drop(tx);

    // The next command succeeds and starts at one.
    let committed = execute::<Account, _, AccountError>(
        db.pool(),
        &AggregateId::new("a1").unwrap(),
        &upcasters(),
        &Metadata::default(),
        |_| Ok(Decision::one(Charged { amount: 5 })),
    )
    .await
    .expect("executes");

    assert_eq!(
        committed.at.map(erp_types::LogPosition::get),
        Some(1),
        "positions 1 and 2 were returned by the rollback, not burned"
    );

    let mut conn = db.pool().acquire().await.expect("connection");
    let report = integrity(&mut conn).await.expect("checks");
    assert!(
        report.is_contiguous(),
        "the contiguity check is only an integrity assertion if this holds: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// The delivery side
// ---------------------------------------------------------------------------

struct Recorder {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EffectHandler for Recorder {
    fn kind(&self) -> EffectKind {
        kind()
    }

    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(effect.idempotency_key.clone());
        Ok(())
    }
}

/// **At-least-once, made concrete.**
///
/// The delivery succeeds and the record of it is lost to a crash. The effect is
/// therefore delivered again — which is not a bug to be fixed here but the
/// unavoidable consequence of the delivery and its record being separate
/// commits. What makes it survivable is that both deliveries carry the *same*
/// idempotency key, so a handler that passes it downstream makes the second one
/// a no-op on the far side.
///
/// A test asserting "delivered exactly once" would be asserting something this
/// design does not provide, and would fail the first time a worker was killed in
/// production rather than in CI.
#[tokio::test]
async fn a_delivery_whose_record_is_lost_happens_again_with_the_same_key() {
    let db = tenant_db().await;

    execute::<Account, _, AccountError>(
        db.pool(),
        &AggregateId::new("a1").unwrap(),
        &upcasters(),
        &Metadata::default(),
        |_| {
            Ok(Decision::one(Charged { amount: 1 })
                .with_effect(Effect::new(kind(), serde_json::json!({}))))
        },
    )
    .await
    .expect("executes");

    let handler = Arc::new(Recorder {
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let dispatcher = Dispatcher::new(RetryPolicy {
        max_attempts: 8,
        base_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        lease: Duration::from_secs(30),
    })
    .register(handler.clone());

    // Claim, deliver, and then lose the connection before recording it.
    let mut conn = db.pool().acquire().await.expect("connection");
    let claimed = dispatcher.claim(&mut conn, 10).await.expect("claims");
    assert_eq!(claimed.len(), 1);

    let settlement = dispatcher.deliver(&claimed[0]).await;
    assert_eq!(settlement, erp_eventlog::Settlement::Delivered);

    kill_connection(&mut conn).await.expect("kills");
    assert!(
        dispatcher
            .settle(&mut conn, &claimed[0], &settlement)
            .await
            .is_err(),
        "the record of the delivery is lost"
    );
    drop(conn);

    // The claim was its own commit, so the attempt counted even though the
    // settle did not.
    let (attempts, delivered): (i32, Option<erp_types::Timestamp>) =
        sqlx::query_as("SELECT attempts, delivered_at FROM outbox")
            .fetch_one(db.pool())
            .await
            .expect("reads");
    assert_eq!(attempts, 1);
    assert_eq!(
        delivered, None,
        "as far as the outbox knows, nothing happened"
    );

    // Once the lease lapses the effect comes back round.
    sqlx::query("UPDATE outbox SET leased_until = now() - INTERVAL '1 second'")
        .execute(db.pool())
        .await
        .expect("expires the lease");

    let pass = dispatcher
        .dispatch_once(db.pool(), 10)
        .await
        .expect("dispatches");
    assert_eq!(pass.delivered, 1);

    let keys = handler.seen.lock().unwrap().clone();
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2, "delivered twice");
    assert_eq!(
        keys[0], keys[1],
        "and both deliveries carry the same idempotency key — which is the \
         only thing that makes at-least-once survivable"
    );
}

/// A crash while claiming does not lose the effect.
#[tokio::test]
async fn a_crash_during_a_claim_leaves_the_effect_owed() {
    let db = tenant_db().await;

    execute::<Account, _, AccountError>(
        db.pool(),
        &AggregateId::new("a1").unwrap(),
        &upcasters(),
        &Metadata::default(),
        |_| {
            Ok(Decision::one(Charged { amount: 1 })
                .with_effect(Effect::new(kind(), serde_json::json!({}))))
        },
    )
    .await
    .expect("executes");

    let handler = Arc::new(Recorder {
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let dispatcher = Dispatcher::new(RetryPolicy::default()).register(handler.clone());

    // Inside a transaction, so the claim has not committed when the connection
    // dies — the shape of a worker killed the instant it took the work.
    let mut tx = db.pool().begin().await.expect("transaction");
    let claimed = dispatcher.claim(&mut tx, 10).await.expect("claims");
    assert_eq!(claimed.len(), 1);
    kill_connection(&mut tx).await.expect("kills");
    drop(tx);

    let (attempts, leased): (i32, Option<erp_types::Timestamp>) =
        sqlx::query_as("SELECT attempts, leased_until FROM outbox")
            .fetch_one(db.pool())
            .await
            .expect("reads");
    assert_eq!(
        attempts, 0,
        "the claim rolled back, so no attempt was spent"
    );
    assert_eq!(leased, None, "and no lease was left behind");

    // Immediately claimable again, with no wait for an expiry.
    let pass = dispatcher
        .dispatch_once(db.pool(), 10)
        .await
        .expect("dispatches");
    assert_eq!(pass.delivered, 1);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}
