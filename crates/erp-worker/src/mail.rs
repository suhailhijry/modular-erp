//! Delivering email effects.
//!
//! # What this is the answer to
//!
//! The outbox was built in Phase 2 and then had **no producers and no
//! handlers** for the whole of Phases 3 and 4. Effects as values, claim under
//! `SKIP LOCKED`, leases, backoff, dead letters, the at-least-once idempotency
//! key, crash tests proving a lost delivery record replays with the same key —
//! all of it finished, tested, and reaching nothing. An invitation was a link
//! somebody copied out of an API response by hand.
//!
//! This is the first handler. It is deliberately the smallest thing that can be
//! one.
//!
//! # Why a `Mailer` trait rather than lettre directly
//!
//! Because the interesting part of this file is not SMTP. It is *what a failure
//! means*: which errors are worth retrying and which will never work, and what
//! the handler does with an address the relay rejects. A test of that has to be
//! able to make the transport fail on demand, and it must never send real mail
//! by accident.
//!
//! Same shape as `tax_sa`'s `Registrar`, for the same reason.

use std::sync::Arc;

use erp_control::mail::{Email, email_kind};
use erp_eventlog::{DeliveryError, EffectHandler, PendingEffect};
use erp_types::EffectKind;

/// Somewhere to hand a message to.
#[async_trait::async_trait]
pub trait Mailer: Send + Sync {
    /// Sends one message.
    ///
    /// `key` is the effect's idempotency key. SMTP has no idempotency parameter,
    /// so it goes out as a `Message-ID`-adjacent header — which is what lets a
    /// relay, or a person reading a mailbox, tell a duplicate delivery from two
    /// genuine invitations to the same address.
    async fn send(&self, email: &Email, key: &str) -> Result<(), MailError>;
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// The relay was unreachable, slow, or answered 4xx. Worth another go.
    #[error("{0}")]
    Unreachable(String),
    /// The relay answered 5xx, or the message could not be built at all — a
    /// malformed address, a body that is not a body. Another attempt produces
    /// the same answer.
    #[error("{0}")]
    Refused(String),
}

/// Delivers `email.send`.
pub struct EmailHandler {
    mailer: Arc<dyn Mailer>,
}

impl std::fmt::Debug for EmailHandler {
    /// Says nothing about the mailer, deliberately: an `Smtp` holds a relay URL
    /// with a password in it, and the one thing a `Debug` impl reliably does is
    /// end up in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailHandler").finish_non_exhaustive()
    }
}

impl EmailHandler {
    #[must_use]
    pub fn new(mailer: Arc<dyn Mailer>) -> Self {
        Self { mailer }
    }
}

#[async_trait::async_trait]
impl EffectHandler for EmailHandler {
    fn kind(&self) -> EffectKind {
        email_kind()
    }

    async fn deliver(&self, effect: &PendingEffect) -> Result<(), DeliveryError> {
        // A payload this build cannot read is **permanent**, not retryable.
        // Retrying it would burn every attempt and dead-letter it anyway, three
        // backoffs later and with a misleading error on the row.
        let email = Email::from_payload(&effect.payload)
            .map_err(|e| DeliveryError::Permanent(format!("not an email effect: {e}")))?;

        match self.mailer.send(&email, &effect.idempotency_key).await {
            Ok(()) => Ok(()),
            Err(MailError::Unreachable(why)) => Err(DeliveryError::Retryable(why)),
            Err(MailError::Refused(why)) => Err(DeliveryError::Permanent(why)),
        }
    }
}

// ---------------------------------------------------------------------------
// SMTP
// ---------------------------------------------------------------------------

