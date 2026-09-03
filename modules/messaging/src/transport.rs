//! Handing a message to something that can deliver it.
//!
//! # Why a trait, and the same shape `Mailer` has
//!
//! Because the interesting part is not the wire protocol. It is **what a
//! failure means** — which errors are worth retrying and which will never work
//! — and a test of that has to be able to make delivery fail on demand, and
//! must never send a real message by accident. `erp_worker::mail::Mailer` and
//! `tax_sa`'s `Registrar` are the same shape for the same reason.
//!
//! # What is here, and what is honestly not
//!
//! Two named gateways — [`crate::Taqnyat`] for SMS and [`crate::Fcm`] for push —
//! and [`Relay`], the outbound contract this system defines for everything else.
//! A named gateway wins over the relay for its channel; a channel with neither
//! leaves its messages in the outbox rather than dead-lettering them.
//!
//! Neither gateway has been used against a real account by this build. What is
//! tested is the bytes each puts on the wire and the answer it makes of every
//! documented reply, against a server that shows those bytes — see
//! `crate::fake`. The first live call is the operator's, which is the same
//! position the ZATCA client is in and was right there too.
//!
//! **No `WhatsApp` adapter, and it is not an oversight.** Meta accepts free-form
//! text only inside a 24-hour window the *customer* opens by messaging the
//! business; outside it only pre-approved templates are accepted, and a
//! passthrough template whose body is one variable is rejected at creation. This
//! module hands a transport a finished string, so a `WhatsApp` client written to
//! this trait would fail `131047` on every reminder it sent. What it needs is a
//! template model rather than an adapter — review §26 in the plan. `Relay` is
//! the answer for a deployment that has built that mapping outside this system.

use std::sync::Arc;

use erp_eventlog::{DeliveryError, EffectHandler, PendingEffect};
use erp_types::EffectKind;

use crate::channel::Channel;
use crate::send::Outbound;

/// Somewhere to hand a message to.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Which channel this delivers.
    fn channel(&self) -> Channel;

    /// Sends one message.
    ///
    /// `key` is the effect's idempotency key. Pass it downstream wherever the
    /// provider accepts one, so a delivery this dispatcher believes failed but
    /// which actually succeeded is not performed twice.
    async fn send(&self, message: &Outbound, key: &str) -> Result<(), TransportError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Unreachable, slow, or a 4xx worth another go. Retried.
    #[error("{0}")]
    Unreachable(String),
    /// Will never work: a malformed address, a rejected body, a 5xx from
    /// something that has made up its mind. Dead-lettered.
    #[error("{0}")]
    Refused(String),
    /// **The address itself is dead** — a push token the platform has retired.
    ///
    /// Separate from `Refused` because it is the only failure that means
    /// something has to be *written down*: the token must stop being used, or
    /// every future message to that person burns eight attempts on it.
    #[error("{0}")]
    AddressRetired(String),
}

/// Delivers one channel's effects.
pub struct MessageHandler {
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for MessageHandler {
    /// Says nothing about the transport: a `Relay` holds a bearer token, and
    /// the one thing a `Debug` impl reliably does is end up in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageHandler")
            .field("channel", &self.transport.channel())
            .finish_non_exhaustive()
    }
}

