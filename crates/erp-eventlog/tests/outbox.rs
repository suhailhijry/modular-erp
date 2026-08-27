//! The outbox: effects as values (D9).
//!
//! Two properties carry the whole design, and each has a test that would fail if
//! the property were only asserted in a comment:
//!
//! - **Atomicity** — [`a_rolled_back_command_promises_nothing`]. If this breaks,
//!   customers get emailed about transactions that did not happen.
//! - **A missing handler is not a failure** —
//!   [`effects_with_no_registered_handler_are_left_alone`]. If this breaks, an
//!   ordinary staggered deploy dead-letters a tenant's effects.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_eventlog::{
    Aggregate, Decision, DeliveryError, Dispatcher, DomainEvent, Effect, EffectHandler,
    EnqueueError, ExecuteError, Metadata, PendingEffect, RetryPolicy, Upcasters, append_events,
    enqueue, execute, outbox_health,
};
use erp_testkit::{Schema, Template, TestDb};
use erp_types::{AggregateId, DomainName, EffectKind, EventName, SchemaVersion, Sequence};
use serde::{Deserialize, Serialize};

static TENANT: Schema = Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

// ---------------------------------------------------------------------------
// A minimal aggregate to hang commands off
// ---------------------------------------------------------------------------

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

#[derive(Debug, Default, PartialEq, Eq)]
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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum AccountError {
    #[error("refused")]
    Refused,
}

fn upcasters() -> Upcasters {
    Upcasters::new().declare(
        &EventName::new("account.charged").unwrap(),
        SchemaVersion::new(1).unwrap(),
    )
}

fn kind(name: &str) -> EffectKind {
    EffectKind::new(name).unwrap()
}

fn id(name: &str) -> AggregateId {
    AggregateId::new(name).unwrap()
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
// A handler that records what it was asked to do
// ---------------------------------------------------------------------------

/// Delivery outcomes a test handler can be told to produce, in order.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Succeed,
    Retryable,
    Permanent,
}

struct Recorder {
    kind: EffectKind,
    /// Every idempotency key this handler was handed, in order, including
    /// repeats — which is how at-least-once delivery is observed.
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    /// Outcome per call. The last entry repeats once exhausted.
    script: Vec<Outcome>,
    calls: Arc<AtomicUsize>,
    /// Held open for this long inside `deliver`, so a test can have two
    /// dispatchers genuinely in flight at once.
    latency: Duration,
}

impl Recorder {
    fn new(kind: EffectKind, script: Vec<Outcome>) -> Self {
        Self {
            kind,
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
            script,
            calls: Arc::new(AtomicUsize::new(0)),
            latency: Duration::ZERO,
        }
    }

    fn always(kind: EffectKind, outcome: Outcome) -> Self {
        Self::new(kind, vec![outcome])
    }

    fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    fn keys(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl EffectHandler for Recorder {
    fn kind(&self) -> EffectKind {
        self.kind.clone()
    }

    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(effect.idempotency_key.clone());

        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }

        match self.script.get(call).or_else(|| self.script.last()) {
            Some(Outcome::Succeed) | None => Ok(()),
            Some(Outcome::Retryable) => Err(DeliveryError::Retryable("upstream 503".into())),
            Some(Outcome::Permanent) => Err(DeliveryError::Permanent("upstream 400".into())),
        }
    }
}

/// A policy with millisecond timings, so retry tests finish in milliseconds.
///
/// The backoff is 250ms rather than something smaller because one test asserts
/// an effect is *not yet* due immediately after failing. Five milliseconds made
/// that assertion a race against the machine, and it lost about one run in six
/// under a full parallel suite — a flake that says the code is broken when it is
/// the test that is.
fn fast_policy(max_attempts: i32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_backoff: Duration::from_millis(250),
        max_backoff: Duration::from_millis(500),
        lease: Duration::from_secs(30),
    }
}

async fn pending_count(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM outbox WHERE delivered_at IS NULL AND dead_at IS NULL")
        .fetch_one(db.pool())
        .await
        .expect("counts")
}

/// Whether the one pending effect is due for another attempt.
async fn is_due(db: &TestDb) -> bool {
    sqlx::query_scalar(
        "SELECT next_attempt_at <= now() FROM outbox
          WHERE delivered_at IS NULL AND dead_at IS NULL",
    )
    .fetch_one(db.pool())
    .await
    .expect("reads")
}

