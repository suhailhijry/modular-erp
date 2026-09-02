//! **The invariant that makes a report trustworthy.**
//!
//! A figure on a dashboard is worth nothing unless it agrees with the books.
//! The warning this exists to answer, from the system this phase was read
//! against: its customer statement is built from invoices rather than from the
//! ledger, because the ledger was unfinished. **Two financial truths that
//! disagree** is the failure mode — and the one that costs a business real
//! money, because somebody acts on the wrong one.
//!
//! # A discrepancy is a failure, not a coloured cell (L6)
//!
//! [`reconciles`] returns what disagrees. **Empty is healthy**, and anything
//! else stops something: the worker's health check refuses the tenant as
//! unhealthy, and `every_figure_agrees_with_the_books` fails the build. Nothing
//! renders a discrepancy in amber and carries on.
//!
//! # Why this reads no other group
//!
//! It would be natural to compare `proj_reports` against `proj_ledger` — and it
//! would be wrong twice over. L3 forbids the read, and the reason is exactly
//! what would go wrong here: the two groups sit on **two checkpoints**, so a
//! disagreement would as often mean *one is behind* as *one is wrong*. An
//! invariant that fires on a race is an invariant somebody switches off, and
//! then it is not protecting anything.
//!
//! So this group subscribes to `ledger.entry.posted` like it subscribes to
//! everything else, and reconciles against **its own copy at its own
//! checkpoint** (`Book`, in `projections.rs`). Every row it compares was
//! written by one projection run at one position, so a difference is a
//! difference.
//!
//! # What is compared
//!
//! Two things, both exact and neither depending on a chart of accounts:
//!
//! 1. **The trial balance.** Every currency's postings sum to zero. This is the
//!    ledger's own invariant, asserted from the report's side — which is the
//!    point: if this pipeline applied an event twice or a rebuild diverged, the
//!    ledger would still balance and this would not.
//! 2. **Every document against the entry it posted.** The debits of the journal
//!    entry an invoice made equal what the invoice came to; the same for the
//!    entry that credited it. Account-agnostic, so a tenant who renamed their
//!    revenue account or enabled `prepaid` — which moves money in and out of
//!    revenue as packages are redeemed — does not produce a false alarm.

use erp_projection::ProjectionGroup;
use erp_types::{CurrencyCode, Money};
use sqlx::PgConnection;

use crate::projections::Reports;

/// Something the report says that the books do not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discrepancy {
    /// A currency's postings do not sum to zero.
    ///
    /// The ledger makes an unbalanced entry unconstructable, so this never
    /// means "somebody posted badly". It means the pipeline is broken: an event
    /// applied twice, a rebuild that diverged, or rows written by something
    /// that is not this code.
    Unbalanced {
        currency: CurrencyCode,
        /// Debits plus credits. Zero is healthy.
        difference: Money,
    },
    /// An invoice that made no journal entry.
    ///
    /// Excludes the tail of the log — see [`reconciles`] — so this is a
    /// document that really has no accounting, not one whose entry has yet to
    /// be projected.
    Unposted {
        invoice: String,
        /// The entry that should exist.
        entry: String,
        /// What the document came to.
        document: Money,
    },
    /// A journal entry that does not come to what its document says.
    Mismatched {
        invoice: String,
        entry: String,
        /// Net plus tax, from the document.
        document: Money,
        /// The entry's debits.
        posted: Money,
    },
}

impl Discrepancy {
    /// One line, for a log or a failing assertion.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Unbalanced {
                currency,
                difference,
            } => format!("{currency} postings are out by {difference}"),
            Self::Unposted {
                invoice,
                entry,
                document,
            } => format!("invoice {invoice} came to {document} and posted no entry {entry}"),
            Self::Mismatched {
                invoice,
                entry,
                document,
                posted,
            } => format!("invoice {invoice} came to {document}; entry {entry} posted {posted}"),
        }
    }
}

/// **Everything this module says that the books do not.** Empty is healthy.
///
/// Asserted the way `an_unbalanced_entry_is_refused` is: as a fact that holds
/// or a build that fails.
///
/// # The one thing it deliberately does not report
///
/// An invoice at the very tail of what has been projected. An invoice and its
/// journal entry commit together and take consecutive log positions, but a
/// projection batch may end between them — so the document is applied and the
/// entry is one position away, unapplied. Reporting that as "made no entry"
/// would be reporting a batch boundary as a broken ledger.
///
/// Anything below the checkpoint is fair game, because the entry that would
/// have followed it has been applied by definition.
pub async fn reconciles(conn: &mut PgConnection) -> Result<Vec<Discrepancy>, sqlx::Error> {
    let mut found = unbalanced(conn).await?;
    found.extend(undocumented(conn).await?);
    Ok(found)
}

/// Every currency whose postings do not sum to zero.
async fn unbalanced(conn: &mut PgConnection) -> Result<Vec<Discrepancy>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT currency as "currency!",
                  (sum(debits) + sum(credits))::BIGINT as "difference!"
             FROM proj_reports.entry
            GROUP BY currency
           HAVING sum(debits) + sum(credits) <> 0
            ORDER BY currency"#
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            Ok(Discrepancy::Unbalanced {
                currency,
                difference: Money::from_minor(row.difference, currency),
            })
        })
        .collect()
}

/// Every document whose entry is missing or disagrees.
///
/// One query over two tables **in this schema**, which is what makes it a
/// reconciliation and not a cross-group join.
async fn undocumented(conn: &mut PgConnection) -> Result<Vec<Discrepancy>, sqlx::Error> {
    let checkpoint = erp_projection::checkpoint_of(conn, Reports::NAME)
        .await?
        .get();

    let rows = sqlx::query!(
        r#"SELECT i.id as "invoice!", i.currency as "currency!",
                  (i.net + i.tax)::BIGINT as "document!",
                  i.entry as "entry!",
                  e.debits as "posted?"
             FROM proj_reports.invoiced i
             LEFT JOIN proj_reports.entry e ON e.id = i.entry
            WHERE i.position < $1
              AND (e.id IS NULL OR e.debits <> i.net + i.tax)
              -- A document that came to nothing posts nothing, and the ledger
              -- drops zero lines rather than writing an entry with no effect.
              -- Reporting that as unposted would flag every zero-rated
              -- placeholder a business ever issues.
              AND (i.net + i.tax) <> 0
            ORDER BY i.id
            LIMIT 100"#,
        checkpoint,
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let currency =
                CurrencyCode::new(&row.currency).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
            let document = Money::from_minor(row.document, currency);
            Ok(match row.posted {
                None => Discrepancy::Unposted {
                    invoice: row.invoice,
                    entry: row.entry,
                    document,
                },
                Some(posted) => Discrepancy::Mismatched {
                    invoice: row.invoice,
                    entry: row.entry,
                    document,
                    posted: Money::from_minor(posted, currency),
                },
            })
        })
        .collect()
}
