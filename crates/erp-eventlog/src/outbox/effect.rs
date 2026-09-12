//! What a command promised to do, and how the promise is recorded.

use erp_types::{EffectKind, LogPosition, Timestamp};
use sqlx::PgConnection;

/// Longest an idempotency key may be. Matches the column's `CHECK`.
const MAX_KEY_LEN: usize = 200;

/// Something a command decided to do to the outside world, as a value.
///
/// # Why this is not just a function call
///
/// Architecture decision D9: no domain code performs I/O. A handler that sends
/// an email inline has to choose between sending before the transaction commits
/// — and mailing a customer about something that then rolled back — or sending
/// after, and losing the send entirely if the process dies in between.
///
/// An `Effect` is the third option: write the intention in the same transaction
/// as the events, and let a dispatcher deliver it afterwards. After commit,
/// either the events and the promise are both durable or neither is.
///
/// # Building one
///
/// ```
/// # use erp_eventlog::Effect;
/// # use erp_types::EffectKind;
/// # fn kind(s: &str) -> EffectKind { EffectKind::new(s).unwrap() }
/// let effect = Effect::new(
///     kind("email.send"),
///     serde_json::json!({ "to": "a@example.com", "template": "welcome" }),
/// );
/// ```
///
/// The payload must carry **everything the handler needs**, resolved at command
/// time. A handler that looks up the customer's current address defeats L5: the
/// effect would be delivered against configuration that may have changed since
/// the decision was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    kind: EffectKind,
    payload: serde_json::Value,
    key: Option<String>,
}

impl Effect {
    /// An effect whose idempotency key is derived from its cause.
    ///
    /// The derived key is `{position}:{index}`, which is unique without any
    /// coordination because log positions are. It makes *delivery* idempotent —
    /// the dispatcher can retry freely — but not *execution*: running the same
    /// command twice appends at two positions and so promises twice. Deduping
    /// across executions is what [`with_key`](Self::with_key) is for.
    pub fn new(kind: EffectKind, payload: serde_json::Value) -> Self {
        Self {
            kind,
            payload,
            key: None,
        }
    }

    /// An effect with a caller-chosen idempotency key.
    ///
    /// Enqueueing the same key twice inserts one row, whatever the two commands
    /// were. Use it when the *intention* is what must not repeat — one welcome
    /// email per account, whether or not signup was retried — and derive the key
    /// from the thing being deduplicated, never from a clock or a random value.
    pub fn with_key(kind: EffectKind, key: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            kind,
            payload,
            key: Some(key.into()),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &EffectKind {
        &self.kind
    }

    #[must_use]
    pub const fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    /// The pinned key, if the caller chose one.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    /// An effect with no pinned key was enqueued by something that appended no
    /// events, so there is no position to derive a key from.
    ///
    /// The situation is real — a scheduled sweep promising work — and the fix is
    /// to pin a key. Refusing is better than inventing one, because an invented
    /// key means the dispatcher cannot tell a retry from a new promise.
    #[error(
        "an effect of kind {kind} has no idempotency key and no causing event to derive one from"
    )]
    NoKey { kind: EffectKind },
    /// A pinned key is longer than the column allows.
    #[error("idempotency key is {len} characters; the maximum is {MAX_KEY_LEN}")]
    KeyTooLong { len: usize },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl erp_i18n::Localize for EnqueueError {
    fn message(&self) -> erp_i18n::Message {
        // All three are programming or infrastructure faults, never something a
        // user did. They reach a user only as "something went wrong on our side".
        erp_i18n::Message::new(crate::messages::INTERNAL)
    }
}

