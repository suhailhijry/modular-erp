//! Keeping the promises in the outbox.
//!
//! # The one rule
//!
//! **No database transaction is ever open while a handler runs.** Delivery is
//! network I/O with a timeout measured in seconds; holding a transaction across
//! it would pin a connection, hold row locks, and keep `xmin` back so autovacuum
//! cannot clean up behind it. So a delivery is three separate steps: claim in a
//! short transaction, deliver outside one, settle in another.
//!
//! # What that costs, and why it is the right trade
//!
//! Because claiming and settling are separate commits, a dispatcher that dies
//! after delivering but before settling leaves an effect that was performed and
//! not recorded as such. The lease lapses and it is delivered again.
//!
//! Delivery is therefore **at least once**, and that is not fixable here — it is
//! the standard two-generals problem between this process and whatever it is
//! calling.
//!
//! # A slow delivery keeps its lease
//!
//! The lease is short, so a dispatcher that dies gives the row back quickly.
//! That made a *slow* delivery indistinguishable from a dead dispatcher: an
//! email through a congested relay outlived the lease and was claimed again
//! while still in flight, and a text message has no idempotency key at the
//! provider. So a claim mints a token per row (`leased_by`), and while a
//! handler runs a heartbeat renews `leased_until` — only `WHERE leased_by` is
//! ours — every third of the lease. A live dispatcher is never double-claimed
//! however long the provider takes; a dead one's lease still lapses, because
//! nothing renews it. It is fixed one level down instead: every [`PendingEffect`] carries a
//! stable `idempotency_key`, and a handler that passes it to the downstream API
//! makes the second delivery a no-op on the far side. A handler that ignores it
//! is the thing that sends two emails, not this loop.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use erp_types::{EffectKind, LogPosition};
use sqlx::PgPool;

use super::effect::PendingEffect;

/// Why a delivery did not succeed.
///
/// The distinction is the handler's to make and it matters: a 503 is a different
/// event from a 400. Retrying a permanent failure wastes attempts and, worse,
/// delays the dead-letter signal that tells an operator something needs looking
/// at.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// Might work later — a timeout, a 5xx, a refused connection.
    #[error("{0}")]
    Retryable(String),
    /// Will never work — a 400, a malformed payload, an address that does not
    /// exist. Dead-lettered immediately rather than retried.
    #[error("{0}")]
    Permanent(String),
}

/// Performs one kind of effect.
///
/// Implementations live in modules, not in the kernel (D11): the kernel knows
/// that effects exist and how to deliver them reliably, never what any of them
/// mean.
#[async_trait::async_trait]
pub trait EffectHandler: Send + Sync {
    /// Which effects this handles. One handler per kind.
    fn kind(&self) -> EffectKind;

    /// Performs the effect.
    ///
    /// May be called more than once for the same effect — see the module docs.
    /// Pass `effect.idempotency_key` downstream wherever the API accepts one.
    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError>;
}

/// How hard to try, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Claims after which an effect is dead-lettered.
    pub max_attempts: i32,
    /// Delay before the second attempt. Doubles each time, capped.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    /// How long a claim is held before another dispatcher may take the effect.
    ///
    /// Must comfortably exceed the slowest handler's timeout. Too short and a
    /// slow delivery is duplicated while the first is still in flight; too long
    /// and a crashed dispatcher's work sits idle. Handler timeout × 3 is a
    /// reasonable starting point.
    pub lease: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // Eight attempts at doubling from one second reaches roughly two
            // minutes of total delay — long enough to ride out a deploy or a
            // brief upstream outage, short enough that a genuinely broken effect
            // shows up as a dead letter the same day.
            // **Sixteen attempts with the backoff capped at an hour** is a
            // little over four hours of trying. The first version gave up after
            // eight attempts capped at a minute — about eight minutes — which
            // meant a provider outage over lunch dead-lettered every email, text
            // and payment callback promised during it, permanently.
            max_attempts: 16,
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_hours(1),
            lease: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Delay before the next attempt, doubling from the base.
    ///
    /// Integer arithmetic throughout: `float_arithmetic` is denied
    /// workspace-wide, and exponential backoff has no need of fractions anyway.
    #[must_use]
    pub fn backoff(&self, attempts: i32) -> Duration {
        // Capped well below the shift width; the `max_backoff` clamp below makes
        // anything past a handful of doublings indistinguishable regardless.
        let exponent = u32::try_from(attempts.saturating_sub(1))
            .unwrap_or(0)
            .min(20);
        let base = u64::try_from(self.base_backoff.as_millis()).unwrap_or(u64::MAX);
        let millis = base.saturating_mul(1u64 << exponent);
        Duration::from_millis(millis).min(self.max_backoff)
    }
}

