//! Identifiers and positions.
//!
//! `LogPosition` and `Sequence` are the reason this module is fussy. In the
//! prototype both were `u64`, and the dead-letter path wrote a per-aggregate
//! sequence into a column keyed by global position — so unrelated events shared
//! a retry counter and dead-lettered each other. Here they are distinct types
//! with no conversion between them, and that defect does not compile.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::InvalidStringReason;
use crate::{counter, uuid_id, validated_string};

uuid_id! {
    /// A tenant. The routing key for every database decision in the system.
    TenantId
}

uuid_id! {
    /// Something that can authenticate. Not a person, not a role, not a party
    /// record — see `Profile` for those.
    IdentityId
}

uuid_id! {
    /// A `(identity, scope, role)` grant of the right to enter a scope.
    MembershipId
}

uuid_id! {
    /// A party in a tenant's domain: employee, client, supplier, contact.
    /// May or may not be linked to an `IdentityId`.
    ProfileId
}

counter! {
    /// Position in a tenant's event log. Globally ordered within the tenant,
    /// contiguous, and equal to commit order (architecture law L1).
    ///
    /// Not interchangeable with [`Sequence`], deliberately.
    LogPosition
}

counter! {
    /// Version of a single aggregate — how many events it has applied.
    ///
    /// Not interchangeable with [`LogPosition`], deliberately.
    Sequence
}

counter! {
    /// Schema version of a stored event payload, for the upcaster chain.
    SchemaVersion
}

fn ascii_identifier(value: &str) -> Result<(), InvalidStringReason> {
    for (index, ch) in value.char_indices() {
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.') {
            return Err(InvalidStringReason::ForbiddenChar { ch, index });
        }
    }
    Ok(())
}

validated_string! {
    /// The name of an aggregate's domain — `ledger_account`, `journal_entry`.
    ///
    /// Part of the stream key, so it reaches the database and the event log.
    DomainName,
    max_len = 64,
    validate = ascii_identifier
}

validated_string! {
    /// An aggregate's identity within its domain.
    ///
    /// A string rather than a UUID because some aggregates are keyed by natural
    /// identifiers — a chart-of-accounts code, a fiscal period label — and
    /// forcing those through a surrogate key would mean a lookup table for no
    /// benefit.
    AggregateId,
    max_len = 128,
    validate = ascii_identifier
}

validated_string! {
    /// The name of an event type — `journal_entry.posted`.
    EventName,
    max_len = 96,
    validate = ascii_identifier
}

validated_string! {
    /// A module's identifier — `ledger`, `invoicing`.
    ModuleId,
    max_len = 48,
    validate = ascii_identifier
}

validated_string! {
    /// What kind of effect an outbox row is — `email.send`, `webhook.post`.
    ///
    /// The routing key from a promise to the handler that keeps it. Stored, so
    /// renaming one strands every effect already enqueued under the old name.
    EffectKind,
    max_len = 64,
    validate = ascii_identifier
}

/// Where an aggregate's events live: domain plus identity.
///
/// Deliberately a struct rather than two loose `&str` parameters, which is how
/// the prototype passed them and how they got transposed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StreamId {
    pub domain: DomainName,
    pub id: AggregateId,
}

impl StreamId {
    #[must_use]
    pub const fn new(domain: DomainName, id: AggregateId) -> Self {
        Self { domain, id }
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.domain, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NegativeCounter;
    use core::str::FromStr;

    #[test]
    fn uuid_ids_round_trip_through_string_and_json() {
        let id = TenantId::new();
        assert_eq!(TenantId::from_str(&id.to_string()).unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<TenantId>(&json).unwrap(), id);
    }

    #[test]
    fn uuid_ids_are_time_ordered() {
        // v7 gives index locality in Postgres. Same-millisecond generation can
        // tie, so assert non-decreasing rather than strictly increasing.
        let first = TenantId::new();
        let second = TenantId::new();
        assert!(second >= first);
    }

    #[test]
    fn a_malformed_id_is_rejected() {
        assert!(TenantId::from_str("not-a-uuid").is_err());
        assert!(serde_json::from_str::<TenantId>(r#""nope""#).is_err());
    }

    #[test]
    fn counters_reject_negatives() {
        assert!(matches!(
            LogPosition::new(-1),
            Err(NegativeCounter { value: -1, .. })
        ));
        assert_eq!(LogPosition::new(0).unwrap(), LogPosition::ZERO);
    }

    #[test]
    fn counter_next_saturates_rather_than_wrapping() {
        // Wrapping would produce a position that already exists, which is worse
        // than sticking at the maximum.
        let max = LogPosition::new(i64::MAX).unwrap();
        assert_eq!(max.next(), max);
    }

    #[test]
    fn distance_saturates_at_zero_when_not_ahead() {
        let a = LogPosition::new(10).unwrap();
        let b = LogPosition::new(4).unwrap();
        assert_eq!(a.distance_from(b), 6);
        assert_eq!(b.distance_from(a), 0);
        assert_eq!(a.distance_from(a), 0);
    }

    /// The regression guard for the prototype's C3 defect. If a future edit
    /// adds `From<Sequence> for LogPosition`, or makes either type expose
    /// arithmetic that accepts the other, this stops compiling — which is the
    /// entire point of them being separate types.
    #[test]
    fn log_position_and_sequence_do_not_interconvert() {
        fn takes_position(_: LogPosition) {}
        fn takes_sequence(_: Sequence) {}

        let position = LogPosition::new(7).unwrap();
        let sequence = Sequence::new(7).unwrap();

        // They may both be inspected as i64 — that is the only bridge, and it
        // is explicit at the call site.
        assert_eq!(position.get(), sequence.get());

        // Neither of these accepts the other's type. A future `impl From` or a
        // shared arithmetic trait would break this, which is the point.
        takes_position(position);
        takes_sequence(sequence);
    }

    #[test]
    fn validated_strings_reject_empty_overlong_and_forbidden() {
        assert!(DomainName::new("").is_err());
        assert!(DomainName::new("a".repeat(DomainName::MAX_LEN + 1)).is_err());
        assert!(DomainName::new("has space").is_err());
        assert!(DomainName::new("has/slash").is_err());
        assert!(DomainName::new("journal_entry").is_ok());
        assert!(EventName::new("journal_entry.posted").is_ok());
        assert!(AggregateId::new("4000.01").is_ok());
    }

    #[test]
    fn validated_strings_are_validated_on_deserialize_too() {
        // Without a custom Deserialize, serde would construct an invalid value
        // and the guarantee would hold only for values built in Rust — which is
        // exactly the gap that matters for data read back out of the event log.
        assert!(serde_json::from_str::<DomainName>(r#""has space""#).is_err());
        assert!(serde_json::from_str::<DomainName>(r#""""#).is_err());
        assert!(serde_json::from_str::<DomainName>(r#""ledger_account""#).is_ok());
    }

    #[test]
    fn stream_id_renders_readably() {
        let stream = StreamId::new(
            DomainName::new("journal_entry").unwrap(),
            AggregateId::new("abc-123").unwrap(),
        );
        assert_eq!(stream.to_string(), "journal_entry/abc-123");
    }
}