/// Records effects in the outbox.
///
/// **Call this inside the transaction that appends the events**, which is the
/// entire point: the promise and the facts that justify it commit together or
/// not at all. [`execute`](crate::execute) does it for you.
///
/// `cause` is the log position of the first event the command wrote, used both
/// to derive keys and to trace an effect back to its origin. `None` is for
/// callers that appended nothing, and then every effect must pin its own key.
///
/// Returns how many rows were actually inserted, which is fewer than `effects.len()`
/// when a pinned key was already present — that is the deduplication working,
/// not an error.
pub async fn enqueue(
    conn: &mut PgConnection,
    cause: Option<LogPosition>,
    effects: &[Effect],
) -> Result<usize, EnqueueError> {
    if effects.is_empty() {
        return Ok(0);
    }

    let mut keys = Vec::with_capacity(effects.len());
    for (index, effect) in effects.iter().enumerate() {
        let key = match (effect.key(), cause) {
            (Some(pinned), _) => pinned.to_owned(),
            (None, Some(position)) => format!("{position}:{index}"),
            (None, None) => {
                return Err(EnqueueError::NoKey {
                    kind: effect.kind.clone(),
                });
            }
        };
        if key.len() > MAX_KEY_LEN {
            return Err(EnqueueError::KeyTooLong { len: key.len() });
        }
        keys.push(key);
    }

    let kinds: Vec<String> = effects.iter().map(|e| e.kind.as_str().to_owned()).collect();
    let payloads: Vec<serde_json::Value> = effects.iter().map(|e| e.payload.clone()).collect();

    // `DO NOTHING` rather than an error: a key that is already present means the
    // promise is already recorded, which is success. It also absorbs duplicates
    // *within* this batch, so a caller that pins one key on two effects gets one
    // row rather than a constraint violation.
    let inserted = sqlx::query!(
        r#"
        INSERT INTO outbox (idempotency_key, kind, payload, caused_by)
        SELECT t.key, t.kind, t.payload, $4
          FROM UNNEST($1::text[], $2::text[], $3::jsonb[]) AS t(key, kind, payload)
        ON CONFLICT (idempotency_key) DO NOTHING
        "#,
        &keys,
        &kinds,
        &payloads,
        cause.map(LogPosition::get),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    Ok(usize::try_from(inserted).unwrap_or(usize::MAX))
}

/// An effect waiting to be delivered, as handed to a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEffect {
    pub id: i64,
    pub kind: EffectKind,
    pub payload: serde_json::Value,
    /// Stable across retries of this effect. Pass it to any downstream API that
    /// accepts an idempotency key, so a delivery this dispatcher believes failed
    /// but which actually succeeded is not performed twice.
    pub idempotency_key: String,
    /// How many times this effect has been *claimed*, including now. One on the
    /// first delivery.
    pub attempts: i32,
    /// Log position of the command that promised it.
    pub caused_by: Option<LogPosition>,
    pub enqueued_at: Timestamp,
    /// **The token this claim holds the row under.** Minted by the database at
    /// claim time; renewing the lease and settling the effect both say
    /// `WHERE leased_by = <this>`, so a dispatcher that lost the row cannot
    /// extend or overwrite what the next one is doing with it.
    pub lease: String,
}

impl PendingEffect {
    /// Decodes the payload into a handler's own type.
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

/// An effect this system promised and gave up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    pub id: i64,
    pub kind: EffectKind,
    pub idempotency_key: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub enqueued_at: Timestamp,
    pub dead_at: Timestamp,
}

/// Everything given up on, oldest first.
///
/// The first version counted these and nothing else: a provider outage longer
/// than the retry schedule dead-lettered every effect promised during it, and
/// the only way back was hand-written SQL. This is the list somebody reads, and
/// [`requeue`] is the way back.
pub async fn dead_letters(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<DeadLetter>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT id, kind, idempotency_key, attempts, last_error, enqueued_at,
                  dead_at as "dead_at!"
             FROM outbox
            WHERE dead_at IS NOT NULL
            ORDER BY dead_at ASC, id
            LIMIT $1"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(DeadLetter {
                id: r.id,
                kind: EffectKind::new(r.kind).ok()?,
                idempotency_key: r.idempotency_key,
                attempts: r.attempts,
                last_error: r.last_error,
                enqueued_at: r.enqueued_at,
                dead_at: r.dead_at,
            })
        })
        .collect())
}

/// The dead letter [`requeue`] or [`dismiss`] acted on, named the way an audit
/// line may name it: its kind and its key, never its payload — which, for a
/// sign-in code or a reset link, is the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    pub kind: EffectKind,
    pub idempotency_key: String,
}

/// **Puts a dead letter back in the queue**, due now, with its attempts reset.
///
/// `None` when there was no dead letter with that id — already requeued or
/// dismissed, or never dead. The idempotency key travels with it, so a delivery that in fact
/// succeeded before the effect was given up on is still not performed twice by
/// a handler that honours the key.
pub async fn requeue(conn: &mut PgConnection, id: i64) -> Result<Option<Handled>, sqlx::Error> {
    let row = sqlx::query!(
        "UPDATE outbox
            SET dead_at = NULL, attempts = 0, next_attempt_at = now(),
                leased_until = NULL, last_error = NULL
          WHERE id = $1 AND dead_at IS NOT NULL
          RETURNING kind, idempotency_key",
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| handled(r.kind, r.idempotency_key)).transpose()
}

