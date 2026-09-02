//! What a caller can ask of the org chart.
//!
//! # Every one of these opens its own transaction, and that is the point
//!
//! `TenantDb::execute` would be enough if an org event touched one aggregate
//! and nothing else. It does not: hiring somebody writes the event *and* the
//! reporting line *and* the recomputed claim union, and a hire that landed in
//! the log without reaching the tree would be a person the authorization model
//! could not see.
//!
//! So the shape is the one `sales` uses for an invoice and its journal entry:
//! `db.begin()`, the event and the derived state together, one commit.
//!
//! # Nothing here reads a projection
//!
//! The branch is checked against `branches`' **log**, the same way `sales`
//! checks a customer against `crm`'s — a branch opened a moment ago is not in
//! `proj_branches` yet, and refusing to hire somebody into it would be wrong.
//! The manager is checked against this module's own log for the same reason.

use erp_eventlog::{
    Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, load, try_create,
    try_execute,
};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, Timestamp};

use crate::claims::{Claim, ClaimError};
use crate::employee::{BadEmployee, Employee, EmployeeEvent};

#[derive(Debug, thiserror::Error)]
pub enum HrError {
    #[error("there is no employee {0}")]
    NoSuchEmployee(String),
    #[error("there is no employee {0} to report to")]
    NoSuchManager(String),
    #[error("there is no open branch {0}")]
    NoSuchBranch(String),
    #[error("employee {0} has left")]
    Left(String),
    #[error(transparent)]
    Details(#[from] BadEmployee),
    #[error(transparent)]
    Claims(#[from] ClaimError),
}

impl erp_i18n::Localize for HrError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchEmployee(id) => {
                Message::new(messages::NO_SUCH_EMPLOYEE).with("id", MessageArg::text(id))
            }
            Self::NoSuchManager(id) => {
                Message::new(messages::NO_SUCH_MANAGER).with("id", MessageArg::text(id))
            }
            Self::NoSuchBranch(id) => {
                Message::new(messages::NO_SUCH_BRANCH).with("branch", MessageArg::text(id))
            }
            Self::Left(id) => Message::new(messages::LEFT).with("id", MessageArg::text(id)),
            Self::Details(BadEmployee::NoName) => Message::new(messages::NO_NAME),
            Self::Details(BadEmployee::NoContact) => Message::new(messages::NO_CONTACT),
            Self::Claims(e) => e.message(),
        }
    }
}

type Refusal = CommandError<HrError>;
type Outcome = Result<Committed<EmployeeEvent>, Refusal>;

/// Everything needed to put somebody on the books.
#[derive(Debug, Clone)]
pub struct Hire {
    pub details: crate::employee::Details,
    /// Who they report to. `None` makes them the root.
    pub reports_to: Option<AggregateId>,
    /// Where they work. `None` for a company-wide role, or a single-branch
    /// business that has never opened a second one.
    pub branch: Option<AggregateId>,
    pub at: Timestamp,
}

/// Puts somebody on the books, and into the chart.
pub async fn hire(db: &TenantDb, id: &AggregateId, hiring: &Hire, metadata: &Metadata) -> Outcome {
    hiring.details.check().map_err(|e| rejected(e.into()))?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_branch(&mut *conn, hiring.branch.as_ref()).await?;
            check_manager(&mut *conn, hiring.reports_to.as_ref()).await?;

            let committed = try_create::<Employee, _, HrError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |_loaded: &Loaded<Employee>| {
                    Ok(Decision::one(EmployeeEvent::Hired {
                        name: hiring.details.name.trim().to_owned(),
                        name_latin: hiring.details.name_latin.clone(),
                        national_id: hiring.details.national_id.clone(),
                        email: hiring.details.email.clone(),
                        phone: hiring.details.phone.clone(),
                        reports_to: hiring.reports_to.clone(),
                        branch: hiring.branch.clone(),
                        at: hiring.at,
                    }))
                },
            )
            .await?;

            // **The tree is written whether or not the event was.** A retry
            // reports the original hire and writes nothing to the log, and the
            // line it should have created is already there — but writing it
            // again is idempotent and costs one upsert, which is cheaper than a
            // branch that could be wrong.
            crate::claims::place(
                &mut *conn,
                id,
                hiring.reports_to.as_ref(),
                hiring.branch.as_ref(),
            )
            .await
            .map_err(|e| ExecuteError::Rejected(HrError::Claims(e)))?;

            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Changes what is known about somebody. **Never their reporting line.**
