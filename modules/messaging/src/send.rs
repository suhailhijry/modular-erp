//! Sending: resolve, render, meter, promise.
//!
//! # The four steps, in that order, in one transaction
//!
//! 1. **Resolve** the audience against the read model — who is this actually
//!    going to, right now (11a).
//! 2. **Render** the template against the read model — what does it say, right
//!    now (11b).
//! 3. **Charge** the meter, refusing if the month is spent (11a metering, L6).
//! 4. **Promise** the effect, in the caller's transaction (D9).
//!
//! Steps 1 and 2 happen here rather than in the dispatcher, and that is
//! deliberate. The dispatcher holds **no connection** while it delivers — a
//! documented property, and the reason a slow relay cannot exhaust a tenant's
//! pool — so a handler cannot read anything. What "at send time" has to mean is
//! *as late as possible while a connection is legitimately held*, which is
//! here: the reminder job runs shortly before the message goes, so a booking
//! that moved this morning is described as it stands this morning.
//!
//! # Why the effect carries a resolved address after all that
//!
//! Because by the time it does, the address is minutes old. The failure this
//! avoids is the one in the system this phase was read against: a number frozen
//! into a **stored message definition**, where a customer who changed their
//! number keeps getting messages at the old one for as long as the record
//! lives. A number resolved four minutes ago is not that.

use std::collections::BTreeMap;

use erp_eventlog::{ConfigError, Effect, configuration as config};
use erp_i18n::{Locale, Localize, Message, MessageArg};
use erp_types::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::audience::{Address, Audience, Subject};
use crate::budget::{self, SpendError};
use crate::channel::Channel;
use crate::settings::Settings;
use crate::template::{self, Template, TemplateError, Templates};

/// One message, ready to hand to a gateway.
///
/// **What the effect payload is.** Deliberately the same field names as
/// `erp_control::mail::Email`, so an `email.send` effect promised by this module
/// and one promised by the control plane are the same shape on the wire and the
/// same handler could read either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbound {
    pub channel: Channel,
    /// An address, a number, or a device token. Resolved, not a reference.
    pub to: String,
    /// Empty on every channel but email.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
    pub body: String,
    pub locale: Locale,
}

impl Outbound {
    /// The effect that promises to send it.
    ///
    /// The key is the caller's, so a retried send promises one message rather
    /// than two (L8) — and it reaches the gateway as an idempotency key, so a
    /// delivery this dispatcher believes failed but which succeeded is not
    /// performed twice.
    #[must_use]
    pub fn promised(&self, key: String) -> Effect {
        Effect::with_key(
            self.channel.kind(),
            key,
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        )
    }

    /// Reads one back out of an effect's payload.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(payload.clone())
    }
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct Sending {
    /// Which template, by name.
    pub template: String,
    /// What it is about. The **only** thing a caller has to supply about
    /// content — the template asks the read model for the rest.
    pub subject: Subject,
    /// **The caller's key**, and what makes a retry one message rather than
    /// two. Each address gets `{key}.{n}`, because one audience can be two
    /// people and the outbox keys one row per effect.
    pub key: String,
    /// Which identity, when the template is addressed to an operator.
    pub operator: Option<String>,
    /// Anything the caller resolved itself — `link` is the usual one, from
    /// `erp_links::shorten` in this same transaction.
    pub extra: BTreeMap<String, String>,
    /// Which language. `None` takes the tenant's own, which is what a business
    /// writing to its own customers wants.
    ///
    /// ponytail: a per-customer language preference is a `crm` field and
    /// nobody has asked for one. A Saudi salon writes Arabic to everybody, and
    /// the tenant default says so in one place instead of on ten thousand
    /// records.
    pub locale: Option<Locale>,
    pub at: Timestamp,
}

/// What was promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub channel: Channel,
    /// How many people the audience resolved to.
    pub recipients: usize,
    /// **How many of them this call actually promised.**
    ///
    /// Fewer when the same key has been sent before — which is the ordinary
    /// case for anything scheduled, because a reminder job that runs every five
    /// minutes calls this over and over for the same booking.
    pub promised: usize,
    /// What it cost, in billable units. **Zero on a repeat**, because nothing
    /// was sent: charging a retry would let a five-minute job spend a month's
    /// budget on one reminder.
    pub units: i32,
}

