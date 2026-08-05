use crate::event_sourcing::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "identity")]
pub enum IdentityEvent {
    Created { is_system: bool },
    Suspended { reason: String },
    Reinstated,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "identity")]
pub struct Identity {
    id: String,
    version: u64,
    is_system: bool,
    suspended: bool,
}

impl Identity {
    pub fn is_system(&self) -> bool {
        self.is_system
    }

    pub fn is_active(&self) -> bool {
        self.version > 0 && !self.suspended
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity already exists")]
    AlreadyExists,
    #[error("identity does not exist")]
    NotFound,
    #[error("identity already in requested state")]
    NoChange,
}

#[derive(Debug, Clone)]
pub enum IdentityCommand {
    Create { is_system: bool },
    Suspend { reason: String },
    Reinstate,
}

impl Aggregate for Identity {
    type Event = IdentityEvent;
    type Command = IdentityCommand;
    type Error = IdentityError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            IdentityEvent::Created { is_system } => self.is_system = *is_system,
            IdentityEvent::Suspended { .. } => self.suspended = true,
            IdentityEvent::Reinstated => self.suspended = false,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            IdentityCommand::Create { is_system } => {
                if self.version != 0 {
                    return Err(IdentityError::AlreadyExists);
                }
                Ok(vec![IdentityEvent::Created { is_system }])
            }
            IdentityCommand::Suspend { reason } => {
                if self.version == 0 {
                    return Err(IdentityError::NotFound);
                }
                if self.suspended {
                    return Err(IdentityError::NoChange);
                }
                Ok(vec![IdentityEvent::Suspended { reason }])
            }
            IdentityCommand::Reinstate => {
                if self.version == 0 {
                    return Err(IdentityError::NotFound);
                }
                if !self.suspended {
                    return Err(IdentityError::NoChange);
                }
                Ok(vec![IdentityEvent::Reinstated])
            }
        }
    }
}
