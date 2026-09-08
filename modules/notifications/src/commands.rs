//! Reading your own bell, and saying what you want in it.

use std::collections::BTreeMap;

use erp_eventlog::{Decision, Metadata};
use erp_i18n::{Localize, Message};
use erp_tenant::TenantDb;
use erp_types::{AggregateId, Timestamp};
use messaging::Channel;

use crate::notification::{Notification, NotificationEvent};
use crate::person::{Person, PersonEvent};

type Read =
    Result<erp_eventlog::Committed<NotificationEvent>, erp_tenant::CommandError<NotificationError>>;
type Said =
    Result<erp_eventlog::Committed<PersonEvent>, erp_tenant::CommandError<NotificationError>>;

/// How many a single *mark everything read* covers.
///
/// A person with more unread than this clears the newest and clicks again.
/// Unbounded would be one transaction appending an event per notification, on a
/// table nobody has looked at for a year.
pub const MARK_ALL_LIMIT: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotificationError {
    /// Not addressed to whoever is asking — **or not there at all**, and
    /// deliberately the same answer to both. A separate "no such notification"
    /// would let somebody learn which ids exist by the shape of the refusal.
    #[error("no notification of yours has that id")]
    NotYours,
}

impl Localize for NotificationError {
    fn message(&self) -> Message {
        match self {
            Self::NotYours => Message::new(crate::messages::NOT_YOURS),
        }
    }
}

/// Marks one notification read, for one person.
///
/// Reading it twice is nothing rather than an error: the caller wanted it read
/// and it is read, and the instant it was first seen must not move.
pub async fn read(
    db: &TenantDb,
    notification: &AggregateId,
    by: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Read {
    let by = by.to_owned();
    db.execute::<Notification, _, NotificationError>(
        notification,
        crate::upcasters(),
        metadata,
        move |loaded| {
            let held = &loaded.aggregate;
            // **A stranger gets the same answer as a missing id.** The handler
            // turns this into a 404; a 403 would confirm that a notification
            // with this id exists and who it was for.
            if !held.announced || !held.addressed_to(&by) {
                return Err(NotificationError::NotYours);
            }
            if held.already_read_by(&by) {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(NotificationEvent::Read {
                by: by.clone(),
                at,
            }))
        },
    )
    .await
}

/// Marks everything currently in this person's bell as read.
///
/// **One event on the person**, not one per notification: see [`crate::person`]
/// for why it needs no watermark to be exact.
pub async fn read_all(db: &TenantDb, identity: &str, at: Timestamp, metadata: &Metadata) -> Said {
    let identity = identity.to_owned();
    let id = person_id(&identity);
    db.execute::<Person, _, NotificationError>(&id, crate::upcasters(), metadata, move |_| {
        Ok(Decision::one(PersonEvent::ReadAll {
            identity: identity.clone(),
            at,
        }))
    })
    .await
}

/// Replaces this person's whole grid.
///
/// **Whole, not partial.** A patch is how two open tabs produce a setting
/// nobody chose; a kind the caller leaves out goes back to
/// [`crate::DEFAULT_CHANNELS`], which is what "I never said" means everywhere
/// else in this module.
pub async fn set_preferences(
    db: &TenantDb,
    identity: &str,
    entries: BTreeMap<String, Vec<Channel>>,
    at: Timestamp,
    metadata: &Metadata,
) -> Said {
    let identity = identity.to_owned();
    let id = person_id(&identity);
    db.execute::<Person, _, NotificationError>(&id, crate::upcasters(), metadata, move |loaded| {
        if loaded.aggregate.preferences == entries {
            // Saving the same grid again is nothing: a form submitted twice is
            // one statement, not two.
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(PersonEvent::PreferencesSet {
            identity: identity.clone(),
            entries: entries.clone(),
            at,
        }))
    })
    .await
}

/// **Derived from the login** (L8), so a person has one stream whatever route
/// reaches them and nothing has to mint or store a second identifier for it.
#[must_use]
pub fn person_id(identity: &str) -> AggregateId {
    AggregateId::new(
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("notifications.person:{identity}").as_bytes(),
        )
        .to_string(),
    )
    .unwrap_or_else(|_| unreachable!("a uuid satisfies AggregateId"))
}
