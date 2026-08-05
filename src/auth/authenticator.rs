use crate::{auth::audience::Audience, event_sourcing::*};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Phone,
    Email,
    Username,
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "authenticator")]
pub enum AuthenticatorEvent {
    Registered {
        method: AuthMethod,
        identity_id: String,
        secret_hash: Option<String>,
    },
    Verified,
    /// The audit-trail answer to "this phone number changed hands".
    MovedToIdentity {
        from_identity_id: String,
        to_identity_id: String,
        reason: String,
    },
    SecretChanged {
        secret_hash: String,
    },
    Disabled {
        reason: String,
    },
    Reenabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "authenticator")]
pub struct Authenticator {
    id: String,
    version: u64,
    method: Option<AuthMethod>,
    identity_id: String,
    /// Argon2 hash; only Username-method carries one. Phone/email prove
    /// by OTP challenge at login, so there is nothing to store or leak.
    secret_hash: Option<String>,
    verified: bool,
    disabled: bool,
}

impl Authenticator {
    pub fn resolves_to(&self) -> Option<&str> {
        (self.version > 0 && self.verified && !self.disabled).then_some(self.identity_id.as_str())
    }

    pub fn method(&self) -> Option<AuthMethod> {
        self.method
    }

    pub fn secret_hash(&self) -> Option<&str> {
        self.secret_hash.as_deref()
    }
}

#[derive(Debug, Clone)]
pub enum AuthenticatorCommand {
    /// secret: plain, for Username method only - hashed HERE (Argon2id)
    /// so plaintext never reaches an event.
    Register {
        method: AuthMethod,
        identity_id: String,
        secret: Option<String>,
    },
    Verify,
    MoveToIdentity {
        to_identity_id: String,
        reason: String,
    },
    ChangeSecret {
        new_secret: String,
    },
    Disable {
        reason: String,
    },
    Reenable,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticatorError {
    #[error("authenticator already registered")]
    AlreadyRegistered,
    #[error("authenticator not registered")]
    NotRegistered,
    #[error("username authenticators require a secret; {0:?} must not carry one")]
    SecretMismatch(AuthMethod),
    #[error("hashing failed")]
    Hashing,
    #[error("no change")]
    NoChange,
}

impl Aggregate for Authenticator {
    type Event = AuthenticatorEvent;
    type Command = AuthenticatorCommand;
    type Error = AuthenticatorError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            AuthenticatorEvent::Registered {
                method,
                identity_id,
                secret_hash,
            } => {
                self.method = Some(*method);
                self.identity_id = identity_id.clone();
                self.secret_hash = secret_hash.clone();
                // Phone/email start unverified (OTP confirms possession);
                // username is verified by construction - the secret IS
                // the proof mechanism.
                self.verified = *method == AuthMethod::Username;
            }
            AuthenticatorEvent::Verified => self.verified = true,
            AuthenticatorEvent::MovedToIdentity { to_identity_id, .. } => {
                self.identity_id = to_identity_id.clone()
            }
            AuthenticatorEvent::SecretChanged { secret_hash } => {
                self.secret_hash = Some(secret_hash.clone())
            }
            AuthenticatorEvent::Disabled { .. } => self.disabled = true,
            AuthenticatorEvent::Reenabled => self.disabled = false,
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            AuthenticatorCommand::Register {
                method,
                identity_id,
                secret,
            } => {
                if self.version != 0 {
                    return Err(AuthenticatorError::AlreadyRegistered);
                }
                let secret_hash = match (method, secret) {
                    (AuthMethod::Username, Some(s)) => Some(
                        super::crypto::argon2id_hash(&s)
                            .map_err(|_| AuthenticatorError::Hashing)?,
                    ),
                    (AuthMethod::Username, None) => {
                        return Err(AuthenticatorError::SecretMismatch(method));
                    }
                    (_, None) => None,
                    (m, Some(_)) => return Err(AuthenticatorError::SecretMismatch(m)),
                };
                Ok(vec![AuthenticatorEvent::Registered {
                    method,
                    identity_id,
                    secret_hash,
                }])
            }
            AuthenticatorCommand::Verify => {
                if self.version == 0 {
                    return Err(AuthenticatorError::NotRegistered);
                }
                if self.verified {
                    return Err(AuthenticatorError::NoChange);
                }
                Ok(vec![AuthenticatorEvent::Verified])
            }
            AuthenticatorCommand::MoveToIdentity {
                to_identity_id,
                reason,
            } => {
                if self.version == 0 {
                    return Err(AuthenticatorError::NotRegistered);
                }
                Ok(vec![AuthenticatorEvent::MovedToIdentity {
                    from_identity_id: self.identity_id.clone(),
                    to_identity_id,
                    reason,
                }])
            }
            AuthenticatorCommand::ChangeSecret { new_secret } => {
                if self.method != Some(AuthMethod::Username) {
                    return Err(AuthenticatorError::SecretMismatch(
                        self.method.unwrap_or(AuthMethod::Phone),
                    ));
                }
                let hash = super::crypto::argon2id_hash(&new_secret)
                    .map_err(|_| AuthenticatorError::Hashing)?;
                Ok(vec![AuthenticatorEvent::SecretChanged {
                    secret_hash: hash,
                }])
            }
            AuthenticatorCommand::Disable { reason } => {
                if self.disabled {
                    return Err(AuthenticatorError::NoChange);
                }
                Ok(vec![AuthenticatorEvent::Disabled { reason }])
            }
            AuthenticatorCommand::Reenable => {
                if !self.disabled {
                    return Err(AuthenticatorError::NoChange);
                }
                Ok(vec![AuthenticatorEvent::Reenabled])
            }
        }
    }
}

impl Authenticator {
    /// Password check for Username authenticators. Argon2id verify -
    /// wrong password and missing hash are indistinguishable to callers.
    pub fn verify_secret(&self, presented: &str) -> bool {
        self.resolves_to().is_some()
            && self
                .secret_hash
                .as_deref()
                .is_some_and(|h| super::crypto::argon2id_verify(presented, h))
    }
}

pub struct LoginPolicy;
impl LoginPolicy {
    pub fn allowed_methods(audience: Audience) -> &'static [AuthMethod] {
        match audience {
            Audience::Client => &[AuthMethod::Phone],
            Audience::Employee => &[AuthMethod::Phone, AuthMethod::Email, AuthMethod::Username],
            Audience::Admin => &[AuthMethod::Username],
        }
    }
}