pub async fn amend_employee(
    db: &TenantDb,
    id: &AggregateId,
    details: &crate::employee::Details,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    details.check().map_err(|e| rejected(e.into()))?;
    let details = details.clone();

    db.execute::<Employee, _, HrError>(id, crate::upcasters(), metadata, move |loaded| {
        if !loaded.aggregate.exists() {
            return Err(HrError::NoSuchEmployee(id.to_string()));
        }
        Ok(Decision::one(EmployeeEvent::Amended {
            name: details.name.trim().to_owned(),
            name_latin: details.name_latin.clone(),
            national_id: details.national_id.clone(),
            email: details.email.clone(),
            phone: details.phone.clone(),
            at,
        }))
    })
    .await
}

/// Moves somebody in the chart.
///
/// **Its own command and its own event**, because it moves everything they
/// carry: every claim in their subtree stops reaching their old manager and
/// starts reaching their new one.
pub async fn reparent(
    db: &TenantDb,
    id: &AggregateId,
    reports_to: Option<&AggregateId>,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_manager(&mut *conn, reports_to).await?;

            let held = load::<Employee>(&mut *conn, id, crate::upcasters())
                .await
                .map_err(ExecuteError::Load)?;
            if !held.aggregate.exists() {
                return Err(ExecuteError::Rejected(HrError::NoSuchEmployee(
                    id.to_string(),
                )));
            }

            let committed = try_execute::<Employee, _, HrError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded| {
                    // Moving somebody to where they already are is a no-op, not
                    // an error: a retried request must not look like a failure.
                    if loaded.aggregate.reports_to.as_ref() == reports_to {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(EmployeeEvent::Reparented {
                        reports_to: reports_to.cloned(),
                        why: why.trim().to_owned(),
                        at,
                    }))
                },
            )
            .await?;

            crate::claims::place(&mut *conn, id, reports_to, held.aggregate.branch.as_ref())
                .await
                .map_err(|e| ExecuteError::Rejected(HrError::Claims(e)))?;

            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Moves somebody to another branch.
pub async fn transfer(
    db: &TenantDb,
    id: &AggregateId,
    branch: Option<&AggregateId>,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            check_branch(&mut *conn, branch).await?;

            let held = load::<Employee>(&mut *conn, id, crate::upcasters())
                .await
                .map_err(ExecuteError::Load)?;
            if !held.aggregate.exists() {
                return Err(ExecuteError::Rejected(HrError::NoSuchEmployee(
                    id.to_string(),
                )));
            }

            let committed = try_execute::<Employee, _, HrError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded| {
                    if loaded.aggregate.branch.as_ref() == branch {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(EmployeeEvent::Transferred {
                        branch: branch.cloned(),
                        at,
                    }))
                },
            )
            .await?;

            crate::claims::place(&mut *conn, id, held.aggregate.reports_to.as_ref(), branch)
                .await
                .map_err(|e| ExecuteError::Rejected(HrError::Claims(e)))?;

            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Records that somebody has left.
///
/// **Their claims stop, and their record does not.** They are on last year's
/// payroll and whatever they approved; what ends is authority, which is the
/// half that must end the moment they walk out.
///
/// Their reports keep reporting to them until somebody moves them, which is
/// deliberate: silently re-parenting a whole team to a departed manager's
/// manager is a decision the business makes, not one a resignation makes.
pub async fn record_leaving(
    db: &TenantDb,
    id: &AggregateId,
    why: &str,
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let committed = try_execute::<Employee, _, HrError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded| {
                    if !loaded.aggregate.exists() {
                        return Err(HrError::NoSuchEmployee(id.to_string()));
                    }
                    if !loaded.aggregate.is_employed() {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(EmployeeEvent::Left {
                        why: why.trim().to_owned(),
                        at,
                    }))
                },
            )
            .await?;

            crate::claims::withdraw(&mut *conn, id)
                .await
                .map_err(|e| ExecuteError::Rejected(HrError::Claims(e)))?;

            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Grants a claim, and reports **everyone who gained it**.
