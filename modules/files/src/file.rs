//! A document, and what it is attached to.
//!
//! # The event holds a key, never a URL
//!
//! **A URL is where a file is today; a key is what it is.** A tenant who moves
//! from local disk to object storage, or from one bucket to another, has not
//! changed any of their documents — and an event log full of
//! `https://…/bucket-2019/…` would say otherwise for ever, in a record that
//! cannot be edited.
//!
//! So the event carries `(engine, key, checksum, size, media_type)`, which is
//! `erp_storage::Stored`. Turning that into somewhere a browser can fetch is a
//! read-time concern in the handler that already knows who is asking.
//!
//! # Why the owner is an opaque pair
//!
//! An attachment belongs to an invoice, a booking, an employee record, a
//! journal entry or a customer. Modelling that as five event types, or as five
//! dependencies so the id could be validated, would make attaching to the sixth
//! thing a change here. It is a `(kind, id)` pair, and the sixth thing is a new
//! value of an existing enum.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// What a document can be attached to.
///
/// **Not an exhaustive list of everything in the system** — it is the list of
/// things somebody has wanted to attach a document to. A seventh is one more
/// value here and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Invoice,
    Bill,
    Reservation,
    Customer,
    Employee,
    /// A journal entry — a scanned receipt behind a manual posting.
    Entry,
    /// The business itself: a licence, a certificate, a logo.
    Tenant,
}

impl OwnerKind {
    pub const ALL: [Self; 7] = [
        Self::Invoice,
        Self::Bill,
        Self::Reservation,
        Self::Customer,
        Self::Employee,
        Self::Entry,
        Self::Tenant,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invoice => "invoice",
            Self::Bill => "bill",
            Self::Reservation => "reservation",
            Self::Customer => "customer",
            Self::Employee => "employee",
            Self::Entry => "entry",
            Self::Tenant => "tenant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not something a document can be attached to")]
pub struct UnknownOwner(pub String);

impl std::str::FromStr for OwnerKind {
    type Err = UnknownOwner;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownOwner(s.to_owned()))
    }
}

/// What a document is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub kind: OwnerKind,
    /// Opaque here. The module that owns the meaning owns the id — the same
    /// arrangement `occupancy_resource` has with `booking`.
    pub id: AggregateId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileEvent {
    /// The bytes are in storage and this says where.
    ///
    /// **Written after the bytes, never before.** An orphaned object is wasted
    /// space somebody can sweep; a record pointing at bytes that were never
    /// written is a document that cannot be opened and no way to tell why.
    Stored {
        name: String,
        owner: Owner,
        /// The engine, the key, the checksum, the size and the declared type.
        stored: erp_storage::Stored,
        at: Timestamp,
    },
    /// Detached. **Not a delete**: a document that was on an invoice is part of
    /// what happened, and this is the record that it was taken off.
    ///
    /// Whether the bytes go too is a separate decision and a separate act —
    /// see [`crate::commands::detach`].
    Removed {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        why: String,
        at: Timestamp,
    },
}

impl FileEvent {
    pub const NAMES: [&'static str; 2] = ["files.file.stored", "files.file.removed"];
}

impl DomainEvent for FileEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Stored { .. } => Self::NAMES[0],
            Self::Removed { .. } => Self::NAMES[1],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a document before deciding.
#[derive(Debug, Default, Clone)]
pub struct File {
    pub stored: Option<erp_storage::Stored>,
    pub owner: Option<Owner>,
    pub removed: bool,
}

impl Aggregate for File {
    type Event = FileEvent;

    fn domain() -> DomainName {
        crate::domain("files_file")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            FileEvent::Stored { stored, owner, .. } => {
                self.stored = Some(stored.clone());
                self.owner = Some(owner.clone());
                self.removed = false;
            }
            FileEvent::Removed { .. } => self.removed = true,
        }
    }
}

impl File {
    /// Whether this document exists at all.
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.stored.is_some()
    }
}
