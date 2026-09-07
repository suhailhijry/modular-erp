//! **One live deposit per thing a deposit is held against.**
//!
//! # Why this exists
//!
//! A deposit's payment id is chosen by the caller — the public deposit route
//! makes the browser's `Idempotency-Key` the charge's id, so a reload pays the
//! same charge. That is right for a reload and wrong for a second tab: a fresh
//! key is a fresh charge against the same booking, and a customer who pays both
//! has paid twice. The first version guarded this by reading the payments
//! projection for an existing charge, which is a check against a read model
//! that has not necessarily seen the charge made a moment ago — the exact
//! window two tabs stand in.
//!
//! So the fact "this booking has a deposit in flight" lives in the log, keyed
//! on the booking, and [`crate::request_in`] refuses a second charge while one
//! is live. It is a `payments` aggregate keyed on an id `payments` does not
//! interpret — the same opacity `Advance::against` already has — so `payments`
//! still names nothing in `booking`.
//!
//! # What "live" means
//!
//! Claimed and not released. A payment releases its claim when it **fails** or
//! is **voided**: the customer's card was refused, or the charge was cancelled
//! before it settled, and they may try again. A settled, refunded or retained
//! deposit keeps the claim: money arrived against this booking once, and a
//! second deposit for the same booking is not something the public surface
//! should be able to start — whatever happens to the first is a conversation
//! with the business, not another charge.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What happened to the deposits held against one thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AwaitingEvent {
    /// A deposit payment was requested against this.
    Claimed { payment: AggregateId, at: Timestamp },
    /// That payment failed or was voided, so another may be requested.
    Released { payment: AggregateId, at: Timestamp },
}

impl AwaitingEvent {
    pub const NAMES: [&'static str; 2] =
        ["payments.awaiting.claimed", "payments.awaiting.released"];
}

impl DomainEvent for AwaitingEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Claimed { .. } => Self::NAMES[0],
            Self::Released { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// The deposit currently in flight against one thing, if any.
///
/// **Keyed on what the deposit is against**, not on the payment: the question
/// is "does this booking already have a charge", and it has to be one load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Awaiting {
    /// The payment that holds the claim. `None` between a release and the next
    /// request, and before any request at all.
    pub live: Option<AggregateId>,
}

impl Awaiting {
    /// The payment that stands in the way of `payment`, if one does.
    ///
    /// The same payment asking again is a retry and stands in nobody's way.
    #[must_use]
    pub fn blocks(&self, payment: &AggregateId) -> Option<&AggregateId> {
        self.live.as_ref().filter(|held| *held != payment)
    }
}

impl Aggregate for Awaiting {
    type Event = AwaitingEvent;

    fn domain() -> DomainName {
        crate::domain("payments_awaiting")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AwaitingEvent::Claimed { payment, .. } => self.live = Some(payment.clone()),
            AwaitingEvent::Released { payment, .. } => {
                if self.live.as_ref() == Some(payment) {
                    self.live = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AggregateId {
        AggregateId::new(value).unwrap_or_else(|_| unreachable!())
    }

    fn now() -> Timestamp {
        Timestamp::from(chrono::Utc::now())
    }

    fn replay(events: &[AwaitingEvent]) -> Awaiting {
        let mut state = Awaiting::default();
        for event in events {
            Aggregate::apply(&mut state, event);
        }
        state
    }

    #[test]
    fn a_claim_blocks_every_other_payment_and_not_its_own_retry() {
        let held = replay(&[AwaitingEvent::Claimed {
            payment: id("pay-1"),
            at: now(),
        }]);
        assert_eq!(held.blocks(&id("pay-2")), Some(&id("pay-1")));
        assert_eq!(
            held.blocks(&id("pay-1")),
            None,
            "a retry is not a second deposit"
        );
    }

    #[test]
    fn a_release_frees_the_claim_and_a_stale_release_does_not() {
        let freed = replay(&[
            AwaitingEvent::Claimed {
                payment: id("pay-1"),
                at: now(),
            },
            AwaitingEvent::Released {
                payment: id("pay-1"),
                at: now(),
            },
        ]);
        assert_eq!(freed.live, None);

        // A release for a payment that no longer holds the claim must not
        // free the one that does — otherwise a failed first attempt's late
        // release would open the door beside a live second one.
        let still_held = replay(&[
            AwaitingEvent::Claimed {
                payment: id("pay-1"),
                at: now(),
            },
            AwaitingEvent::Released {
                payment: id("pay-1"),
                at: now(),
            },
            AwaitingEvent::Claimed {
                payment: id("pay-2"),
                at: now(),
            },
            AwaitingEvent::Released {
                payment: id("pay-1"),
                at: now(),
            },
        ]);
        assert_eq!(still_held.live, Some(id("pay-2")));
    }

    #[test]
    fn every_event_has_a_distinct_name() {
        let events = [
            AwaitingEvent::Claimed {
                payment: id("a"),
                at: now(),
            },
            AwaitingEvent::Released {
                payment: id("a"),
                at: now(),
            },
        ];
        let names: Vec<_> = events.iter().map(|e| e.event_name().to_string()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len());
        assert_eq!(names.len(), AwaitingEvent::NAMES.len());
    }
}