/// Waits for the backoff to elapse.
///
/// Polling the condition rather than sleeping a guessed duration: a sleep long
/// enough to be reliable on a loaded machine is also long enough to slow every
/// run, and one short enough to be quick is the flake this replaced.
async fn wait_until_due(db: &TestDb) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !is_due(db).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the effect never became due"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Writing effects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_command_records_its_events_and_its_effects_together() {
    let db = tenant_db().await;

    let committed = execute::<Account, _, AccountError>(
        db.pool(),
        &id("a1"),
        &upcasters(),
        &Metadata::default(),
        |_| {
            Ok(
                Decision::one(Charged { amount: 500 }).with_effect(Effect::new(
                    kind("email.send"),
                    serde_json::json!({ "to": "customer@example.com" }),
                )),
            )
        },
    )
    .await
    .expect("executes");

    assert_eq!(committed.events.len(), 1);
    assert_eq!(committed.effects_enqueued, 1);

    let (kind_, caused_by): (String, Option<i64>) =
        sqlx::query_as("SELECT kind, caused_by FROM outbox")
            .fetch_one(db.pool())
            .await
            .expect("reads");
    assert_eq!(kind_, "email.send");
    assert_eq!(
        caused_by,
        committed.at.map(erp_types::LogPosition::get),
        "an effect must name the command that promised it"
    );
}

