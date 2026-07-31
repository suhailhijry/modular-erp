use serde::{Deserialize, Serialize};

use crate::event_sourcing::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "account_type", rename_all = "lowercase")]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "accounting_side", rename_all = "lowercase")]
pub enum BalanceSide {
    Debit,
    Credit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statement {
    BalanceSheet,
    IncomeStatement,
}

impl AccountType {
    pub fn default_normal(self) -> BalanceSide {
        match self {
            AccountType::Asset | AccountType::Expense => BalanceSide::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                BalanceSide::Credit
            }
        }
    }

    pub fn statement(self) -> Statement {
        match self {
            AccountType::Asset | AccountType::Liability | AccountType::Equity => {
                Statement::BalanceSheet
            }
            AccountType::Revenue | AccountType::Expense => Statement::IncomeStatement,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[serde(tag = "type", rename_all = "snake_case")]
#[event(prefix = "account")]
pub enum LedgerAccountEvent {
    Created {
        code: String,
        name: String,
        account_type: AccountType,
        normal: BalanceSide,
        parent_code: Option<String>,
    },
    Renamed {
        new_name: String,
    },
    Deactivated,
    Reactivated,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "ledger_account")]
pub struct LedgerAccount {
    id: String, // the account "id" IS its chart-of-accounts code, e.g. "1000", "4000.01"
    version: u64,

    name: String,
    account_type: Option<AccountType>, // None until Created applies
    normal: Option<BalanceSide>,       // None until Created applies
    parent_code: Option<String>,
    active: bool,
}

impl LedgerAccount {
    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn parent_code(&self) -> Option<String> {
        self.parent_code.clone()
    }

    pub fn account_type(&self) -> AccountType {
        self.account_type.expect("account_type set on creation")
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn normal(&self) -> BalanceSide {
        self.normal.expect("account normal set on creation")
    }

    pub fn is_contra(&self) -> bool {
        self.normal() != self.account_type().default_normal()
    }
}

#[derive(Debug, Clone)]
pub enum LedgerAccountCommand {
    Create {
        code: String,
        name: String,
        account_type: AccountType,
        normal: BalanceSide,
        parent_code: Option<String>,
    },
    Rename {
        new_name: String,
    },
    Deactivate,
    Reactivate,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerAccountError {
    #[error("account {0} already exists")]
    AlreadyExists(String),
    #[error("account does not exist")]
    NotYetCreated,
    #[error("account is inactive")]
    Inactive,
    #[error("account is already active")]
    AlreadyActive,
}

impl Aggregate for LedgerAccount {
    type Event = LedgerAccountEvent;
    type Command = LedgerAccountCommand;
    type Error = LedgerAccountError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            LedgerAccountEvent::Created {
                code,
                name,
                account_type,
                normal,
                parent_code,
            } => {
                self.id = code.clone();
                self.name = name.clone();
                self.account_type = Some(*account_type);
                self.normal = Some(*normal);
                self.parent_code = parent_code.clone();
                self.active = true;
            }
            LedgerAccountEvent::Renamed { new_name } => self.name = new_name.clone(),
            LedgerAccountEvent::Deactivated => self.active = false,
            LedgerAccountEvent::Reactivated => self.active = true,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            LedgerAccountCommand::Create {
                code,
                name,
                account_type,
                normal,
                parent_code,
            } => {
                if self.version != 0 {
                    return Err(LedgerAccountError::AlreadyExists(code));
                }
                Ok(vec![LedgerAccountEvent::Created {
                    code,
                    name,
                    account_type,
                    normal,
                    parent_code,
                }])
            }
            LedgerAccountCommand::Rename { new_name } => {
                if self.account_type.is_none() {
                    return Err(LedgerAccountError::NotYetCreated);
                }
                Ok(vec![LedgerAccountEvent::Renamed { new_name }])
            }
            LedgerAccountCommand::Deactivate => {
                if !self.active {
                    return Err(LedgerAccountError::Inactive);
                }
                Ok(vec![LedgerAccountEvent::Deactivated])
            }
            LedgerAccountCommand::Reactivate => {
                if self.active {
                    return Err(LedgerAccountError::AlreadyActive);
                }
                Ok(vec![LedgerAccountEvent::Reactivated])
            }
        }
    }
}
