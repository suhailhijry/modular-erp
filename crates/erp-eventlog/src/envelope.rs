//! What a stored event looks like on the way out.

use erp_types::{EventName, LogPosition, SchemaVersion, Sequence, StreamId, Timestamp};
use serde::{Deserialize, Serialize};

/// Context recorded alongside an event: who caused it, and under what
/// configuration.
///
/// The `config_version` and `rule_version` fields are what make architecture law
/// L5 checkable. An event records the *outcome* a command decided, and names the
/// configuration that decided it — so configuration stays freely editable while
/// replay stays reproducible, and "why was this 10% and not 15%" remains
/// answerable a year later.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// The identity whose action produced this event, if any. `None` for events
    /// produced by workers, reapers, and provisioning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Platform staff acting on a tenant's behalf. Both parties are recorded,
    /// so an impersonated action is never indistinguishable from a tenant's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
    /// Ties every event produced by one request together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// The configuration snapshot this command resolved against (L5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<i64>,
    /// Anything a module wants to record without earning a field.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Metadata {
    /// Records which request produced this event.
    ///
    /// Read back by `try_create` to tell a retry from a different request that
    /// reused an identifier. It goes in `extra` rather than earning a field
    /// because only creates consult it, and an event that was not created by
    /// one should not carry an empty column saying so.
    #[must_use]
    pub fn with_fingerprint(mut self, fingerprint: &str) -> Self {
        self.extra.insert(
            crate::REQUEST_FINGERPRINT.to_owned(),
            serde_json::Value::String(fingerprint.to_owned()),
        );
        self
    }

    /// Records which branch this happened at.
    ///
    /// **The reporting dimension, and it travels in metadata on purpose.** A
    /// branch is a fact about *where the request came from*, uniform across
    /// every event one request produces — an invoice, its journal entry and the
    /// shift that rang it are all at the same counter. Putting it here rather
    /// than in each event's payload is what makes that true by construction
    /// instead of by four modules remembering to thread a field through.
    #[must_use]
    pub fn at_branch(mut self, branch: &str) -> Self {
        self.extra.insert(
            crate::BRANCH.to_owned(),
            serde_json::Value::String(branch.to_owned()),
        );
        self
    }

    /// Where this happened, if the request said.
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        self.extra
            .get(crate::BRANCH)
            .and_then(serde_json::Value::as_str)
    }

    /// Which request produced this, if it said.
    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        self.extra
            .get(crate::REQUEST_FINGERPRINT)
            .and_then(serde_json::Value::as_str)
    }
}

/// A stored event, as read back from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Gapless, commit-ordered position in this tenant's log.
    pub position: LogPosition,
    pub stream: StreamId,
    /// The aggregate's version after this event. Not interchangeable with
    /// [`Envelope::position`] — see `erp-types`.
    pub sequence: Sequence,
    pub event_name: EventName,
    /// Which shape `payload` is in. Drives the upcaster chain on read.
    pub schema_version: SchemaVersion,
    pub payload: serde_json::Value,
    pub metadata: Metadata,
    /// When the append committed. **The only clock a projection may read** —
    /// see architecture law L2. Using `now()` inside a projector makes replay
    /// non-reproducible.
    pub recorded_at: Timestamp,
}
