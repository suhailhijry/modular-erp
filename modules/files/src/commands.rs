//! Attaching a document, and taking one off.

use erp_eventlog::{Decision, Metadata};
use erp_i18n::{Localize, Message, MessageArg};
use erp_tenant::TenantDb;
use erp_types::{AggregateId, Timestamp};

use crate::file::{File, FileEvent, Owner};

type Outcome = Result<erp_eventlog::Committed<FileEvent>, erp_tenant::CommandError<FileError>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("a document needs a name")]
    NoName,
    #[error("no such document: {0}")]
    NoSuchFile(String),
    #[error("{0} has already been taken off")]
    AlreadyRemoved(String),
    #[error(transparent)]
    Storage(#[from] erp_storage::StorageError),
}

impl Localize for FileError {
    fn message(&self) -> Message {
        match self {
            Self::NoName => Message::new(crate::messages::NO_NAME),
            Self::NoSuchFile(id) => {
                Message::new(crate::messages::NO_SUCH_FILE).with("id", MessageArg::text(id))
            }
            Self::AlreadyRemoved(id) => {
                Message::new(crate::messages::ALREADY_REMOVED).with("id", MessageArg::text(id))
            }
            Self::Storage(e) => e.message(),
        }
    }
}

/// The longest a name may be. A filename, not a description.
const MAX_NAME: usize = 200;

/// Records a document that is **already in storage**.
///
/// # The order, and why it is this way round
///
/// The bytes go first and the event second. An orphaned object is wasted space
/// somebody can sweep; a record pointing at bytes that were never written is a
/// document that cannot be opened, with nothing to say why. The caller stores,
/// then calls this.
///
/// # Idempotency
///
/// `create`, so a second attach under a taken id is refused unless it is a
/// retry of the request that made it (L8). A retried upload therefore stores
/// the same bytes under the same key — which is a rewrite of identical
/// content — and records nothing twice.
pub async fn attach(
    db: &TenantDb,
    id: &AggregateId,
    name: &str,
    owner: &Owner,
    stored: &erp_storage::Stored,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(erp_tenant::CommandError::Execute(
            erp_eventlog::ExecuteError::Rejected(FileError::NoName),
        ));
    }
    let owner = owner.clone();
    let stored = stored.clone();

    db.create::<File, _, FileError>(id, crate::upcasters(), metadata, move |_loaded| {
        Ok(Decision::one(FileEvent::Stored {
            name: name.clone(),
            owner: owner.clone(),
            stored: stored.clone(),
            at,
        }))
    })
    .await
}

/// Takes a document off what it was attached to.
///
/// **The bytes are not touched.** Two reasons, and the second is the one that
/// matters: a document that was on an invoice is part of what happened, and
/// erasing it on a click would erase evidence. Removing the bytes as well is a
/// separate act with its own authority — the same argument
/// `crm::archive_customer` makes about never deleting a customer.
///
/// Idempotent: taking off a document that is already off is refused rather than
/// written twice, because the second call is either a retry (nothing to do) or
/// a mistake (worth saying so).
pub async fn detach(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let why = why.trim().to_owned();
    let key = id.to_string();

    db.execute::<File, _, FileError>(id, crate::upcasters(), metadata, move |loaded| {
        if !loaded.aggregate.exists() {
            return Err(FileError::NoSuchFile(key.clone()));
        }
        if loaded.aggregate.removed {
            // Already off. The caller wanted it off and it is off, so this is
            // nothing rather than an error — the same call `archive_customer`
            // makes.
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(FileEvent::Removed {
            why: why.clone(),
            at,
        }))
    })
    .await
}
