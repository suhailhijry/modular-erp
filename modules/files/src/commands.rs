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
    /// The record this would go on is not in the log.
    #[error("there is no {0} {1} to attach this to")]
    NoSuchOwner(String, String),
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
            Self::NoSuchOwner(kind, id) => Message::new(crate::messages::NO_SUCH_OWNER)
                .with("kind", MessageArg::text(kind))
                .with("id", MessageArg::text(id)),
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
    // **The record it goes on has to exist.** Filed under an id that parses but
    // names nothing, a document would appear on no page and be erased by no
    // request. Checked against the log, the way every other command that names
    // another module's record checks — and here rather than only in the HTTP
    // handler, because this is the function every caller goes through.
    {
        let mut conn = db.acquire().await?;
        if !owner_exists(&mut conn, owner).await.map_err(|e| {
            erp_tenant::CommandError::Execute(erp_eventlog::ExecuteError::Load(
                erp_eventlog::LoadError::Read(e),
            ))
        })? {
            return Err(erp_tenant::CommandError::Execute(
                erp_eventlog::ExecuteError::Rejected(FileError::NoSuchOwner(
                    owner.kind.as_str().to_owned(),
                    owner.id.to_string(),
                )),
            ));
        }
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

/// Whether the record an attachment names is in the log.
///
/// A stream with at least one event is a record that was created; `files` does
/// not know or care what state it is in — a document can go on a cancelled
/// invoice, and often should. The tenant itself always exists.
pub async fn owner_exists(
    conn: &mut sqlx::PgConnection,
    owner: &Owner,
) -> Result<bool, erp_eventlog::ReadError> {
    let Some(domain) = owner.kind.domain() else {
        return Ok(true);
    };
    let stream = erp_types::StreamId::new(crate::domain(domain), owner.id.clone());
    Ok(!erp_eventlog::read_stream(conn, &stream).await?.is_empty())
}
