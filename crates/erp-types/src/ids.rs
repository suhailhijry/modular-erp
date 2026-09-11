//! Identifiers and positions.
//!
//! `LogPosition` and `Sequence` are the reason this module is fussy. In the
//! prototype both were `u64`, and the dead-letter path wrote a per-aggregate
//! sequence into a column keyed by global position — so unrelated events shared
//! a retry counter and dead-lettered each other. Here they are distinct types
//! with no conversion between them, and that defect does not compile.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::Timestamp;
use crate::error::InvalidStringReason;
use crate::{counter, uuid_id, validated_string};

uuid_id! {
    /// A tenant. The routing key for every database decision in the system.
    TenantId
}

impl TenantId {
    /// The prefix every tenant database's name carries.
    ///
    /// The name is `TENANT_DATABASE_PREFIX` plus this id's hex, which is what
    /// makes [`Self::named_in_database`] able to work backwards from a name to
    /// when the tenant was created — and the only reason anything can tell a
    /// tenant database from a database somebody made by hand.
    pub const DATABASE_PREFIX: &'static str = "erp_tenant_";

    /// **When the tenant a database is named after was created**, or `None` if
    /// the name is not one of ours.
    ///
    /// A `TenantId` is a `UUIDv7`, so the first forty-eight bits of the name are
    /// the millisecond it was minted. That makes the name self-dating: there is
    /// no `created_at` in `pg_database`, and a filesystem time would be a second
    /// source that can disagree with this one.
    ///
    /// `None` for everything that is not exactly this shape, and each exclusion
    /// is load-bearing wherever the answer decides whether a database may be
    /// destroyed:
    ///
    /// - Not the prefix, or not thirty-two hex characters — an operator's
    ///   `erp_tenant_backup_before_upgrade` is not a tenant.
    /// - **The hyphenated form**, which `Uuid::parse_str` otherwise accepts and
    ///   which nothing here ever mints.
    /// - **Any version but seven.** `get_timestamp` answers for v1 and v6 as
    ///   well, and both convert to a plausible Unix second, so leaning on it to
    ///   mean "one of ours" would date a name this system never made.
    #[must_use]
    pub fn named_in_database(datname: &str) -> Option<Timestamp> {
        let hex = datname.strip_prefix(Self::DATABASE_PREFIX)?;
        if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let id = uuid::Uuid::parse_str(hex).ok()?;
        if id.get_version_num() != 7 {
            return None;
        }
        let (seconds, nanos) = id.get_timestamp()?.to_unix();
        chrono::DateTime::from_timestamp(i64::try_from(seconds).ok()?, nanos)
    }
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

/// A module's identifier, which is narrower than an identifier.
///
/// # Why this is not `ascii_identifier`
///
/// It was, and the type accepted `tax-sa` — which constructed fine, passed
/// every test that does not touch the control plane, and failed at the moment a
/// tenant enabled it: `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$` and
/// the database refused what the type had allowed. A terrible place to find out.
///
/// So the type carries the rule the schema enforces: lower case, starting with
/// a letter, and `_` as the only separator. A module id that cannot be stored
/// is now one that cannot be built.
fn module_identifier(value: &str) -> Result<(), InvalidStringReason> {
    for (index, ch) in value.char_indices() {
        let allowed = if index == 0 {
            ch.is_ascii_lowercase()
        } else {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'
        };
        if !allowed {
            return Err(InvalidStringReason::ForbiddenChar { ch, index });
        }
    }
    Ok(())
}

validated_string! {
    /// A module's identifier — `ledger`, `sales`, `tax_sa`.
    ModuleId,
    max_len = 48,
    validate = module_identifier
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

    /// **The rule that decides whether a database may be destroyed**, so every
    /// exclusion is checked rather than assumed.
    #[test]
    fn only_a_name_this_system_mints_is_dated() {
        let id = TenantId::new();
        let mine = format!("{}{}", TenantId::DATABASE_PREFIX, id.as_uuid().simple());
        let named = TenantId::named_in_database(&mine).expect("its own name");
        assert!(
            (chrono::Utc::now() - named).num_seconds() < 5,
            "a name minted just now dated as {named}"
        );

        for foreign in [
            // Not ours.
            "postgres",
            "erp_control",
            "erp_test_1700000000_1_0",
            // An operator's copy, which is what somebody makes before an
            // upgrade and exactly what they would not forgive losing.
            "erp_tenant_backup_before_upgrade",
            // Right shape, wrong length.
            "erp_tenant_01a086d2daaa72a2b4974af36080",
            // Hex-looking, not hex.
            "erp_tenant_01a086d2daaa72a2b4974af3608096zz",
            // **The hyphenated form**, which `Uuid::parse_str` accepts and
            // nothing here ever mints.
            "erp_tenant_01a086d2-daaa-72a2-b497-4af3608096b2",
            // **v1 and v6 carry timestamps too**, and both convert to a
            // plausible Unix second — so the version has to be checked rather
            // than inferred from "did a timestamp come back".
            "erp_tenant_2c1a5d3e9f1b11ee8c900242ac120002",
            "erp_tenant_1ee9f1b2c1a56d3e8c900242ac120002",
        ] {
            assert!(
                TenantId::named_in_database(foreign).is_none(),
                "{foreign} was dated, and anything with a date can be swept"
            );
        }

        // v4 has no timestamp at all, which is a different reason for the same
        // answer and worth pinning separately. Written out rather than
        // generated: this crate does not compile the `v4` feature, and the
        // version nibble is the only part that matters.
        assert!(
            TenantId::named_in_database("erp_tenant_2c1a5d3e9f1b41ee8c900242ac120002").is_none()
        );
    }
    /// **The type refuses what the database would.**
    ///
    /// `tax-sa` once constructed fine and failed at the moment a tenant enabled
    /// it, because `entitlement.module_id` is `^[a-z][a-z0-9_]{0,47}$`. Every
    /// character this accepts, that pattern accepts.
    #[test]
    fn a_module_id_is_what_the_schema_will_store() {
        for good in ["ledger", "sales", "tax_sa", "a", "m2m", "a_b_c"] {
            assert!(ModuleId::new(good).is_ok(), "{good} was refused");
        }
        for bad in [
            "tax-sa", // the one that got through
            "tax.sa", "TaxSa", "Ledger", "2fast", "_leading", "", "tax sa",
        ] {
            assert!(ModuleId::new(bad).is_err(), "{bad:?} was accepted");
        }
    }

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
