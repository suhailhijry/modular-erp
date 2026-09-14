//! Announcing: resolve, word, prefer, record, fan out.
//!
//! # The order, and why the record comes before the messages
//!
//! 1. **Resolve** the audience against the read model — who has a login, right
//!    now — or, for a kind the caller names the people for, take its names.
//! 2. **Word it** in both languages, against the bindings, right now.
//! 3. **Record it** in the tenant's log. This is the notification.
//! 4. **Fan out** to the paid channels each person asked for.
//!
//! Three before four, because a notification that exists only as an SMS did not
//! happen for anybody who was not holding their phone. The record is the thing;
//! the message is a copy of it sent somewhere else.
//!
//! All four in **the caller's transaction**, so a producer that rolls back
//! announces nothing and charges nothing — the same contract
//! `messaging::send` has.

use std::collections::BTreeMap;
use std::convert::Infallible;

use erp_eventlog::{ConfigError, Decision, ExecuteError, Metadata, configuration as config};
use erp_i18n::{Locale, Localize, Message, MessageArg};
use erp_types::{AggregateId, Timestamp};
use messaging::template::{self, Templates};
use messaging::{Channel, Settings, Subject};
use sqlx::PgConnection;

use crate::Kind;
use crate::notification::{Notification, NotificationEvent, Wording};

/// What a producer asks for.
#[derive(Debug, Clone)]
pub struct Announcing {
    pub kind: Kind,
    /// What it is about. The **only** thing a producer supplies about content —
    /// the wording asks the read model for the rest.
    pub subject: Subject,
    /// **Who is told, for a kind the caller names them for**
    /// ([`Kind::told_by_caller`]): logins, resolved by whoever could ask. Not
    /// read for any other kind, whose audience is resolved here — leave it
    /// empty.
    pub to: Vec<String>,
    pub at: Timestamp,
}

/// What was announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    /// How many people it reached.
    pub recipients: usize,
    /// **Whether this call wrote it.** `false` when the same kind about the
    /// same subject was already announced — the ordinary case for a producer
    /// that sweeps a window every tick.
    pub announced: bool,
    /// How many messages were promised beyond the bell, across everybody.
    pub promised: usize,
}