///
/// The second half is what the granting screen shows. See [`crate::claims`].
pub async fn grant_claim(
    db: &TenantDb,
    id: &AggregateId,
    claim: &Claim,
    propagates: bool,
) -> Result<Vec<String>, Refusal> {
    let mut tx = db.begin().await?;
    let conn = &mut *tx;

    let held = load::<Employee>(conn, id, crate::upcasters())
        .await
        .map_err(|e| CommandError::Execute(ExecuteError::<HrError>::Load(e)))?;
    if !held.aggregate.is_employed() {
        tx.rollback().await.map_err(ExecuteError::from)?;
        return Err(rejected(HrError::NoSuchEmployee(id.to_string())));
    }

    let gained = crate::claims::grant(conn, id, claim, propagates)
        .await
        .map_err(|e| rejected(HrError::Claims(e)))?;
    tx.commit().await.map_err(ExecuteError::from)?;
    Ok(gained)
}

/// Takes a claim back, and reports everyone who lost it.
pub async fn revoke_claim(
    db: &TenantDb,
    id: &AggregateId,
    claim: &Claim,
) -> Result<Vec<String>, Refusal> {
    let mut tx = db.begin().await?;
    let lost = crate::claims::revoke(&mut tx, id, claim)
        .await
        .map_err(|e| rejected(HrError::Claims(e)))?;
    tx.commit().await.map_err(ExecuteError::from)?;
    Ok(lost)
}

// -------------------------------------------------------------------- checks

/// Refuses a branch that is not open, against the **log**.
///
/// Not `post_entry_in`'s check, and not inherited from it: hiring somebody
/// posts nothing, so there is no journal entry to carry it. Same position
/// `booking` is in when it declares a resource.
async fn check_branch(
    conn: &mut sqlx::PgConnection,
    branch: Option<&AggregateId>,
) -> Result<(), ExecuteError<HrError>> {
    let Some(branch) = branch else { return Ok(()) };
    if branches::accepts_documents(&mut *conn, branch)
        .await
        .map_err(ExecuteError::Load)?
    {
        return Ok(());
    }
    Err(ExecuteError::Rejected(HrError::NoSuchBranch(
        branch.to_string(),
    )))
}

/// Refuses a manager who is not there, or who has left.
///
/// A departed manager is refused rather than allowed, because reporting to
/// somebody who no longer works here is not a state anybody means to create —
/// and under the union it would park a subtree's claims on a person who has
/// none.
async fn check_manager(
    conn: &mut sqlx::PgConnection,
    manager: Option<&AggregateId>,
) -> Result<(), ExecuteError<HrError>> {
    let Some(manager) = manager else {
        return Ok(());
    };
    let held = load::<Employee>(&mut *conn, manager, crate::upcasters())
        .await
        .map_err(ExecuteError::Load)?;
    if held.aggregate.is_employed() {
        return Ok(());
    }
    Err(ExecuteError::Rejected(HrError::NoSuchManager(
        manager.to_string(),
    )))
}

// ------------------------------------------------------------------ plumbing

/// Commits, rolls back and retries — the one place that decides which.
///
/// Written out here for the reason `booking`, `prepaid` and `pos` write it out:
/// a generic `AsyncFn` helper reads better and does not compile, because axum
/// needs a handler's future to be `Send`.
async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<HrError>>,
) -> Result<Option<T>, Refusal> {
    match outcome {
        Ok(done) => {
            tx.commit().await.map_err(ExecuteError::from)?;
            Ok(Some(done))
        }
        Err(e) if e.is_conflict() => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Ok(None)
        }
        Err(e) => {
            tx.rollback().await.map_err(ExecuteError::from)?;
            Err(e.into())
        }
    }
}

fn rejected(error: HrError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn contended<T>(id: &AggregateId) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: erp_types::StreamId::new(
            <Employee as erp_eventlog::Aggregate>::domain(),
            id.clone(),
        ),
        attempts: MAX_ATTEMPTS,
    }))
}