/// **Deletes a dead letter**, and nothing else: `None` for a row that is
/// pending, delivered or gone.
///
/// For the one that should not be sent: a promise that went stale while it was
/// being retried. A credential that expires sooner than the retry schedule
/// runs out — a sign-in code, a reset link — is dead before it is dead-lettered,
/// and requeueing it mails somebody a link that no longer works. Without this
/// the only ways to clear one were to send it or to write SQL, and a dead
/// letter nobody can clear keeps `no_dead_letters` firing until people stop
/// reading it.
///
/// This is the one way a dead letter is deleted. Both outbox migrations still
/// say one is "never deleted"; they predate this and sqlx checksums them. What
/// survives a dismissal is the caller's to keep: it gets back the kind and
/// key, and the control plane's route records them (`effect.dismissed`).
pub async fn dismiss(conn: &mut PgConnection, id: i64) -> Result<Option<Handled>, sqlx::Error> {
    let row = sqlx::query!(
        "DELETE FROM outbox WHERE id = $1 AND dead_at IS NOT NULL
          RETURNING kind, idempotency_key",
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| handled(r.kind, r.idempotency_key)).transpose()
}

fn handled(kind: String, idempotency_key: String) -> Result<Handled, sqlx::Error> {
    Ok(Handled {
        kind: EffectKind::new(kind).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        idempotency_key,
    })
}

/// Deletes delivered effects older than `before`.
///
/// A delivered row is a receipt, and receipts are kept for a while and not for
/// ever: the partial index keeps the queue fast whatever this table holds, but
/// the table itself is cloned and backed up with every tenant database.
pub async fn sweep_delivered(
    conn: &mut PgConnection,
    before: Timestamp,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "DELETE FROM outbox WHERE delivered_at IS NOT NULL AND delivered_at < $1",
        before,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Deletes recorded provider callbacks older than `before`.
///
/// The table is `migrations/tenant/0011_webhooks.sql` — kernel-level, like the
/// outbox, and written by the API's hook route. A callback is a doorbell that
/// has been answered; keeping its payload for ninety days is for the argument
/// with the provider, not for ever.
pub async fn sweep_webhook_events(
    conn: &mut PgConnection,
    before: Timestamp,
) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query!("DELETE FROM webhook_event WHERE received_at < $1", before,)
            .execute(&mut *conn)
            .await?
            .rows_affected(),
    )
}

/// Counts an operator, and either plane's health check, cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxHealth {
    /// Promised, not yet delivered, not yet given up on.
    pub pending: i64,
    /// Given up on. Architecture §7 asserts this is zero.
    pub dead: i64,
    /// Age in seconds of the oldest pending effect. `None` when nothing pends.
    ///
    /// The number that actually matters: a large backlog that is moving is fine,
    /// a small one that is not is an outage. Counting rows alone cannot tell the
    /// difference.
    pub backlog_age_seconds: Option<i64>,
}

impl OutboxHealth {
    /// Whether the outbox is keeping up and nothing has been abandoned.
    #[must_use]
    pub fn is_healthy(&self, max_backlog_age_seconds: i64) -> bool {
        self.dead == 0
            && self
                .backlog_age_seconds
                .is_none_or(|a| a <= max_backlog_age_seconds)
    }
}

/// Reads the outbox's health counters.
pub async fn outbox_health(conn: &mut PgConnection) -> Result<OutboxHealth, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT count(*) FILTER (
                   WHERE delivered_at IS NULL AND dead_at IS NULL
               )                                          AS "pending!",
               count(*) FILTER (WHERE dead_at IS NOT NULL) AS "dead!",
               EXTRACT(EPOCH FROM now() - min(enqueued_at) FILTER (
                   WHERE delivered_at IS NULL AND dead_at IS NULL
               ))::BIGINT                                  AS backlog_age_seconds
          FROM outbox
        "#
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(OutboxHealth {
        pending: row.pending,
        dead: row.dead,
        backlog_age_seconds: row.backlog_age_seconds,
    })
}
