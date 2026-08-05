use std::sync::Arc;

use crate::{
    auth::{
        audience::Audience,
        authenticator::{AuthMethod, Authenticator, LoginPolicy},
        otp_store::{OtpError, OtpRequestOutcome, OtpStore, otp_policy_for},
        session_store::SessionStore,
    },
    event_sourcing::{EventStore, load_aggregate},
};

pub struct LoginFlow {
    pub store: Arc<dyn EventStore>,
    pub sessions: Arc<dyn SessionStore>,
    pub otp: Arc<dyn OtpStore>,
}

impl LoginFlow {
    /// Step 1 of phone/email login. Returns the code TO SEND (caller
    /// owns SMS/email delivery). Errors are deliberately uniform: an
    /// attacker probing identifiers learns nothing from the response
    /// shape (send the "if this number is registered, a code was sent"
    /// message regardless).
    pub async fn otp_request(
        &self,
        method: AuthMethod,
        identifier: &str,
        audience: Audience,
    ) -> Result<Option<String>, OtpError> {
        if !LoginPolicy::allowed_methods(audience).contains(&method) {
            return Err(OtpError::Unusable);
        }
        let auth_id = authenticator_id(method, identifier);
        let authenticator = load_aggregate::<Authenticator>(self.store.as_ref(), &auth_id)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;
        if authenticator.resolves_to().is_none() {
            // Unknown/disabled identifier: succeed silently (no code to
            // send). The caller's response is identical either way.
            return Ok(None);
        }
        match self.otp.request(&auth_id, otp_policy_for(audience)).await? {
            OtpRequestOutcome::Send { code } => Ok(Some(code)),
            OtpRequestOutcome::RecentlySent => Ok(None),
        }
    }

    /// Step 2: verify code -> session.
    pub async fn otp_verify(
        &self,
        method: AuthMethod,
        identifier: &str,
        code: &str,
        audience: Audience,
    ) -> Result<String, OtpError> {
        if !LoginPolicy::allowed_methods(audience).contains(&method) {
            return Err(OtpError::Unusable);
        }
        let auth_id = authenticator_id(method, identifier);
        let authenticator = load_aggregate::<Authenticator>(self.store.as_ref(), &auth_id)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;
        let Some(identity_id) = authenticator.resolves_to() else {
            return Err(OtpError::Invalid);
        };
        self.otp
            .verify(&auth_id, code, otp_policy_for(audience))
            .await?;
        self.sessions
            .create(identity_id, audience)
            .await
            .map_err(OtpError::Other)
            .map_err(Into::into)
    }

    /// Username+password -> session. Verify runs even for unknown
    /// identitynames (against a dummy hash) so response TIMING doesn't
    /// enumerate identitynames.
    pub async fn password_login(
        &self,
        username: &str,
        password: &str,
        audience: Audience,
    ) -> Result<String, OtpError> {
        if !LoginPolicy::allowed_methods(audience).contains(&AuthMethod::Username) {
            return Err(OtpError::Unusable);
        }
        let auth_id = authenticator_id(AuthMethod::Username, username);
        let authenticator = load_aggregate::<Authenticator>(self.store.as_ref(), &auth_id)
            .await
            .map_err(|e| OtpError::Other(e.into()))?;

        if authenticator.resolves_to().is_none() {
            // Burn comparable time to a real verify, then uniform error.
            let _ = super::crypto::argon2id_verify(password, DUMMY_HASH);
            return Err(OtpError::Invalid);
        }
        if !authenticator.verify_secret(password) {
            return Err(OtpError::Invalid);
        }
        let identity_id = authenticator.resolves_to().expect("checked above");
        self.sessions
            .create(identity_id, audience)
            .await
            .map_err(OtpError::Other)
            .map_err(Into::into)
    }
}

pub fn authenticator_id(method: AuthMethod, identifier: &str) -> String {
    let m = match method {
        AuthMethod::Phone => "phone",
        AuthMethod::Email => "email",
        AuthMethod::Username => "identityname",
    };
    // Normalize: identifiers are aggregate ids, so "+9665..." and
    // " +9665... " must be the same aggregate. Extend with full E.164
    // normalization for phones at the API edge.
    format!("{m}:{}", identifier.trim().to_lowercase())
}

/// A real Argon2id hash of an unguessable value - exists only to make
/// unknown-identityname verification take the same time as a real one.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$YXV0aGRlc2lnbnNhbHQ$XHuEz4FqIWi5jFF1JWLScS0PLLQfjcHXHu5i4WQTWHs";
