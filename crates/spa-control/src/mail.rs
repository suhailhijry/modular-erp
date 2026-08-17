//! Email, as a promise rather than a send.
//!
//! # What is here and what is not
//!
//! Here: the *shape* of an email and the effect kind that carries it. Sending is
//! a `spa-worker` concern and this crate has no idea how it happens — no SMTP,
//! no address of a relay, no credentials.
//!
//! That split is D9 restated. A control-plane operation that sent mail inline
//! would have two failure modes with no good answer: send before commit and a
//! rolled-back invitation has already mailed somebody, send after commit and a
//! crash loses it with nothing recording that it was owed. Writing the
//! *intention* into the same transaction as the invitation removes both.
//!
//! # Why the text is rendered here, not at delivery
//!
//! Because the effect must record a **resolved decision** (L5). The recipient of
//! an invitation has no account, so no stored language preference exists to
//! render from later; what does exist, at the moment of inviting, is the
//! language the inviter was working in. That is the best signal there will ever
//! be, and it is gone by the time a worker picks the row up.
//!
//! It also means a catalog edit does not silently change what an already-issued
//! invitation says, which is the same reason an invoice stores its VAT rate.

use spa_i18n::{Catalog, Locale, Message, MessageArg};
use spa_types::EffectKind;

/// The kind every email effect is enqueued under. One handler answers for it.
#[must_use]
pub fn email_kind() -> EffectKind {
    EffectKind::new("email.send")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies EffectKind"))
}

/// One message, ready to send. No template, no locale, no lookup — those
/// happened before this was written down.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Email {
    /// A single recipient. Nothing here needs more than one, and a list would
    /// need a rule about partial failure that nothing is asking for yet.
    pub to: String,
    pub subject: String,
    /// Plain text. HTML mail needs a template system, an inliner, and a plain
    /// alternative to go with it; an invitation is four sentences and a link.
    pub body: String,
    /// The language it was written in, so a delivery log can say so and a bounce
    /// can be answered in it.
    pub locale: Locale,
}

impl Email {
    /// Renders one from the catalog, in the language the sender was working in.
    ///
    /// `subject` and `body` are two codes rather than one, because a subject
    /// line and a body wrap differently in Arabic and pretending they are one
    /// string produces a subject with a paragraph in it.
    pub fn rendered(
        catalog: &dyn Catalog,
        locale: Locale,
        to: String,
        subject: &Message,
        body: &Message,
    ) -> Self {
        Self {
            to,
            subject: catalog.render_or_code(locale, subject),
            body: catalog.render_or_code(locale, body),
            locale,
        }
    }

    /// The effect that promises to send it.
    ///
    /// The key is **pinned by the caller**, never derived: the control plane has
    /// no event log, so there is no position to derive one from — and pinning is
    /// what makes re-inviting the same address enqueue one email rather than
    /// two.
    #[must_use]
    pub fn promised(&self, key: String) -> spa_eventlog::Effect {
        spa_eventlog::Effect::with_key(
            email_kind(),
            key,
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Reads one back out of an effect's payload.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(payload.clone())
    }
}

/// The invitation email, as the two messages it is made of.
///
/// A free function rather than a method on `Invitation` because it needs the
/// link, and the link needs the public domain — which is a deployment fact the
/// control plane does not have and should not learn.
#[must_use]
pub fn invitation_messages(company: &str, link: &str) -> (Message, Message) {
    (
        Message::new(crate::messages::INVITATION_SUBJECT)
            .with("company", MessageArg::text(company.to_owned())),
        Message::new(crate::messages::INVITATION_BODY)
            .with("company", MessageArg::text(company.to_owned()))
            .with("link", MessageArg::text(link.to_owned())),
    )
}
