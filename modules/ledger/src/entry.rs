//! Journal entries.

use serde::{Deserialize, Serialize};
use spa_eventlog::{Aggregate, DomainEvent};
use spa_types::{DomainName, EventName, SchemaVersion, Timestamp};

use crate::lines::BalancedLines;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEntryEvent {
    Posted {
        /// The accounting date, which is **not** when it was recorded. A March
        /// invoice entered in April belongs to March.
        occurred_on: Timestamp,
        memo: String,
        /// Carries its own proof — see [`BalancedLines`]. An unbalanced entry
        /// cannot be constructed here and cannot be decoded from storage.
        lines: BalancedLines,
    },
    /// Accounting does not delete. Reversing writes the opposite entry; this
    /// records that it happened and which entry did it.
    Reversed { by: String, occurred_on: Timestamp },
}

impl JournalEntryEvent {
    pub const NAMES: [&'static str; 2] = ["ledger.entry.posted", "ledger.entry.reversed"];
}

impl DomainEvent for JournalEntryEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Posted { .. } => Self::NAMES[0],
            Self::Reversed { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// # Why this aggregate is nearly empty
///
/// A posted entry never changes. The state exists only to answer "has this id
/// been used?", which is what makes posting the same entry twice a no-op rather
/// than a duplicate — and that is the whole job. Adding a draft state before
/// anyone can save a draft would be inventing a workflow.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub posted: bool,
    pub reversed: bool,
}

impl Aggregate for JournalEntry {
    type Event = JournalEntryEvent;

    fn domain() -> DomainName {
        crate::domain("ledger_entry")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            JournalEntryEvent::Posted { .. } => self.posted = true,
            JournalEntryEvent::Reversed { .. } => self.reversed = true,
        }
    }
}
