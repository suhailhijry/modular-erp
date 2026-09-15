//! Cost centers: the dimension a profit and loss is cut by.
//!
//! # What one is
//!
//! A department, a project, a shop — whatever a business wants to know the
//! result of. Decided 2026-09-15: a small aggregate like an account (opened,
//! renamed, closed; a closed one refuses new lines), carried **per line** of a
//! journal entry, so one entry can charge two departments. A line that names
//! none takes the entry's branch, which is why **every open branch is a cost
//! center without being opened**: the two share an id namespace, and a posting
//! checks a cost center it does not know against the branches log the way it
//! checks a branch.
//!
//! # What one is not
//!
//! A book. The balance sheet is company-wide only — a transfer between two
//! departments debits one and credits the other, and neither balances on its
//! own — and nothing here requires a cost center on a line, because a tenant
//! that opened one for the head office should not find every till sale refused.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CostCenterEvent {
    Opened {
        name: String,
    },
    Renamed {
        name: String,
    },
    /// Keeps its history and refuses new lines.
    Closed,
}

impl CostCenterEvent {
    /// Every name this event type can carry; the upcaster registry declares
    /// exactly these.
    pub const NAMES: [&'static str; 3] = [
        "ledger.cost_center.opened",
        "ledger.cost_center.renamed",
        "ledger.cost_center.closed",
    ];
}

impl DomainEvent for CostCenterEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Opened { .. } => Self::NAMES[0],
            Self::Renamed { .. } => Self::NAMES[1],
            Self::Closed => Self::NAMES[2],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CostCenter {
    pub exists: bool,
    pub name: String,
    pub closed: bool,
}

impl Aggregate for CostCenter {
    type Event = CostCenterEvent;

    fn domain() -> DomainName {
        crate::domain("ledger_cost_center")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            CostCenterEvent::Opened { name } => {
                self.exists = true;
                self.name.clone_from(name);
            }
            CostCenterEvent::Renamed { name } => self.name.clone_from(name),
            CostCenterEvent::Closed => self.closed = true,
        }
    }
}

impl CostCenter {
    /// Whether a line may name this cost center right now.
    #[must_use]
    pub const fn accepts_lines(&self) -> bool {
        self.exists && !self.closed
    }
}