impl MessageHandler {
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

#[async_trait::async_trait]
impl EffectHandler for MessageHandler {
    fn kind(&self) -> EffectKind {
        self.transport.channel().kind()
    }

    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError> {
        // A payload this build cannot read is **permanent**, not retryable.
        // Retrying would burn every attempt and dead-letter it anyway, three
        // backoffs later and with a misleading error on the row.
        let message = Outbound::from_payload(&effect.payload)
            .map_err(|e| DeliveryError::Permanent(format!("not a message effect: {e}")))?;

        match self.transport.send(&message, &effect.idempotency_key).await {
            Ok(()) => Ok(()),
            Err(TransportError::Unreachable(why)) => Err(DeliveryError::Retryable(why)),
            Err(TransportError::Refused(why)) => Err(DeliveryError::Permanent(why)),
            // **The handler holds no connection** — a documented property of
            // the dispatcher, and the reason a slow relay cannot exhaust a
            // tenant's pool — so retiring the token cannot happen here. It
            // happens on the next sweep, from the dead letter this becomes.
            Err(TransportError::AddressRetired(why)) => Err(DeliveryError::Permanent(format!(
                "the address is retired: {why}"
            ))),
        }
    }
}

/// Every handler this module contributes, one per transport given.
///
/// A worker deployed with an SMS transport and no `WhatsApp` one registers one
/// handler, and `WhatsApp` effects wait in the outbox for a worker that has it —
/// which is the dispatcher's existing behaviour and better than dead-lettering
/// a tenant's messages during a staggered rollout.
#[must_use]
pub fn handlers(transports: Vec<Arc<dyn Transport>>) -> Vec<Arc<dyn EffectHandler>> {
    transports
        .into_iter()
        .map(|transport| Arc::new(MessageHandler::new(transport)) as Arc<dyn EffectHandler>)
        .collect()
}

// ---------------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------------

/// **The outbound contract this system defines.**
///
/// One `POST` per message, to a URL an operator configures, with a bearer
/// token:
///
/// ```json
/// {
///   "channel": "sms",
///   "to": "+966500000000",
///   "subject": "",
///   "body": "موعدك غدًا الساعة ١٠:٠٠",
///   "locale": "arabic",
///   "key": "booking.reminder.BK-1.0"
/// }
/// ```
///
/// `key` is the idempotency key and the relay **must** treat two posts with the
/// same one as the same message. Everything else is the payload verbatim.
///
/// The answer decides what happens next, and the three cases are the three
/// variants of [`TransportError`]:
///
/// | Answer | Meaning |
/// |---|---|
/// | `2xx` | Sent, or accepted for sending |
/// | `410 Gone` | The address is dead — a retired push token. Never retried |
/// | any other `4xx` | Will never work. Dead-lettered |
/// | `5xx`, timeout, refused | Worth another go |
pub struct Relay {
    channel: Channel,
    url: String,
    token: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL, and **not** the token.
        f.debug_struct("Relay")
            .field("channel", &self.channel)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Relay {
    /// How long one delivery may take.
    ///
    /// Comfortably inside the dispatcher's default thirty-second lease, so a
    /// slow relay cannot still be in flight when another dispatcher takes the
    /// effect and sends it again.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

    pub fn new(channel: Channel, url: &str, token: &str) -> Result<Self, TransportError> {
        if !url.starts_with("https://") && !url.starts_with("http://localhost") {
            // A message can carry a link that is a credential, and this one
            // carries a bearer token in a header. Cleartext to anywhere but a
            // sidecar on the same host is a refusal.
            return Err(TransportError::Refused(format!(
                "{url} is not somewhere a message may be sent"
            )));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| TransportError::Refused(format!("the client cannot be built: {e}")))?;

        Ok(Self {
            channel,
            url: url.to_owned(),
            token: token.to_owned(),
            client,
        })
    }
}

#[async_trait::async_trait]
impl Transport for Relay {
    fn channel(&self) -> Channel {
        self.channel
    }

    async fn send(&self, message: &Outbound, key: &str) -> Result<(), TransportError> {
        let mut body = serde_json::to_value(message).map_err(|e| {
            TransportError::Refused(format!("a message that will not serialize: {e}"))
        })?;
        if let Some(object) = body.as_object_mut() {
            object.insert("key".to_owned(), serde_json::Value::String(key.to_owned()));
        }

        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| TransportError::Unreachable(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // Read the body for the message, and cap it: a relay that answers with
        // a megabyte of HTML should not put a megabyte of HTML in a dead letter.
        let said = response.text().await.unwrap_or_default();
        let said: String = said.chars().take(500).collect();

        if status == reqwest::StatusCode::GONE {
            return Err(TransportError::AddressRetired(said));
        }
        if status.is_client_error() {
            return Err(TransportError::Refused(format!("{status}: {said}")));
        }
        Err(TransportError::Unreachable(format!("{status}: {said}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_will_not_carry_a_bearer_token_in_the_clear() {
        assert!(Relay::new(Channel::Sms, "https://relay.test/send", "t").is_ok());
        assert!(Relay::new(Channel::Sms, "http://localhost:9000/send", "t").is_ok());
        assert!(Relay::new(Channel::Sms, "http://relay.test/send", "t").is_err());
        assert!(Relay::new(Channel::Sms, "ftp://relay.test", "t").is_err());
    }

    /// The token is the one thing that must never reach a log line.
    #[test]
    fn a_relay_does_not_print_its_token() {
        let relay =
            Relay::new(Channel::Sms, "https://relay.test/send", "sk-secret").expect("built");
        let printed = format!("{relay:?}");
        assert!(printed.contains("relay.test"));
        assert!(!printed.contains("sk-secret"), "{printed}");
    }
}
