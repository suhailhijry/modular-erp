use serde::{Deserialize, Serialize};

use crate::event_sourcing::*;

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "account")]
pub enum AccountEvent {
    Opened { id: String, owner: String },
    Deposited { amount: u64 },
    Withdrawn { amount: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "account")]
pub struct Account {
    #[aggregate_id]
    pub id: String,
    pub owner: String,
    pub balance: u64,
    #[version]
    version: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("account is already open")]
    AlreadyOpen,
    #[error("insufficient funds: have {balance}, need {requested}")]
    InsufficientFunds { balance: u64, requested: u64 },
}

impl Aggregate for Account {
    type Event = AccountEvent;
    type Command = AccountCommand;
    type Error = AccountError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AccountEvent::Opened { id, owner } => {
                self.id = id.clone();
                self.owner = owner.clone();
            }
            AccountEvent::Deposited { amount } => self.balance += amount,
            AccountEvent::Withdrawn { amount } => self.balance -= amount,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            AccountCommand::Open { id, owner } => {
                if self.version != 0 {
                    return Err(AccountError::AlreadyOpen);
                }
                Ok(vec![AccountEvent::Opened { id, owner }])
            }
            AccountCommand::Deposit { amount } => Ok(vec![AccountEvent::Deposited { amount }]),
            AccountCommand::Withdraw { amount } => {
                if amount > self.balance {
                    return Err(AccountError::InsufficientFunds {
                        balance: self.balance,
                        requested: amount,
                    });
                }
                Ok(vec![AccountEvent::Withdrawn { amount }])
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum AccountCommand {
    Open { id: String, owner: String },
    Deposit { amount: u64 },
    Withdraw { amount: u64 },
}
