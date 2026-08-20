//! The chart of accounts.

use serde::{Deserialize, Serialize};
use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{CurrencyCode, DomainName, EventName, SchemaVersion};

/// What an account is for.
///
/// Fixed, not configurable: the five are what double-entry accounting *is*, and
/// every statement format in every jurisdiction is built from them. Tenant
/// vocabulary ("Cash at bank") is the account's name; this is its behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

impl AccountKind {
    /// Whether a positive (debit) balance is the account's normal state.
    ///
    /// Assets and expenses grow by debit; the rest grow by credit. Used only for
    /// presentation — nothing here refuses a balance on the "wrong" side,
    /// because a contra account and an overdrawn bank account are both real.
    #[must_use]
    pub const fn is_debit_normal(self) -> bool {
        matches!(self, Self::Asset | Self::Expense)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Asset => "asset",
            Self::Liability => "liability",
            Self::Equity => "equity",
            Self::Revenue => "revenue",
            Self::Expense => "expense",
        }
    }
}

impl std::str::FromStr for AccountKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "asset" => Ok(Self::Asset),
            "liability" => Ok(Self::Liability),
            "equity" => Ok(Self::Equity),
            "revenue" => Ok(Self::Revenue),
            "expense" => Ok(Self::Expense),
            other => Err(format!("unknown account kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AccountEvent {
    Opened {
        name: String,
        kind: AccountKind,
        /// An account holds one currency. A business with two currencies has two
        /// cash accounts, which is how every accounting system works and what
        /// makes the trial balance checkable per currency.
        currency: CurrencyCode,
    },
    Renamed {
        name: String,
    },
    /// Closed accounts keep their history and refuse new postings.
    Closed,
    Reopened,
}

impl AccountEvent {
    /// Every name this event type can carry. The upcaster registry declares
    /// exactly these, and `names_are_valid` checks they parse.
    pub const NAMES: [&'static str; 4] = [
        "ledger.account.opened",
        "ledger.account.renamed",
        "ledger.account.closed",
        "ledger.account.reopened",
    ];
}

impl DomainEvent for AccountEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Opened { .. } => Self::NAMES[0],
            Self::Renamed { .. } => Self::NAMES[1],
            Self::Closed => Self::NAMES[2],
            Self::Reopened => Self::NAMES[3],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Account {
    pub exists: bool,
    pub name: String,
    pub kind: Option<AccountKind>,
    pub currency: Option<CurrencyCode>,
    pub closed: bool,
}

impl Aggregate for Account {
    type Event = AccountEvent;

    fn domain() -> DomainName {
        crate::domain("ledger_account")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::Opened {
                name,
                kind,
                currency,
            } => {
                self.exists = true;
                self.name.clone_from(name);
                self.kind = Some(*kind);
                self.currency = Some(*currency);
                self.closed = false;
            }
            AccountEvent::Renamed { name } => self.name.clone_from(name),
            AccountEvent::Closed => self.closed = true,
            AccountEvent::Reopened => self.closed = false,
        }
    }
}

impl Account {
    /// Whether this account can take a posting right now.
    #[must_use]
    pub const fn accepts_postings(&self) -> bool {
        self.exists && !self.closed
    }
}