/// Why nothing was sent.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    Template(#[from] TemplateError),
    /// The audience resolved to nobody reachable on this channel.
    ///
    /// **A refusal rather than a silent success.** A reminder that went to
    /// nobody is a chair that stays empty, and the caller is the only thing
    /// that can do something about it.
    #[error("{audience} for {topic} {id} is nobody reachable on {channel}")]
    Unreachable {
        audience: String,
        topic: String,
        id: String,
        channel: String,
    },
    #[error(transparent)]
    Spend(#[from] SpendError),
    #[error(transparent)]
    Enqueue(#[from] erp_eventlog::EnqueueError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl Localize for SendError {
    fn message(&self) -> Message {
        match self {
            Self::Template(e) => e.message(),
            Self::Unreachable {
                audience, channel, ..
            } => Message::new(crate::messages::UNREACHABLE)
                .with("audience", MessageArg::text(audience))
                .with("channel", MessageArg::text(channel)),
            Self::Spend(SpendError::Refused(over)) => over.message(),
            Self::Spend(SpendError::Config(e)) | Self::Config(e) => e.message(),
            Self::Spend(SpendError::Database(_)) | Self::Database(_) | Self::Enqueue(_) => {
                Message::new(crate::messages::DATABASE)
            }
        }
    }
}

impl Localize for TemplateError {
    fn message(&self) -> Message {
        use crate::messages as m;
        match self {
            Self::NotAName(name) => {
                Message::new(m::NOT_A_NAME).with("name", MessageArg::text(name))
            }
            Self::UnknownBinding { binding, topic } => Message::new(m::UNKNOWN_BINDING)
                .with("binding", MessageArg::text(binding))
                .with("topic", MessageArg::text(topic)),
            Self::WrongAudience { topic, audience } => Message::new(m::WRONG_AUDIENCE)
                .with("topic", MessageArg::text(topic))
                .with("audience", MessageArg::text(audience)),
            Self::NoSubjectLine { channel } => {
                Message::new(m::NO_SUBJECT_LINE).with("channel", MessageArg::text(channel))
            }
            Self::NeedsASubject { channel } => {
                Message::new(m::NEEDS_A_SUBJECT).with("channel", MessageArg::text(channel))
            }
            Self::MissingLanguage { locale } => {
                Message::new(m::MISSING_LANGUAGE).with("locale", MessageArg::text(locale))
            }
            Self::Empty => Message::new(m::EMPTY_TEMPLATE),
            Self::NoSuchTemplate(name) => {
                Message::new(m::NO_SUCH_TEMPLATE).with("name", MessageArg::text(name))
            }
        }
    }
}

/// Resolves, renders, meters and promises — **in the caller's transaction**.
///
/// # The order matters, and it is promise-then-charge
///
/// The outbox deduplicates on the key, so promising the same message twice
/// writes one row and reports it. The charge is made **only for what was
/// actually written**, which is what stops a reminder job that runs every five
/// minutes from spending a month's budget re-promising the same reminder.
///
/// # Roll back on a refusal
///
/// The meter is written before the budget is checked, because that write is the
/// lock that makes two concurrent sends resolve to one. A caller that swallows
/// the refusal and commits has spent budget on a message it did not send. This
/// module never opens a transaction behind your back, exactly as
/// `erp_occupancy::take` does not.
pub async fn send(conn: &mut PgConnection, sending: &Sending) -> Result<Sent, SendError> {
    let templates = config::get::<Templates>(&mut *conn, template::KEY)
        .await?
        .map(|c| c.value)
        .unwrap_or_default();
    let template = templates
        .get(&sending.template)
        .filter(|t| t.active)
        .ok_or_else(|| TemplateError::NoSuchTemplate(sending.template.clone()))?;

    let settings = config::get::<Settings>(&mut *conn, crate::settings::KEY)
        .await?
        .map(|c| c.value)
        .unwrap_or_default();
    let locale = sending.locale.unwrap_or(settings.language);

    let addresses = to(conn, template, sending).await?;
    if addresses.is_empty() {
        return Err(SendError::Unreachable {
            audience: template.audience.as_str().to_owned(),
            topic: sending.subject.topic.as_str().to_owned(),
            id: sending.subject.id.as_str().to_owned(),
            channel: template.channel.as_str().to_owned(),
        });
    }

    let (subject, body) = wording(conn, template, sending, &settings, locale).await?;

    let recipients = addresses.len();
    let mut promised = 0;
    let mut units = 0;
    for (n, address) in addresses.into_iter().enumerate() {
        let message = Outbound {
            channel: template.channel,
            to: address.value,
            subject: subject.clone(),
            body: body.clone(),
            locale,
        };

        // **No cause.** The key is the caller's, which is what an effect with
        // no log position behind it needs — a reminder is not caused by an
        // event, it is caused by a clock.
        let effect = message.promised(format!("{}.{n}", sending.key));
        if erp_eventlog::enqueue(conn, None, std::slice::from_ref(&effect)).await? == 0 {
            // Already promised under this key. Nothing was written, so nothing
            // is charged.
            continue;
        }
        promised += 1;
        // **Charged per person**, because that is how a gateway bills: one
        // audience that resolves to two managers is two messages and two
        // segments each.
        //
        // `each`, and not the running total the meter hands back — adding the
        // month's total per recipient would report a two-person send as having
        // cost whatever the month has cost so far.
        let each = template.channel.units(&body);
        budget::charge(conn, template.channel, each, sending.at).await?;
        units += each;
    }

    Ok(Sent {
        channel: template.channel,
        recipients,
        promised,
        units,
    })
}

/// Who this actually reaches, right now.
async fn to(
    conn: &mut PgConnection,
    template: &Template,
    sending: &Sending,
) -> Result<Vec<Address>, SendError> {
    if template.channel == Channel::Push {
        // Push resolves to **devices**, and a device is not a property of a
        // person in any read model. See `crate::push`.
        let owner = match template.audience {
            Audience::Operator => sending.operator.clone(),
            _ => Some(sending.subject.id.as_str().to_owned()),
        };
        let Some(owner) = owner else {
            return Ok(Vec::new());
        };
        return Ok(crate::push::tokens(conn, &owner)
            .await?
            .into_iter()
            .map(|token| Address {
                channel: Channel::Push,
                value: token,
            })
            .collect());
    }

    Ok(crate::audience::resolve(
        conn,
        template.audience,
        &sending.subject,
        template.channel,
        sending.operator.as_deref(),
    )
    .await?)
}

/// What it says, right now.
async fn wording(
    conn: &mut PgConnection,
    template: &Template,
    sending: &Sending,
    settings: &Settings,
    locale: Locale,
) -> Result<(String, String), SendError> {
    let mut values = crate::bindings::of(conn, &sending.subject).await?;
    values.insert("business".to_owned(), settings.business.clone());
    // The caller's own, last, so a link it made in this transaction wins over
    // anything a read model happened to call the same thing.
    values.extend(sending.extra.clone());

    let body = template
        .body(locale)
        .ok_or(TemplateError::MissingLanguage {
            locale: locale.code().to_owned(),
        })?;

    Ok((
        template::render(&body.subject, &values),
        template::render(&body.text, &values),
    ))
}
