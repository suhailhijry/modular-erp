//! What a caller can ask of a branch.
//!
//! Nothing here posts. A branch is a **dimension**, not a transaction: opening
//! one moves no money, and the entries that carry it are written by whichever
//! module wrote the document.

use erp_eventlog::{Decision, ExecuteError, Metadata};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, Timestamp};

use crate::branch::{BadBranch, Branch, BranchEvent, Details};

#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error("there is no branch {0}")]
    NoSuchBranch(String),
    #[error("branch {0} is closed")]
    Closed(String),
    #[error(transparent)]
    Details(#[from] BadBranch),
}

impl erp_i18n::Localize for BranchError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchBranch(id) => {
                Message::new(messages::NO_SUCH_BRANCH).with("id", MessageArg::text(id))
            }
            Self::Closed(id) => Message::new(messages::CLOSED).with("id", MessageArg::text(id)),
            Self::Details(BadBranch::NoName) => Message::new(messages::NO_NAME),
            Self::Details(BadBranch::NoAddress) => Message::new(messages::NO_ADDRESS),
            Self::Details(BadBranch::NotACountry(code)) => {
                Message::new(messages::NOT_A_COUNTRY).with("country", MessageArg::text(code))
            }
        }
    }
}

type Outcome = Result<erp_eventlog::Committed<BranchEvent>, CommandError<BranchError>>;

/// Opens a place to trade from.
pub async fn open_branch(
    db: &TenantDb,
    id: &AggregateId,
    details: &Details,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    details.check().map_err(|e| rejected(e.into()))?;
    let details = details.clone();

    db.create::<Branch, _, BranchError>(id, crate::upcasters(), metadata, move |_loaded| {
        Ok(Decision::one(BranchEvent::Opened {
            name: details.name.trim().to_owned(),
            name_latin: details.name_latin.clone(),
            address: details.address.clone(),
            at,
        }))
    })
    .await
}

/// Changes what is known about a branch.
///
/// A no-op when nothing moved, so a settings form saved twice writes one event —
/// which is what keeps a branch's history readable: every event in it is a
/// change somebody made.
pub async fn amend_branch(
    db: &TenantDb,
    id: &AggregateId,
    details: &Details,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    details.check().map_err(|e| rejected(e.into()))?;
    let details = details.clone();
    let key = id.to_string();

    db.execute::<Branch, _, BranchError>(id, crate::upcasters(), metadata, move |loaded| {
        let held = &loaded.aggregate;
        if !held.exists() {
            return Err(BranchError::NoSuchBranch(key.clone()));
        }
        if held.details().as_ref() == Some(&details) {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(BranchEvent::Amended {
            name: details.name.trim().to_owned(),
            name_latin: details.name_latin.clone(),
            address: details.address.clone(),
            at,
        }))
    })
    .await
}

/// Stops a branch trading, keeping everything it traded.
pub async fn close_branch(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let why = why.to_owned();
    let key = id.to_string();

    db.execute::<Branch, _, BranchError>(id, crate::upcasters(), metadata, move |loaded| {
        let held = &loaded.aggregate;
        if !held.exists() {
            return Err(BranchError::NoSuchBranch(key.clone()));
        }
        if held.closed_at.is_some() {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(BranchEvent::Closed {
            why: why.clone(),
            at,
        }))
    })
    .await
}

/// Puts a closed branch back into service.
pub async fn reopen_branch(
    db: &TenantDb,
    id: &AggregateId,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    let key = id.to_string();

    db.execute::<Branch, _, BranchError>(id, crate::upcasters(), metadata, move |loaded| {
        let held = &loaded.aggregate;
        if !held.exists() {
            return Err(BranchError::NoSuchBranch(key.clone()));
        }
        if held.closed_at.is_none() {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(BranchEvent::Reopened { at }))
    })
    .await
}

/// **Whether a document may be dated to this branch**, answered from the log.
///
/// Against the log and not `proj_branches.branch`, for the reason
/// `crm::accepts_documents` is: a branch opened a moment ago is not in that
/// table yet, and validating against it would refuse an invoice for a place the
/// caller has just created. Reading another module's *write* side is not the
/// cross-group read L3 forbids — that law is about projection groups, and the
/// event log is shared by design.
pub async fn accepts_documents(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
) -> Result<bool, erp_eventlog::LoadError> {
    let loaded = erp_eventlog::load::<Branch>(conn, id, crate::upcasters()).await?;
    Ok(loaded.aggregate.accepts_documents())
}

fn rejected(e: BranchError) -> CommandError<BranchError> {
    CommandError::Execute(ExecuteError::Rejected(e))
}