/// What one dispatch pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dispatched {
    pub claimed: usize,
    pub delivered: usize,
    /// Failed and scheduled for another attempt.
    pub retrying: usize,
    /// Given up on: attempts exhausted, or a permanent failure.
    pub dead: usize,
    /// Deliveries abandoned because the lease was found gone mid-flight. Zero
    /// unless renewals were failing for longer than the lease.
    pub lost: usize,
}

impl Dispatched {
    /// Whether more may be waiting. Drives "keep going until idle" loops.
    #[must_use]
    pub const fn did_work(&self) -> bool {
        self.claimed > 0
    }

    /// Folds one settled effect into the tally.
    pub(crate) const fn count(&mut self, settlement: &Settlement) {
        match settlement {
            Settlement::Delivered => self.delivered += 1,
            Settlement::Retrying { .. } => self.retrying += 1,
            Settlement::Dead { .. } => self.dead += 1,
            Settlement::Lost => self.lost += 1,
            Settlement::Abandoned => {}
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// A stored row does not satisfy the invariants its types promise. Stops
    /// rather than guessing (L6).
    #[error("outbox row {id} is invalid: {reason}")]
    Corrupt { id: i64, reason: String },
    /// The lease could not be renewed for a reason that is not the database
    /// refusing — a connection could not be had. Logged and retried on the
    /// next beat; the lease itself is still live until it lapses.
    #[error("the lease could not be renewed: {0}")]
    Lease(String),
}

impl erp_i18n::Localize for DispatchError {
    fn message(&self) -> erp_i18n::Message {
        // Operator-facing. A user never waits on a dispatch pass.
        erp_i18n::Message::new(crate::messages::INTERNAL)
    }
}

/// Claims effects and delivers them.
///
/// # Concurrency
///
/// One pass delivers its batch sequentially. Concurrency comes from running
/// **more dispatchers**, which is safe by construction: the claim uses
/// `FOR UPDATE SKIP LOCKED`, so two dispatchers never see the same row, and
/// `two_dispatchers_never_deliver_the_same_effect` proves it. That is a property
/// worth having anyway — the fleet has many workers — and it makes intra-batch
/// concurrency an optimization to add against a measurement rather than a
/// guess.
pub struct Dispatcher {
    handlers: HashMap<EffectKind, Arc<dyn EffectHandler>>,
    policy: RetryPolicy,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("kinds", &self.handlers.keys().collect::<Vec<_>>())
            .field("policy", &self.policy)
            .finish()
    }
}

impl Dispatcher {
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            handlers: HashMap::new(),
            policy,
        }
    }

    /// Registers a handler. A second handler for the same kind replaces the
    /// first.
    #[must_use]
    pub fn register(mut self, handler: Arc<dyn EffectHandler>) -> Self {
        self.handlers.insert(handler.kind(), handler);
        self
    }

    /// The kinds this dispatcher can deliver.
    #[must_use]
    pub fn kinds(&self) -> Vec<EffectKind> {
        self.handlers.keys().cloned().collect()
    }

    #[must_use]
    pub const fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Claims up to `limit` effects and delivers them.
    ///
    /// A convenience over [`claim`](Self::claim), [`deliver`](Self::deliver) and
    /// [`settle`](Self::settle) for callers holding a pool. A worker uses the
    /// three steps directly, so each database moment draws a metered connection
    /// from `TenantDb` and none is held across the delivery.
    ///
    /// # Effects with no registered handler are not claimed
    ///
    /// The claim filters on the kinds this dispatcher knows, so a worker
    /// deployed without some module's handler simply leaves those rows for a
    /// worker that has it. The alternative — claim, fail, back off — would burn
    /// attempts and dead-letter a tenant's effects during an ordinary staggered
    /// rollout.
    ///
    /// If *nobody* ever picks them up, the rows age and the backlog-age health
    /// check fires, in either plane. That is the correct place for "no worker
    /// can handle this" to surface: an alarm about a stalled queue, not a storm
    /// of dead letters.
    pub async fn dispatch_once(
        &self,
        pool: &PgPool,
        limit: i64,
    ) -> Result<Dispatched, DispatchError> {
        let claimed = {
            let mut conn = pool.acquire().await?;
            self.claim(&mut conn, limit).await?
        };

        let mut result = Dispatched {
            claimed: claimed.len(),
            ..Dispatched::default()
        };

        for effect in claimed {
            // No connection is held here. See the module docs. The heartbeat
            // takes one from the pool for each renewal and gives it back.
            let settlement = self
                .deliver(&effect, || async {
                    let mut conn = pool.acquire().await?;
                    self.renew(&mut conn, &effect).await
                })
                .await;

            let mut conn = pool.acquire().await?;
            self.settle(&mut conn, &effect, &settlement).await?;
            result.count(&settlement);
        }

        Ok(result)
    }

    /// Dispatches until nothing more is **due**.
    ///
    /// Not the same as "until the outbox is empty": an effect that failed and is
    /// backing off is not due, so this returns while it is still owed. That is
    /// also why it terminates — a failure pushes `next_attempt_at` into the
    /// future, so the next pass cannot re-claim it.
    pub async fn dispatch_until_idle(
        &self,
        pool: &PgPool,
        limit: i64,
    ) -> Result<Dispatched, DispatchError> {
        let mut total = Dispatched::default();
        loop {
            let pass = self.dispatch_once(pool, limit).await?;
            if !pass.did_work() {
                return Ok(total);
            }
            total.claimed += pass.claimed;
            total.delivered += pass.delivered;
            total.retrying += pass.retrying;
            total.dead += pass.dead;
            total.lost += pass.lost;
        }
    }

    // -----------------------------------------------------------------------
    // The three steps, for callers who own their connections
    // -----------------------------------------------------------------------

    /// Takes a lease on up to `limit` due effects this dispatcher can handle.
    ///
    /// Short and self-contained: hold the connection for this call and let it
    /// go, because the delivery that follows must not hold one.
    pub async fn claim(
        &self,
        conn: &mut sqlx::PgConnection,
        limit: i64,
    ) -> Result<Vec<PendingEffect>, DispatchError> {
        let kinds = self.kinds();
        if kinds.is_empty() {
            return Ok(Vec::new());
        }

        let kind_strings: Vec<String> = kinds.iter().map(|k| k.as_str().to_owned()).collect();
        let lease_millis = i64::try_from(self.policy.lease.as_millis()).unwrap_or(i64::MAX);

        // `SKIP LOCKED` is what makes concurrent dispatchers safe: a row another
        // dispatcher is claiming right now is passed over instead of blocking.
        // `leased_until` covers the longer window — a dispatcher that claimed
        // and then died holds nothing, so expiry is what returns the row.
        let rows = sqlx::query!(
            r#"
            UPDATE outbox
               SET attempts      = attempts + 1,
                   leased_until  = now() + ($2::BIGINT * INTERVAL '1 millisecond'),
                   leased_by     = gen_random_uuid()::text
             WHERE id IN (
                 SELECT id
                   FROM outbox
                  WHERE delivered_at IS NULL
                    AND dead_at IS NULL
                    AND next_attempt_at <= now()
                    AND (leased_until IS NULL OR leased_until <= now())
                    AND kind = ANY($3::TEXT[])
                  ORDER BY next_attempt_at, id
                  LIMIT $1
                    FOR UPDATE SKIP LOCKED
             )
            RETURNING id, kind, payload, idempotency_key, attempts, caused_by, enqueued_at,
                      leased_by as "lease!"
            "#,
            limit,
            lease_millis,
            &kind_strings,
        )
        .fetch_all(&mut *conn)
        .await?;

        rows.into_iter()
            .map(|r| {
                let id = r.id;
                Ok(PendingEffect {
                    id,
                    // Validated rather than trusted: this is where rows written
                    // by an older version of the system arrive.
                    kind: EffectKind::new(r.kind).map_err(|e| DispatchError::Corrupt {
                        id,
                        reason: e.to_string(),
                    })?,
                    payload: r.payload,
                    idempotency_key: r.idempotency_key,
                    attempts: r.attempts,
                    caused_by: r.caused_by.map(LogPosition::new).transpose().map_err(|e| {
                        DispatchError::Corrupt {
                            id,
                            reason: e.to_string(),
                        }
                    })?,
                    enqueued_at: r.enqueued_at,
                    lease: r.lease,
                })
            })
            .collect()
    }

    /// **Extends the lease on an effect this dispatcher holds.** `false` when
    /// the row is no longer ours — settled, or claimed by somebody else after
    /// the lease lapsed — which is the signal to stop and not settle.
    pub async fn renew(
        &self,
        conn: &mut sqlx::PgConnection,
        effect: &PendingEffect,
    ) -> Result<bool, DispatchError> {
        let lease_millis = i64::try_from(self.policy.lease.as_millis()).unwrap_or(i64::MAX);
        let touched = sqlx::query!(
            "UPDATE outbox
                SET leased_until = now() + ($2::BIGINT * INTERVAL '1 millisecond')
              WHERE id = $1 AND leased_by = $3
                AND delivered_at IS NULL AND dead_at IS NULL",
            effect.id,
            lease_millis,
            effect.lease,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
        Ok(touched == 1)
    }

    /// Performs one claimed effect and decides what should be recorded.
    ///
    /// **The only step that touches the outside world, and it touches no
    /// database.** Call it with no connection held: a handler's timeout is
    /// seconds, and a connection held for that long is a connection not serving
    /// a customer.
    ///
    /// Never fails — a delivery that went wrong is a [`Settlement`], not an
    /// error, because "it failed" is information the outbox has to record.
    /// Performs one effect, renewing its lease while the handler runs.
    ///
    /// `renew` is how this dispatcher reaches the database for the heartbeat —
    /// a pool or a tenant handle, whichever the caller has. It is called every
    /// third of the lease; a renewal that finds the lease gone ends the delivery
    /// as [`Settlement::Lost`], and one that fails for another reason is logged
    /// and tried again on the next beat, the lease still being live.
    pub async fn deliver<F, Fut>(&self, effect: &PendingEffect, renew: F) -> Settlement
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<bool, DispatchError>>,
    {
        let Some(handler) = self.handlers.get(&effect.kind) else {
            // Unreachable via `claim`, which filters on exactly these kinds.
            // Leaving the lease to lapse beats a panic: the row returns to the
            // queue and a worker that does know the kind takes it.
            tracing::error!(
                id = effect.id,
                kind = %effect.kind,
                "claimed an effect with no handler; the claim filter is wrong"
            );
            return Settlement::Abandoned;
        };

        let beat = (self.policy.lease / 3).max(Duration::from_millis(10));
        let delivery = handler.deliver(effect);
        tokio::pin!(delivery);
        let mut ticks = tokio::time::interval(beat);
        // The first tick of an interval is immediate; the lease was just taken.
        ticks.tick().await;
        // Labelled, because `select!` has a loop of its own for a bare `break`
        // to land in.
        let outcome = 'heartbeat: loop {
            tokio::select! {
                biased;
                result = &mut delivery => break 'heartbeat result,
                _ = ticks.tick() => match renew().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::error!(
                            id = effect.id,
                            kind = %effect.kind,
                            key = %effect.idempotency_key,
                            "the lease on an effect was lost mid-delivery; not settling it"
                        );
                        return Settlement::Lost;
                    }
                    Err(e) => tracing::warn!(
                        id = effect.id,
                        kind = %effect.kind,
                        error = %e,
                        "could not renew an effect's lease; will try again on the next beat"
                    ),
                },
            }
        };
        match outcome {
            Ok(()) => Settlement::Delivered,
            Err(failure) => {
                let give_up = matches!(failure, DeliveryError::Permanent(_))
                    || effect.attempts >= self.policy.max_attempts;

                if give_up {
                    tracing::error!(
                        id = effect.id,
                        kind = %effect.kind,
                        key = %effect.idempotency_key,
                        attempts = effect.attempts,
                        error = %failure,
                        "effect dead-lettered"
                    );
                    Settlement::Dead {
                        error: failure.to_string(),
                    }
                } else {
                    let delay = self.policy.backoff(effect.attempts);
                    tracing::warn!(
                        id = effect.id,
                        kind = %effect.kind,
                        attempts = effect.attempts,
                        retry_in_ms = delay.as_millis(),
                        error = %failure,
                        "effect delivery failed; will retry"
                    );
                    Settlement::Retrying {
                        delay,
                        error: failure.to_string(),
                    }
                }
            }
        }
    }

    /// Records what [`deliver`](Self::deliver) concluded.
    ///
    /// Separate from the delivery, and therefore not atomic with it — which is
    /// exactly why delivery is at-least-once and why every effect carries an
    /// idempotency key. See the module docs.
    pub async fn settle(
        &self,
        conn: &mut sqlx::PgConnection,
        effect: &PendingEffect,
        settlement: &Settlement,
    ) -> Result<(), DispatchError> {
        match settlement {
            Settlement::Delivered => {
                let touched = sqlx::query!(
                    "UPDATE outbox
                        SET delivered_at = now(), leased_until = NULL, last_error = NULL
                      WHERE id = $1 AND leased_by = $2",
                    effect.id,
                    effect.lease,
                )
                .execute(&mut *conn)
                .await?
                .rows_affected();
                not_ours(effect, touched);
            }
            Settlement::Retrying { delay, error } => {
                let delay_millis = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
                let touched = sqlx::query!(
                    "UPDATE outbox
                        SET next_attempt_at = now() + ($2::BIGINT * INTERVAL '1 millisecond'),
                            leased_until    = NULL,
                            last_error      = $3
                      WHERE id = $1 AND leased_by = $4",
                    effect.id,
                    delay_millis,
                    truncate(error),
                    effect.lease,
                )
                .execute(&mut *conn)
                .await?
                .rows_affected();
                not_ours(effect, touched);
            }
            Settlement::Dead { error } => {
                let touched = sqlx::query!(
                    "UPDATE outbox
                        SET dead_at = now(), leased_until = NULL, last_error = $2
                      WHERE id = $1 AND leased_by = $3",
                    effect.id,
                    truncate(error),
                    effect.lease,
                )
                .execute(&mut *conn)
                .await?
                .rows_affected();
                not_ours(effect, touched);
            }
            // Deliberately writes nothing: the lease lapses and the row returns
            // to the queue for a worker that can handle it.
            Settlement::Abandoned | Settlement::Lost => {}
        }
        Ok(())
    }
}

