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

/// # What this aggregate is for
///
/// A posted entry never changes, so most of this is about answering two
/// questions a command needs: "has this id been used?", which makes posting the
/// same entry twice a no-op rather than a duplicate, and "what did it say?",
/// which is what reversing one needs in order to write its opposite.
///
/// The lines are kept because [`reverse_entry`](crate::reverse_entry) negates
/// them. Reading them back out of the log by hand would work and would be a
/// second place that knows how a `Posted` event is shaped.
///
/// There is still no draft state, because nobody can save a draft.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub posted: bool,
    /// The entry that reversed this one, if any. The id rather than a flag:
    /// "already reversed" and "already reversed **by this**" want different
    /// answers, and only the second is a safe retry.
    pub reversed_by: Option<String>,
    /// What was posted, for whatever needs to undo it.
    pub lines: Option<BalancedLines>,
    pub occurred_on: Option<Timestamp>,
}

impl Aggregate for JournalEntry {
    type Event = JournalEntryEvent;

    fn domain() -> DomainName {
        crate::domain("ledger_entry")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            JournalEntryEvent::Posted {
                lines, occurred_on, ..
            } => {
                self.posted = true;
                self.lines = Some(lines.clone());
                self.occurred_on = Some(*occurred_on);
            }
            JournalEntryEvent::Reversed { by, .. } => self.reversed_by = Some(by.clone()),
        }
    }
}

impl JournalEntry {
    /// Whether this entry has been undone.
    #[must_use]
    pub const fn is_reversed(&self) -> bool {
        self.reversed_by.is_some()
    }
}