/// **The property the whole design rests on.**
///
/// If a promise can outlive the transaction that made it, the system emails
/// customers about transactions that never happened. Asserted directly: append
/// and enqueue in one transaction, roll it back, and check that *neither*
/// survived.
#[tokio::test]
async fn a_rolled_back_command_promises_nothing() {
    let db = tenant_db().await;
    let mut tx = db.pool().begin().await.expect("transaction");

    let envelopes = append_events::<Account>(
        &mut tx,
        &id("a1"),
        Sequence::ZERO,
        &[Charged { amount: 100 }],
        &Metadata::default(),
    )
    .await
    .expect("appends");

    let enqueued = enqueue(
        &mut tx,
        envelopes.first().map(|e| e.position),
        &[Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await
    .expect("enqueues");
    assert_eq!(
        enqueued, 1,
        "the promise was written inside the transaction"
    );

    tx.rollback().await.expect("rolls back");

    let effects: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(db.pool())
        .await
        .expect("counts");
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM event")
        .fetch_one(db.pool())
        .await
        .expect("counts");

    assert_eq!(effects, 0, "the promise must roll back with its cause");
    assert_eq!(events, 0);
}

#[tokio::test]
async fn a_rejected_command_promises_nothing() {
    let db = tenant_db().await;

    let result = execute::<Account, _, AccountError>(
        db.pool(),
        &id("a1"),
        &upcasters(),
        &Metadata::default(),
        |_| Err(AccountError::Refused),
    )
    .await;

    assert!(matches!(result, Err(ExecuteError::Rejected(_))));
    assert_eq!(pending_count(&db).await, 0);
}

#[tokio::test]
async fn effects_from_one_command_get_distinct_derived_keys() {
    let db = tenant_db().await;

    let committed = execute::<Account, _, AccountError>(
        db.pool(),
        &id("a1"),
        &upcasters(),
        &Metadata::default(),
        |_| {
            Ok(Decision::one(Charged { amount: 1 })
                .with_effect(Effect::new(kind("email.send"), serde_json::json!({})))
                .with_effect(Effect::new(kind("email.send"), serde_json::json!({})))
                .with_effect(Effect::new(kind("webhook.post"), serde_json::json!({}))))
        },
    )
    .await
    .expect("executes");

    assert_eq!(committed.effects_enqueued, 3);

    let keys: Vec<String> =
        sqlx::query_scalar("SELECT idempotency_key FROM outbox ORDER BY idempotency_key")
            .fetch_all(db.pool())
            .await
            .expect("reads");
    assert_eq!(keys, vec!["1:0", "1:1", "1:2"], "position, then index");
}

/// A pinned key deduplicates across command executions.
#[tokio::test]
async fn a_pinned_key_is_promised_only_once() {
    let db = tenant_db().await;

    let charge_twice = || async {
        execute::<Account, _, AccountError>(
            db.pool(),
            &id("a1"),
            &upcasters(),
            &Metadata::default(),
            |_| {
                Ok(
                    Decision::one(Charged { amount: 1 }).with_effect(Effect::with_key(
                        kind("email.send"),
                        "welcome:a1",
                        serde_json::json!({}),
                    )),
                )
            },
        )
        .await
        .expect("executes")
    };

    let first = charge_twice().await;
    let second = charge_twice().await;

    assert_eq!(first.effects_enqueued, 1);
    assert_eq!(
        second.effects_enqueued, 0,
        "the second promise is the same promise"
    );

    // Both commands still recorded their events — deduplicating the effect must
    // not deduplicate the facts.
    assert_eq!(second.version.get(), 2);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox")
        .fetch_one(db.pool())
        .await
        .expect("counts");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn duplicate_pinned_keys_within_one_command_collapse() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let effect = || Effect::with_key(kind("email.send"), "same", serde_json::json!({}));
    let enqueued = enqueue(&mut conn, None, &[effect(), effect()])
        .await
        .expect("enqueues");

    assert_eq!(
        enqueued, 1,
        "a batch that repeats a key must insert one row, not violate a constraint"
    );
}

#[tokio::test]
async fn an_effect_with_no_key_and_no_cause_is_refused() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let result = enqueue(
        &mut conn,
        None,
        &[Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    // Refusing beats inventing a key: an invented one makes a retry
    // indistinguishable from a new promise.
    assert!(
        matches!(result, Err(EnqueueError::NoKey { .. })),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------------
// Delivering them
// ---------------------------------------------------------------------------

async fn promise(db: &TestDb, effects: Vec<Effect>) {
    let mut decision = Decision::one(Charged { amount: 1 });
    decision = decision.with_effects(effects);
    let decision = std::sync::Mutex::new(Some(decision));

    execute::<Account, _, AccountError>(
        db.pool(),
        &id("a1"),
        &upcasters(),
        &Metadata::default(),
        |_| {
            Ok(decision
                .lock()
                .unwrap()
                .clone()
                .expect("the decision is remade on retry"))
        },
    )
    .await
    .expect("executes");
}

#[tokio::test]
async fn a_delivered_effect_is_never_delivered_again() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let handler = Arc::new(Recorder::always(kind("email.send"), Outcome::Succeed));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(handler.clone());

    let first = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(first.delivered, 1);

    let second = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(second.claimed, 0, "a delivered effect is not claimable");
    assert_eq!(handler.call_count(), 1);
}

#[tokio::test]
async fn a_retryable_failure_is_tried_again_and_keeps_its_key() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let handler = Arc::new(Recorder::new(
        kind("email.send"),
        vec![Outcome::Retryable, Outcome::Retryable, Outcome::Succeed],
    ));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(handler.clone());

    let first = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(first.retrying, 1);
    assert_eq!(first.delivered, 0);

    // The backoff was recorded. Asserted against the row rather than against a
    // second dispatch, so it holds however slow the machine is.
    assert!(
        !is_due(&db).await,
        "a failed effect must not be due at once"
    );

    // And the claim query honours it. This also pins down what
    // `dispatch_until_idle` means — it drains what is *due*, not what will
    // eventually be due, so it returns while this effect is still owed.
    let immediate = dispatcher
        .dispatch_until_idle(db.pool(), 10)
        .await
        .expect("runs");
    assert_eq!(
        immediate.claimed, 0,
        "the backoff must actually hold it back"
    );

    wait_until_due(&db).await;
    let second = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(second.retrying, 1, "second attempt, failing again");

    wait_until_due(&db).await;
    let third = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(third.delivered, 1);

    let keys = handler.keys();
    assert_eq!(keys.len(), 3, "three deliveries were attempted");
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "the idempotency key must be stable across retries, or the downstream \
         system cannot deduplicate them: {keys:?}"
    );
}

#[tokio::test]
async fn attempts_run_out_and_the_effect_becomes_a_dead_letter() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let handler = Arc::new(Recorder::always(kind("email.send"), Outcome::Retryable));
    let dispatcher = Dispatcher::new(fast_policy(2)).register(handler.clone());

    dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    wait_until_due(&db).await;
    let last = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");

    assert_eq!(last.dead, 1);
    assert_eq!(handler.call_count(), 2, "exactly max_attempts deliveries");

    let mut conn = db.pool().acquire().await.expect("connection");
    let health = outbox_health(&mut conn).await.expect("reads");
    assert_eq!(health.dead, 1);
    assert_eq!(health.pending, 0);
    assert!(
        !health.is_healthy(60),
        "a dead letter must make the tenant unhealthy — it is a promise nobody kept"
    );

    let error: Option<String> = sqlx::query_scalar("SELECT last_error FROM outbox")
        .fetch_one(db.pool())
        .await
        .expect("reads");
    assert!(
        error.is_some_and(|e| e.contains("503")),
        "the dead letter must say why"
    );
}

#[tokio::test]
async fn a_permanent_failure_does_not_waste_seven_more_attempts() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let handler = Arc::new(Recorder::always(kind("email.send"), Outcome::Permanent));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(handler.clone());

    let pass = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");

    assert_eq!(pass.dead, 1);
    assert_eq!(
        handler.call_count(),
        1,
        "a 400 will not become a 200; retrying it only delays the alarm"
    );
}

