use crate::event_sourcing::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MachinePrincipal {
    FirstParty { app: String },
    ThirdParty { partner: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "api_key")]
pub enum ApiKeyEvent {
    Issued {
        principal: MachinePrincipal,
        secret_hash: String,
        scopes: Vec<String>,
    },
    Revoked {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "api_key")]
pub struct ApiKey {
    id: String, // the public prefix, e.g. "ak_live_x7f3"
    version: u64,
    principal: Option<MachinePrincipal>,
    secret_hash: String,
    scopes: Vec<String>,
    revoked: bool,
}

impl ApiKey {
    /// SHA-256, NOT Argon2, deliberately: the secret is 256 bits of our
    /// own CSPRNG output (see crypto.rs rationale) and this runs on
    /// every request. Constant-time digest comparison.
    pub fn verify(&self, presented_secret: &str) -> bool {
        !self.revoked
            && self.version > 0
            && super::crypto::constant_time_eq(
                &super::crypto::sha256_hex(presented_secret),
                &self.secret_hash,
            )
    }
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
    pub fn principal(&self) -> Option<&MachinePrincipal> {
        self.principal.as_ref()
    }
}

#[derive(Debug, Clone)]
pub enum ApiKeyCommand {
    /// secret_sha256 comes from crypto::generate_api_key at the
    /// endpoint - the plain secret is returned to the caller ONCE and
    /// never enters an event.
    Issue {
        principal: MachinePrincipal,
        secret_sha256: String,
        scopes: Vec<String>,
    },
    Revoke {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    #[error("api key already issued under this id")]
    AlreadyIssued,
    #[error("api key does not exist")]
    NotFound,
    #[error("api key already revoked")]
    AlreadyRevoked,
}

impl Aggregate for ApiKey {
    type Event = ApiKeyEvent;
    type Command = ApiKeyCommand;
    type Error = ApiKeyError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ApiKeyEvent::Issued {
                principal,
                secret_hash,
                scopes,
            } => {
                self.principal = Some(principal.clone());
                self.secret_hash = secret_hash.clone();
                self.scopes = scopes.clone();
            }
            ApiKeyEvent::Revoked { .. } => self.revoked = true,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            ApiKeyCommand::Issue {
                principal,
                secret_sha256,
                scopes,
            } => {
                if self.version != 0 {
                    return Err(ApiKeyError::AlreadyIssued);
                }
                Ok(vec![ApiKeyEvent::Issued {
                    principal,
                    secret_hash: secret_sha256,
                    scopes,
                }])
            }
            ApiKeyCommand::Revoke { reason } => {
                if self.version == 0 {
                    return Err(ApiKeyError::NotFound);
                }
                if self.revoked {
                    return Err(ApiKeyError::AlreadyRevoked);
                }
                Ok(vec![ApiKeyEvent::Revoked { reason }])
            }
        }
    }
}
