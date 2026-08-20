//! What a projection is, and the only inputs it is allowed.

use serde::de::DeserializeOwned;
use erp_eventlog::{Envelope, UpcastError, Upcasters};
use erp_types::{LogPosition, Timestamp};
use sqlx::PgConnection;
use uuid::Uuid;

/// A set of tables that must agree with one another.
///
/// The unit of consistency *and* of replay. Tables in one group advance together
/// in a single transaction against a single checkpoint; tables in different
/// groups never read each other.
///
/// Group by **what must agree**, not by what is related. A ledger's postings and
/// its trial balance must agree, so they are one group. An audit log that merely
/// mentions the same accounts does not, so it is another — and keeping it
/// separate means it can fall behind, be rebuilt, or fail without touching the
/// numbers anyone is looking at.
pub trait ProjectionGroup: Send + Sync + 'static {
    /// Checkpoint key. Lowercase, underscores.
    const NAME: &'static str;

    /// The Postgres schema this group owns exclusively.
    ///
    /// The runner sets `search_path` to it for the duration of each transaction,
    /// so a projection that reaches into another group's tables fails the first
    /// time it runs rather than passing review (law L3).
    const SCHEMA: &'static str;
}

/// Fixed namespace for [`ProjectionCtx::derive_id`].
///
/// Constant on purpose. Changing it would change every derived id, which — for a
/// projection keyed on one — means a replay producing different rows than live.
const DERIVED_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x5b, 0x1e, 0x0a, 0x4c, 0x8f, 0x2d, 0x4e, 0x7a, 0x9c, 0x31, 0x6d, 0x88, 0xf4, 0x02, 0xa1, 0x77,
]);

/// Everything a projection is permitted to know.
///
/// # Law L2: projections are pure functions of the event stream
///
/// This type is the compile-time half of that. It holds no pool, no clock, no
/// random source, and no HTTP client — **there is nothing in scope to be impure
/// with**. A projector cannot call `Utc::now()` because it has no reason to
/// import chrono, and cannot generate a random id because it has no RNG.
///
/// What it does hold is deliberately all deterministic:
///
/// - [`position`](Self::position) and [`event_time`](Self::event_time) come from
///   the event, so they are identical on replay.
/// - [`derive_id`](Self::derive_id) is a hash, not a generator.
/// - The upcaster table is a set of function pointers with no state, so decoding
///   an event yields the same value every time. Holding it here does not weaken
///   L2; it is what lets a projection decode without reaching for a global.
///
/// The runtime half is `search_path` isolation (L3) and the shadow-replay differ,
/// which catches anything that slips through by comparing a rebuild against live.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionCtx<'a> {
    position: LogPosition,
    event_time: Timestamp,
    upcasters: &'a Upcasters,
}

impl<'a> ProjectionCtx<'a> {
    pub(crate) const fn new(
        position: LogPosition,
        event_time: Timestamp,
        upcasters: &'a Upcasters,
    ) -> Self {
        Self {
            position,
            event_time,
            upcasters,
        }
    }

    /// Where this event sits in the log. Stable across replays.
    #[must_use]
    pub const fn position(&self) -> LogPosition {
        self.position
    }

    /// When the event was recorded.
    ///
    /// **The only clock a projection may read.** `Utc::now()` inside a projector
    /// makes a replay produce different rows than the live run, and the
    /// difference is a timestamp column nobody thinks to check.
    #[must_use]
    pub const fn event_time(&self) -> Timestamp {
        self.event_time
    }

    /// A stable identifier derived from this event's position.
    ///
    /// For projections that need a surrogate key. `Uuid::new_v4()` would produce
    /// a different key on every replay, so every row would differ and the differ
    /// would report the whole table as changed — which is at least loud. Worse is
    /// a projection that *joins* on such a key: replay silently reassociates
    /// rows.
    ///
    /// `salt` distinguishes several ids derived from one event — a line index,
    /// say.
    #[must_use]
    pub fn derive_id(&self, salt: &str) -> Uuid {
        Uuid::new_v5(
            &DERIVED_ID_NAMESPACE,
            format!("{}:{salt}", self.position).as_bytes(),
        )
    }

    /// Decodes an event's payload, running it through the upcaster chain.
    ///
    /// The entry point for a projection that cares about this event. Returns
    /// `None`-shaped errors nowhere: an event a projection does not care about
    /// is filtered on `event_name` *before* decoding, not by a failed decode.
    pub fn decode<E: DeserializeOwned>(&self, envelope: &Envelope) -> Result<E, UpcastError> {
        self.upcasters.decode(
            &envelope.event_name,
            envelope.schema_version,
            envelope.payload.clone(),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("decoding {event_name} at position {position}: {source}")]
    Decode {
        event_name: String,
        position: LogPosition,
        #[source]
        source: UpcastError,
    },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    /// For a projection that hits something it cannot interpret. Stops the group
    /// rather than skipping the event (L6).
    #[error("{0}")]
    Rejected(String),
}

/// One read model, fed by the log.
///
/// # Contract
///
/// - **Pure.** Same events in, same tables out, every time. [`ProjectionCtx`]
///   removes most ways to break this; the shadow differ catches the rest.
/// - **Confined.** Writes only to its group's schema. `search_path` is already
///   set when `apply` is called, so unqualified names resolve there and nowhere
///   else.
/// - **Ordered.** Events arrive in position order and are applied one at a time
///   within the group's transaction.
///
/// Idempotency is *not* required. Law L4 commits effects and checkpoint
/// together, so an event is never applied twice — which means a projection may
/// safely `UPDATE … SET total = total + $1` without a dedup guard.
#[async_trait::async_trait]
pub trait Projection: Send + Sync {
    type Group: ProjectionGroup;

    /// Stable name, for logs and metrics.
    fn name(&self) -> &'static str;

    /// Applies one event.
    ///
    /// `conn` is inside the group's transaction with `search_path` already set.
    /// Do not commit, roll back, or open another transaction on it — the runner
    /// owns the boundary, and that is what makes L4 hold.
    async fn apply(
        &self,
        ctx: &ProjectionCtx<'_>,
        envelope: &Envelope,
        conn: &mut PgConnection,
    ) -> Result<(), ProjectionError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(position: i64, upcasters: &Upcasters) -> ProjectionCtx<'_> {
        ProjectionCtx::new(
            LogPosition::new(position).expect("valid"),
            chrono::DateTime::from_timestamp(0, 0).expect("valid"),
            upcasters,
        )
    }

    #[test]
    fn derived_ids_are_stable_for_a_position() {
        let upcasters = Upcasters::new();
        // The property replay depends on: the same position yields the same id,
        // however many times it is rebuilt.
        assert_eq!(
            ctx(1, &upcasters).derive_id("line-0"),
            ctx(1, &upcasters).derive_id("line-0")
        );
    }

    #[test]
    fn derived_ids_differ_by_position_and_by_salt() {
        let upcasters = Upcasters::new();
        assert_ne!(
            ctx(1, &upcasters).derive_id("line-0"),
            ctx(2, &upcasters).derive_id("line-0"),
            "two events must not collide"
        );
        assert_ne!(
            ctx(1, &upcasters).derive_id("line-0"),
            ctx(1, &upcasters).derive_id("line-1"),
            "two ids from one event must not collide"
        );
    }
}