/// **The test that makes staggered deploys safe.**
///
/// A worker without some module's handler must leave those effects alone. The
/// tempting alternative — claim, fail, back off — burns attempts and
/// dead-letters a tenant's effects during an ordinary rollout, which is an
/// outage caused entirely by the deploy.
#[tokio::test]
async fn effects_with_no_registered_handler_are_left_alone() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![
            Effect::new(kind("email.send"), serde_json::json!({})),
            Effect::new(kind("ledger.export"), serde_json::json!({})),
        ],
    )
    .await;

    // This worker knows about email and nothing else.
    let email = Arc::new(Recorder::always(kind("email.send"), Outcome::Succeed));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(email.clone());

    let pass = dispatcher
        .dispatch_until_idle(db.pool(), 10)
        .await
        .expect("runs");
    assert_eq!(pass.delivered, 1);
    assert_eq!(pass.dead, 0);

    let (attempts, kind_): (i32, String) = sqlx::query_as(
        "SELECT attempts, kind FROM outbox WHERE delivered_at IS NULL AND dead_at IS NULL",
    )
    .fetch_one(db.pool())
    .await
    .expect("reads");
    assert_eq!(kind_, "ledger.export");
    assert_eq!(
        attempts, 0,
        "an unhandled effect must not burn an attempt — it was never tried"
    );

    // It is still owed, and the backlog is what tells an operator so.
    let mut conn = db.pool().acquire().await.expect("connection");
    let health = outbox_health(&mut conn).await.expect("reads");
    assert_eq!(health.pending, 1);
    assert_eq!(health.dead, 0);
    assert!(health.backlog_age_seconds.is_some());

    // And a worker that does know it picks it up with no ceremony.
    let exporter = Arc::new(Recorder::always(kind("ledger.export"), Outcome::Succeed));
    let complete = Dispatcher::new(fast_policy(8)).register(exporter.clone());
    let pass = complete
        .dispatch_until_idle(db.pool(), 10)
        .await
        .expect("runs");
    assert_eq!(pass.delivered, 1);
    assert_eq!(pending_count(&db).await, 0);
}

/// `SKIP LOCKED`: a row another dispatcher is working on is passed over, not
/// waited for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locked_effect_is_skipped_rather_than_blocked_on() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![
            Effect::new(kind("email.send"), serde_json::json!({ "n": 1 })),
            Effect::new(kind("email.send"), serde_json::json!({ "n": 2 })),
            Effect::new(kind("email.send"), serde_json::json!({ "n": 3 })),
        ],
    )
    .await;

    // Stand in for another dispatcher mid-claim.
    //
    // The id is chosen first and locked by primary key. `... ORDER BY id LIMIT 1
    // OFFSET 1 FOR UPDATE` looks equivalent and is not: rows discarded by the
    // OFFSET still pass through the LockRows node, so it locks the first row as
    // well and the test silently measures the wrong thing.
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM outbox ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("reads");
    let held = ids[1];

    let mut holder = db.pool().begin().await.expect("transaction");
    sqlx::query_scalar::<_, i64>("SELECT id FROM outbox WHERE id = $1 FOR UPDATE")
        .bind(held)
        .fetch_one(&mut *holder)
        .await
        .expect("takes the lock");

    let handler = Arc::new(Recorder::always(kind("email.send"), Outcome::Succeed));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(handler.clone());

    let pass = tokio::time::timeout(
        Duration::from_secs(5),
        dispatcher.dispatch_once(db.pool(), 10),
    )
    .await
    .expect("must not block behind the held row")
    .expect("runs");

    assert_eq!(pass.delivered, 2, "the other two go through");

    let still_pending: i64 =
        sqlx::query_scalar("SELECT id FROM outbox WHERE delivered_at IS NULL AND dead_at IS NULL")
            .fetch_one(db.pool())
            .await
            .expect("reads");
    assert_eq!(still_pending, held);

    holder.rollback().await.expect("releases");
    let pass = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(pass.delivered, 1, "and it goes through once released");
}