/// A relay, and the address mail comes from.
pub struct Smtp {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl std::fmt::Debug for Smtp {
    /// The sender, and **not** the relay: `SMTP_URL` carries the password, and
    /// a struct that prints it is one `tracing::debug!` away from a credential
    /// in a log aggregator.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Smtp")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

impl Smtp {
    /// Builds one from a URL and a sender.
    ///
    /// The URL is lettre's own form — `smtps://user:pass@relay:465` for implicit
    /// TLS, `smtp://user:pass@relay:587?tls=required` for STARTTLS. Every
    /// provider this would plausibly be pointed at documents both, and so does
    /// a self-hosted Postfix, which is the whole argument for SMTP over one
    /// vendor's JSON.
    ///
    /// **`tls=required` matters.** lettre's `smtp://` without it will happily
    /// continue in the clear if the relay does not offer STARTTLS, and mail
    /// carrying an invitation link is mail carrying a credential.
    pub fn new(url: &str, from: &str) -> Result<Self, MailError> {
        let transport = lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::from_url(url)
            .map_err(|e| MailError::Refused(format!("SMTP_URL is not usable: {e}")))?
            .build();
        let from = from
            .parse()
            .map_err(|e| MailError::Refused(format!("SMTP_FROM is not an address: {e}")))?;
        Ok(Self { transport, from })
    }
}

#[async_trait::async_trait]
impl Mailer for Smtp {
    async fn send(&self, email: &Email, key: &str) -> Result<(), MailError> {
        use lettre::AsyncTransport as _;

        let to: lettre::message::Mailbox = email
            .to
            .parse()
            // An address that will not parse will not parse next time either.
            .map_err(|e| MailError::Refused(format!("{} is not an address: {e}", email.to)))?;

        let message = lettre::Message::builder()
            .from(self.from.clone())
            .to(to)
            // The subject is encoded per RFC 2047 by the builder, which is what
            // an Arabic subject line needs and what a hand-rolled header would
            // get wrong.
            .subject(email.subject.clone())
            // Marks a redelivery as one. See `Mailer::send`.
            .header(lettre::message::header::ContentType::TEXT_PLAIN)
            .message_id(Some(format!("<{key}@erp>")))
            .body(email.body.clone())
            .map_err(|e| MailError::Refused(format!("could not build the message: {e}")))?;

        self.transport
            .send(message)
            .await
            .map(|_| ())
            // **Every SMTP failure is retryable except a permanent one.**
            //
            // lettre reports a 5xx as `is_permanent()`. Everything else — a
            // refused connection, a TLS handshake, a 4xx greylisting, a timeout
            // — is the relay having a moment, and dead-lettering an invitation
            // because a mail server was restarting would be losing it.
            .map_err(|e| {
                let why = e.to_string();
                if e.is_permanent() {
                    MailError::Refused(why)
                } else {
                    MailError::Unreachable(why)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_i18n::Locale;

    /// Records what it was asked to send, and can be told to fail.
    struct Fake {
        sent: std::sync::Mutex<Vec<(Email, String)>>,
        fails: Option<MailError>,
    }

    impl Fake {
        fn working() -> Arc<Self> {
            Arc::new(Self {
                sent: std::sync::Mutex::new(Vec::new()),
                fails: None,
            })
        }
        fn failing(with: MailError) -> Arc<Self> {
            Arc::new(Self {
                sent: std::sync::Mutex::new(Vec::new()),
                fails: Some(with),
            })
        }
    }

    #[async_trait::async_trait]
    impl Mailer for Fake {
        async fn send(&self, email: &Email, key: &str) -> Result<(), MailError> {
            match &self.fails {
                Some(MailError::Unreachable(why)) => Err(MailError::Unreachable(why.clone())),
                Some(MailError::Refused(why)) => Err(MailError::Refused(why.clone())),
                None => {
                    self.sent
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((email.clone(), key.to_owned()));
                    Ok(())
                }
            }
        }
    }

    fn effect(payload: serde_json::Value) -> PendingEffect {
        PendingEffect {
            id: 1,
            kind: email_kind(),
            payload,
            idempotency_key: "invitation:abc".to_owned(),
            attempts: 1,
            caused_by: None,
            enqueued_at: chrono::Utc::now(),
        }
    }

    fn an_email() -> Email {
        Email {
            to: "sara@example.test".to_owned(),
            subject: "تمت دعوتك إلى شركة الرياض".to_owned(),
            body: "افتح هذا الرابط:\nhttps://acme.erp.test/v1/join/xyz".to_owned(),
            locale: Locale::Arabic,
        }
    }

    #[tokio::test]
    async fn an_email_effect_reaches_the_mailer_with_its_idempotency_key() {
        let mailer = Fake::working();
        let handler = EmailHandler::new(mailer.clone());

        handler
            .deliver(&effect(
                serde_json::to_value(an_email()).expect("serializes"),
            ))
            .await
            .expect("delivers");

        let sent = mailer
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, an_email(), "the payload arrived unchanged");
        assert_eq!(
            sent[0].1, "invitation:abc",
            "without the key a redelivery is indistinguishable from a second invitation"
        );
    }

    /// **The distinction the whole handler exists to make.**
    ///
    /// A relay that is down must be retried; one that refuses the message must
    /// not be. Getting these the wrong way round either loses invitations to a
    /// restart, or spends four attempts and two minutes of backoff on an
    /// address that does not exist.
    #[tokio::test]
    async fn a_relay_having_a_moment_is_retried_and_a_refusal_is_not() {
        let down = EmailHandler::new(Fake::failing(MailError::Unreachable(
            "connection refused".to_owned(),
        )));
        let refused = EmailHandler::new(Fake::failing(MailError::Refused(
            "550 no such user".to_owned(),
        )));
        let payload = serde_json::to_value(an_email()).expect("serializes");

        assert!(matches!(
            down.deliver(&effect(payload.clone())).await,
            Err(DeliveryError::Retryable(_))
        ));
        assert!(matches!(
            refused.deliver(&effect(payload)).await,
            Err(DeliveryError::Permanent(_))
        ));
    }

    /// A payload this build cannot read is dead on arrival, not retried three
    /// times first with a misleading error left on the row.
    #[tokio::test]
    async fn a_payload_that_is_not_an_email_is_permanent() {
        let handler = EmailHandler::new(Fake::working());
        let verdict = handler
            .deliver(&effect(serde_json::json!({ "nothing": "useful" })))
            .await;
        assert!(
            matches!(verdict, Err(DeliveryError::Permanent(_))),
            "{verdict:?}"
        );
    }

    /// A misconfigured deployment fails where it is configured, not on the first
    /// invitation somebody sends.
    #[test]
    fn an_unusable_relay_or_sender_is_refused_at_construction() {
        assert!(Smtp::new("not a url", "ERP <noreply@erp.test>").is_err());
        assert!(Smtp::new("smtp://localhost:2525?tls=required", "not an address").is_err());
        assert!(
            Smtp::new(
                "smtp://localhost:2525?tls=required",
                "ERP <noreply@erp.test>"
            )
            .is_ok()
        );
    }
}
