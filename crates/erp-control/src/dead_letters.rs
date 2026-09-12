//! The control plane's dead letters, for platform staff.
//!
//! Its outbox is the tenant's table (`the_two_outboxes_are_the_same_table`), so
//! these are `erp_eventlog`'s own list, requeue and dismiss pointed at the
//! control pool rather than a copy of them. What they add is the audit trail:
//! a requeue sends somebody a message and a dismissal deletes a promise, and
//! both are done by a person, who is named.

use erp_eventlog::{DeadLetter, Handled};

use crate::model::Actor;
use crate::{AccessError, ControlPlane};

impl ControlPlane {
    /// Everything the control plane gave up on — signup, invitation and reset
    /// emails, sign-in texts — oldest first, at most `limit`.
    pub async fn dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, AccessError> {
        let mut conn = self.pool.acquire().await?;
        Ok(erp_eventlog::dead_letters(&mut conn, limit).await?)
    }

    /// Puts one back in the queue, due now; the next platform pass sends it.
    /// `false` when `id` is not a dead letter.
    pub async fn requeue_dead_letter(&self, id: i64, actor: Actor) -> Result<bool, AccessError> {
        let mut conn = self.pool.acquire().await?;
        let handled = erp_eventlog::requeue(&mut conn, id).await?;
        drop(conn);
        self.dealt_with(id, handled, "effect.requeued", actor).await
    }

    /// Deletes one that should not be sent — see [`erp_eventlog::dismiss`].
    /// `false` when `id` is not a dead letter; nothing pending or delivered is
    /// ever touched.
    pub async fn dismiss_dead_letter(&self, id: i64, actor: Actor) -> Result<bool, AccessError> {
        let mut conn = self.pool.acquire().await?;
        let handled = erp_eventlog::dismiss(&mut conn, id).await?;
        drop(conn);
        self.dealt_with(id, handled, "effect.dismissed", actor)
            .await
    }

    /// Records what was done, naming the effect by kind and key — the key says
    /// which signup, invitation, reset or code it was — and never by payload,
    /// which holds the address and, for most of these, the credential.
    async fn dealt_with(
        &self,
        id: i64,
        handled: Option<Handled>,
        action: &str,
        actor: Actor,
    ) -> Result<bool, AccessError> {
        let Some(handled) = handled else {
            return Ok(false);
        };
        self.record(
            actor,
            None,
            action,
            "effect",
            &id.to_string(),
            serde_json::json!({
                "kind": handled.kind.to_string(),
                "idempotency_key": handled.idempotency_key,
            }),
        )
        .await?;
        Ok(true)
    }
}