/// Two dispatchers against one outbox deliver each effect exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_dispatchers_never_deliver_the_same_effect() {
    const EFFECTS: usize = 24;

    let db = Arc::new(tenant_db().await);
    promise(
        &db,
        (0..EFFECTS)
            .map(|n| Effect::new(kind("email.send"), serde_json::json!({ "n": n })))
            .collect(),
    )
    .await;

    // Latency keeps both dispatchers genuinely in flight, so the claims overlap
    // instead of one finishing before the other starts.
    let make = || {
        let handler = Arc::new(
            Recorder::always(kind("email.send"), Outcome::Succeed)
                .with_latency(Duration::from_millis(5)),
        );
        (
            Dispatcher::new(fast_policy(8)).register(handler.clone()),
            handler,
        )
    };
    let (left, left_handler) = make();
    let (right, right_handler) = make();

    let (a, b) = {
        let db_a = Arc::clone(&db);
        let db_b = Arc::clone(&db);
        tokio::join!(
            tokio::spawn(async move { left.dispatch_until_idle(db_a.pool(), 4).await }),
            tokio::spawn(async move { right.dispatch_until_idle(db_b.pool(), 4).await }),
        )
    };
    a.expect("joins").expect("runs");
    b.expect("joins").expect("runs");

    let mut keys = left_handler.keys();
    keys.extend(right_handler.keys());
    let unique: std::collections::HashSet<_> = keys.iter().collect();

    assert_eq!(
        keys.len(),
        EFFECTS,
        "every effect delivered exactly once across both dispatchers"
    );
    assert_eq!(unique.len(), EFFECTS, "and none delivered twice: {keys:?}");
    assert_eq!(pending_count(&db).await, 0);
}

/// A dispatcher that dies mid-delivery holds nothing forever.
#[tokio::test]
async fn a_lapsed_lease_returns_an_effect_to_the_queue() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let handler = Arc::new(Recorder::always(kind("email.send"), Outcome::Succeed));
    let dispatcher = Dispatcher::new(fast_policy(8)).register(handler.clone());

    // Exactly what a claimed-then-crashed dispatcher leaves behind: a live lease
    // on a row nobody is working on.
    sqlx::query("UPDATE outbox SET attempts = 1, leased_until = now() + INTERVAL '1 hour'")
        .execute(db.pool())
        .await
        .expect("leases");

    let blocked = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(blocked.claimed, 0, "a live lease is respected");

    sqlx::query("UPDATE outbox SET leased_until = now() - INTERVAL '1 second'")
        .execute(db.pool())
        .await
        .expect("expires the lease");

    let recovered = dispatcher.dispatch_once(db.pool(), 10).await.expect("runs");
    assert_eq!(
        recovered.delivered, 1,
        "expiry, not release, is what makes the lease crash-safe"
    );
}

#[tokio::test]
async fn health_reports_an_empty_outbox_as_healthy() {
    let db = tenant_db().await;
    let mut conn = db.pool().acquire().await.expect("connection");

    let health = outbox_health(&mut conn).await.expect("reads");
    assert_eq!(health.pending, 0);
    assert_eq!(health.dead, 0);
    assert_eq!(health.backlog_age_seconds, None);
    assert!(health.is_healthy(60));
}

#[tokio::test]
async fn a_dispatcher_with_no_handlers_does_nothing_at_all() {
    let db = tenant_db().await;
    promise(
        &db,
        vec![Effect::new(kind("email.send"), serde_json::json!({}))],
    )
    .await;

    let pass = Dispatcher::new(fast_policy(8))
        .dispatch_once(db.pool(), 10)
        .await
        .expect("runs");

    assert_eq!(pass, erp_eventlog::Dispatched::default());
    assert_eq!(
        pending_count(&db).await,
        1,
        "and leaves the work for someone else"
    );
}
