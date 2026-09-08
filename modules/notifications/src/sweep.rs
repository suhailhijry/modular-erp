//! Announcing a whole window of things, which is what every producer does.
//!
//! # Why this is here and not in each job
//!
//! The four producers differ only in what they read: new bookings, settled
//! payments, refused documents, expiring papers. What they do with it — skip
//! what has been said, announce the rest, one transaction each, carry on past a
//! refusal — is the same loop four times, and a loop written four times is
//! three chances to get the transaction boundary wrong.
//!
//! It also puts the loop somewhere a test can reach. A job in the composition
//! root is a binary; this is a function.

use erp_tenant::TenantDb;
use erp_types::{AggregateId, Timestamp};
use messaging::Subject;

use crate::{AnnounceError, Announcing, Kind};

/// What one sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// Notifications actually written.
    pub announced: usize,
    /// Already said, so nothing was written. The ordinary case for a window
    /// that overlaps the last one.
    pub skipped: usize,
    /// Nobody with a login is listed to hear about it — most often an org chart
    /// where nobody has been linked to a login yet.
    pub unreachable: usize,
}

/// Why a whole sweep stopped.
///
/// **Not why one subject was not announced** — those are counted in [`Swept`]
/// and logged, because a window is a batch of unrelated facts and one that
/// cannot be told is no reason to withhold the rest.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Pool(#[from] erp_tenant::PoolError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// Announces one kind about many subjects.
///
/// **One transaction per subject**, so a refusal about one booking does not
/// roll back the notification about another — the same shape
/// `BookingReminders` uses, and for the same reason.
///
/// Nothing here is fatal. A producer sweeps a window every tick and the window
/// overlaps the last one by design; the interesting outcomes are counted and
/// returned rather than raised.
pub async fn announce_all(
    db: &TenantDb,
    kind: Kind,
    subjects: &[AggregateId],
    at: Timestamp,
) -> Result<Swept, SweepError> {
    let mut swept = Swept::default();
    if subjects.is_empty() {
        return Ok(swept);
    }

    // **A cheap first pass, not the guard.** The aggregate refuses a repeat
    // outright, so this only saves opening a transaction per row. It reads a
    // projection and can lag, and being wrong costs one wasted attempt.
    let known: Vec<String> = subjects.iter().map(|s| s.as_str().to_owned()).collect();
    let already = {
        let mut conn = db.read().await?;
        crate::announced_subjects(&mut conn, kind, &known).await?
    };

    for subject in subjects {
        if already.contains(subject.as_str()) {
            swept.skipped += 1;
            continue;
        }

        let announcing = Announcing {
            kind,
            subject: Subject::new(kind.topic(), subject.clone()),
            at,
        };
        let mut tx = db.begin().await?;
        match crate::announce(&mut tx, &announcing, &erp_eventlog::Metadata::default()).await {
            Ok(announced) => {
                tx.commit().await?;
                if announced.announced {
                    swept.announced += 1;
                } else {
                    swept.skipped += 1;
                }
            }
            Err(AnnounceError::Unreachable { .. }) => {
                let _ = tx.rollback().await;
                swept.unreachable += 1;
                tracing::debug!(
                    kind = kind.as_str(),
                    subject = subject.as_str(),
                    "nobody with a login is listed to hear about this"
                );
            }
            Err(error) => {
                let _ = tx.rollback().await;
                // **Not fatal, and not silent.** One subject that cannot be
                // announced — a template that will not render, a budget that is
                // spent — is a fact about that row, and the rest of the window
                // is still worth telling somebody about.
                tracing::warn!(
                    kind = kind.as_str(),
                    subject = subject.as_str(),
                    %error,
                    "this one could not be announced"
                );
            }
        }
    }

    Ok(swept)
}