/// Why nothing was announced.
#[derive(Debug, thiserror::Error)]
pub enum AnnounceError {
    /// Nobody with a login is listed to hear about this.
    ///
    /// **A refusal rather than a silent success**, for the same reason
    /// `messaging::send` refuses an unreachable audience: an announcement that
    /// reached nobody is something only the caller can do anything about — most
    /// often because no employee has been linked to a login. Every producer
    /// logs it and carries on to the next row.
    #[error("nobody with a login is listed to be told about {kind}")]
    Unreachable { kind: String },
    #[error(transparent)]
    Send(#[from] messaging::SendError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Execute(#[from] ExecuteError<Infallible>),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl Localize for AnnounceError {
    fn message(&self) -> Message {
        match self {
            Self::Unreachable { kind } => {
                Message::new(crate::messages::UNREACHABLE).with("kind", MessageArg::text(kind))
            }
            Self::Send(e) => e.message(),
            Self::Config(e) => e.message(),
            Self::Execute(_) | Self::Database(_) => Message::new(crate::messages::DATABASE),
        }
    }
}

/// Tells whoever should know.
///
/// **In the caller's transaction.** This module never opens one behind your
/// back, exactly as `messaging::send` does not: commit and the notification
/// stands, roll back and nothing was announced and nothing was charged.
#[expect(
    clippy::too_many_lines,
    reason = "the four steps of announcing, in the order the doc comment gives \
              them; splitting them would hide that order"
)]
pub async fn announce(
    conn: &mut PgConnection,
    announcing: &Announcing,
    metadata: &Metadata,
) -> Result<Announced, AnnounceError> {
    // 1 · Who, right now. The first audience that resolves to anybody with a
    // login wins, which is how one field says "the stylist it was booked with,
    // and whoever runs the branch when nobody was assigned".
    let mut people = Vec::new();
    if announcing.kind.told_by_caller() {
        // **Logins, and nothing else about them.** They reach the bell; a paid
        // channel needs an address, and an address is `hr`'s to know.
        people = announcing
            .to
            .iter()
            .map(|identity| messaging::audience::Person {
                identity: Some(identity.clone()),
                email: None,
                phone: None,
            })
            .collect();
    }
    for audience in announcing.kind.audiences() {
        let found = messaging::audience::people(&mut *conn, *audience, &announcing.subject, None)
            .await?
            .into_iter()
            .filter(|person| person.identity.is_some())
            .collect::<Vec<_>>();
        if !found.is_empty() {
            people = found;
            break;
        }
    }
    if people.is_empty() {
        return Err(AnnounceError::Unreachable {
            kind: announcing.kind.as_str().to_owned(),
        });
    }
    let recipients: Vec<String> = people
        .iter()
        .filter_map(|person| person.identity.clone())
        .collect();

    // 2 · What it says, in both languages, as things stand this minute.
    let wording = wording(&mut *conn, announcing).await?;

    // 3 · The record, before anything leaves the building.
    let id = derived_id(announcing.kind, &announcing.subject);
    let event = NotificationEvent::Announced {
        kind: announcing.kind.as_str().to_owned(),
        topic: announcing.subject.topic.as_str().to_owned(),
        subject: announcing.subject.id.clone(),
        recipients: recipients.clone(),
        wording: wording.clone(),
        at: announcing.at,
    };
    let committed = erp_eventlog::try_create::<Notification, _, Infallible>(
        &mut *conn,
        &id,
        crate::upcasters(),
        metadata,
        move |_| Ok(Decision::one(event.clone())),
    )
    .await?;
    if committed.events.is_empty() {
        // **Already announced.** The id is derived, so this is the same thing
        // being said again — which is what lets every producer be a scan that
        // may run twice. Nothing is written and nothing is promised twice.
        return Ok(Announced {
            recipients: recipients.len(),
            announced: false,
            promised: 0,
        });
    }

    // 4 · And out, to whatever each of them asked for beyond the bell.
    let wanted = crate::preferences_for(&mut *conn, &recipients, announcing.kind).await?;
    let locale = config::get::<Settings>(&mut *conn, messaging::settings::KEY)
        .await?
        .map_or_else(|| Settings::default().language, |c| c.value.language);
    let said = wording
        .get(locale.code())
        .or_else(|| wording.get(Locale::DEFAULT.code()));

    let mut promised = 0;
    for person in &people {
        let Some(identity) = person.identity.as_deref() else {
            continue;
        };
        let channels = wanted
            .get(identity)
            .map_or(crate::DEFAULT_CHANNELS.as_slice(), Vec::as_slice);
        for channel in channels {
            // The bell is the record above; there is nothing to promise for it.
            if *channel == Channel::InSystem {
                continue;
            }
            let (Some(address), Some(said)) = (person.reachable_on(*channel), said) else {
                // Somebody who asked for SMS and has no number is not a
                // failure: they still have the bell, which is the channel that
                // cannot fail to have an address.
                continue;
            };
            let message = messaging::Outbound {
                channel: *channel,
                to: address.value,
                subject: if channel.has_a_subject() {
                    said.title.clone()
                } else {
                    String::new()
                },
                body: said.body.clone(),
                locale,
                platform: address.platform,
            };
            // Keyed on the notification and the person, so a producer that
            // announces the same thing twice promises one message (L8) — and
            // the outbox deduplicates on it even if the record above somehow
            // did not.
            let key = format!("{}.{identity}.{}", id.as_str(), channel.as_str());
            if messaging::deliver(
                &mut *conn,
                &message,
                key,
                Some(&announcing.subject),
                announcing.at,
            )
            .await?
            {
                promised += 1;
            }
        }
    }

    Ok(Announced {
        recipients: recipients.len(),
        announced: true,
        promised,
    })
}

/// What it says, in every language, right now.
///
/// The tenant's own template wins when there is an active one named after the
/// kind on the in-system channel; otherwise the compiled copy. Both are
/// rendered against the same bindings, so "a booking that moved says the new
/// time" holds either way.
async fn wording(
    conn: &mut PgConnection,
    announcing: &Announcing,
) -> Result<BTreeMap<String, Wording>, AnnounceError> {
    let templates = config::get::<Templates>(&mut *conn, template::KEY)
        .await?
        .map(|c| c.value)
        .unwrap_or_default();
    let own = templates
        .get(announcing.kind.as_str())
        .filter(|t| t.active && t.channel == Channel::InSystem);

    let settings = config::get::<Settings>(&mut *conn, messaging::settings::KEY)
        .await?
        .map(|c| c.value)
        .unwrap_or_default();

    let mut values = messaging::bindings::of(&mut *conn, &announcing.subject).await?;
    values.insert("business".to_owned(), settings.business.clone());

    let mut wording = BTreeMap::new();
    for locale in Locale::ALL {
        let (title, body) = if let Some(body) = own.and_then(|t| t.body(locale)) {
            (body.subject.clone(), body.text.clone())
        } else {
            let copy = crate::copy::of(announcing.kind, locale, &values);
            (copy.title.to_owned(), copy.body.to_owned())
        };
        wording.insert(
            locale.code().to_owned(),
            Wording {
                title: template::render(&title, &values),
                body: template::render(&body, &values),
            },
        );
    }
    Ok(wording)
}

/// **Derived, never minted** (L8).
///
/// The same kind about the same subject is the same notification. That single
/// fact is what removes the queue from this design: a producer sweeps a window
/// every tick and announces everything in it, and the second attempt loads an
/// aggregate that already exists and writes nothing.
#[must_use]
pub fn derived_id(kind: Kind, subject: &Subject) -> AggregateId {
    let name = format!(
        "{}:{}:{}",
        kind.as_str(),
        subject.topic.as_str(),
        subject.id.as_str()
    );
    AggregateId::new(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()).to_string())
        .unwrap_or_else(|_| unreachable!("a uuid satisfies AggregateId"))
}