/// A settlement that found the row no longer ours. Not an error to the caller
/// — the effect belongs to whoever holds it now — but loud, because it means a
/// delivery may have happened twice.
fn not_ours(effect: &PendingEffect, touched: u64) {
    if touched == 0 {
        tracing::error!(
            id = effect.id,
            kind = %effect.kind,
            key = %effect.idempotency_key,
            "settled an effect whose lease had moved on; the delivery may have been repeated"
        );
    }
}

/// What a delivery concluded, before it is recorded.
///
/// A value rather than a `Result` so the decision — retry, or give up — is a
/// pure function of the policy and the failure, testable without a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    Delivered,
    Retrying {
        delay: Duration,
        error: String,
    },
    Dead {
        error: String,
    },
    /// The lease was found gone while the handler ran. Nothing is written: the
    /// row belongs to whoever holds it now.
    Lost,
    /// No handler was registered. Nothing is recorded; the lease lapses.
    Abandoned,
}

/// Caps a handler's error text.
///
/// An upstream that echoes a megabyte of HTML into its error message should not
/// be able to bloat the outbox one row at a time.
fn truncate(error: &str) -> String {
    const MAX: usize = 1000;
    if error.len() <= MAX {
        return error.to_owned();
    }
    let mut end = MAX;
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &error[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_then_stops() {
        let policy = RetryPolicy {
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(800),
            ..RetryPolicy::default()
        };

        assert_eq!(policy.backoff(1), Duration::from_millis(100));
        assert_eq!(policy.backoff(2), Duration::from_millis(200));
        assert_eq!(policy.backoff(4), Duration::from_millis(800));
        assert_eq!(
            policy.backoff(5),
            Duration::from_millis(800),
            "the cap holds"
        );
        assert_eq!(
            policy.backoff(1_000_000),
            Duration::from_millis(800),
            "and does not overflow into something small"
        );
    }

    #[test]
    fn backoff_handles_a_zeroth_attempt_without_underflowing() {
        // `attempts` is 1 on the first claim, but a stored row could hold
        // anything, and `0 - 1` on an unsigned shift is a panic.
        let policy = RetryPolicy::default();
        assert_eq!(policy.backoff(0), policy.base_backoff);
    }

    #[test]
    fn truncate_does_not_split_a_character() {
        let arabic = "ح".repeat(2000);
        let short = truncate(&arabic);
        assert!(short.len() <= 1004, "{}", short.len());
        // The real assertion: it is still valid UTF-8 and still Arabic, which a
        // naive `&s[..1000]` would panic on rather than corrupt.
        assert!(short.starts_with('ح'));
    }
}
