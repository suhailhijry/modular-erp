//! What a caller can ask the ledger to do.

use spa_control::{CommandError, TenantDb};
use spa_eventlog::{Committed, Decision, ExecuteError, Metadata};
use spa_i18n::Locale;
use spa_types::{AggregateId, CurrencyCode, Timestamp};

use crate::account::{Account, AccountEvent, AccountKind};
use crate::charts::{Chart, Installed};
use crate::entry::{JournalEntry, JournalEntryEvent};
use crate::lines::{BalancedLines, Unbalanced};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("account {0} already exists")]
    AccountExists(String),
    #[error("no account {0}")]
    NoSuchAccount(String),
    #[error("account {0} is closed")]
    AccountClosed(String),
    #[error("entry {0} has already been posted")]
    AlreadyPosted(String),
    #[error("there is no entry {0}")]
    NoSuchEntry(String),
    #[error("entry {entry} was already reversed by {by}")]
    AlreadyReversed { entry: String, by: String },
    #[error(transparent)]
    Unbalanced(#[from] Unbalanced),
    /// A chart shipped a code that is not a usable identifier. A build bug, not
    /// something a user can cause — `charts::tests` catches it first.
    #[error("{0}")]
    BadAccountCode(String),
}

impl spa_i18n::Localize for LedgerError {
    fn message(&self) -> spa_i18n::Message {
        use crate::messages;
        use spa_i18n::{Message, MessageArg};
        match self {
            Self::AccountExists(code) => {
                Message::new(messages::ACCOUNT_EXISTS).with("code", MessageArg::text(code.clone()))
            }
            Self::NoSuchAccount(code) => {
                Message::new(messages::NO_SUCH_ACCOUNT).with("code", MessageArg::text(code.clone()))
            }
            Self::AccountClosed(code) => {
                Message::new(messages::ACCOUNT_CLOSED).with("code", MessageArg::text(code.clone()))
            }
            // Not an error a user should see as a failure — the entry is there.
            // The route turns this into a 200 with the existing entry.
            Self::AlreadyPosted(_) => Message::new(messages::ALREADY_POSTED),
            Self::NoSuchEntry(id) => {
                Message::new(messages::NO_SUCH_ENTRY).with("entry", MessageArg::text(id.clone()))
            }
            Self::AlreadyReversed { by, .. } => {
                Message::new(messages::ALREADY_REVERSED).with("by", MessageArg::text(by.clone()))
            }
            Self::Unbalanced(e) => e.message(),
            Self::BadAccountCode(_) => Message::new(spa_control::messages::INTERNAL),
        }
    }
}

type Outcome<E> = Result<Committed<E>, CommandError<LedgerError>>;

/// Opens an account.
///
/// Idempotent by refusal, not by silence: re-opening an existing code is an
/// error, because the second caller almost certainly meant a different account.
pub async fn open_account(
    db: &TenantDb,
    code: &AggregateId,
    name: &str,
    kind: AccountKind,
    currency: CurrencyCode,
    metadata: &Metadata,
) -> Outcome<AccountEvent> {
    let name = name.trim().to_owned();
    db.execute::<Account, _, LedgerError>(code, crate::upcasters(), metadata, |loaded| {
        if loaded.aggregate.exists {
            return Err(LedgerError::AccountExists(code.as_str().to_owned()));
        }
        Ok(Decision::one(AccountEvent::Opened {
            name: name.clone(),
            kind,
            currency,
        }))
    })
    .await
}

/// Renames an account. A no-op if the name already matches.
pub async fn rename_account(
    db: &TenantDb,
    code: &AggregateId,
    name: &str,
    metadata: &Metadata,
) -> Outcome<AccountEvent> {
    let name = name.trim().to_owned();
    db.execute::<Account, _, LedgerError>(code, crate::upcasters(), metadata, |loaded| {
        if !loaded.aggregate.exists {
            return Err(LedgerError::NoSuchAccount(code.as_str().to_owned()));
        }
        if loaded.aggregate.name == name {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(AccountEvent::Renamed { name: name.clone() }))
    })
    .await
}

/// Closes an account. Its history stays; new postings are refused.
pub async fn close_account(
    db: &TenantDb,
    code: &AggregateId,
    metadata: &Metadata,
) -> Outcome<AccountEvent> {
    db.execute::<Account, _, LedgerError>(code, crate::upcasters(), metadata, |loaded| {
        if !loaded.aggregate.exists {
            return Err(LedgerError::NoSuchAccount(code.as_str().to_owned()));
        }
        if loaded.aggregate.closed {
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(AccountEvent::Closed))
    })
    .await
}

/// Posts a journal entry.
///
/// # Two checks, in two places
///
/// That the lines balance is [`BalancedLines`]'s job, done before this is
/// called — the type cannot hold an unbalanced set. That every account exists
/// and is open is [`post_entry_in`]'s job, because it needs state the type
/// cannot see.
///
/// Re-posting the same `id` is a no-op, which is what makes a retried request
/// safe without an idempotency table.
pub async fn post_entry(
    db: &TenantDb,
    id: &AggregateId,
    occurred_on: Timestamp,
    memo: &str,
    lines: BalancedLines,
    metadata: &Metadata,
) -> Outcome<JournalEntryEvent> {
    for _ in 1..=spa_eventlog::MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match post_entry_in(&mut tx, id, occurred_on, memo, &lines, metadata).await {
            Ok(committed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(ExecuteError::Contended {
        stream: spa_types::StreamId::new(
            <JournalEntry as spa_eventlog::Aggregate>::domain(),
            id.clone(),
        ),
        attempts: spa_eventlog::MAX_ATTEMPTS,
    }
    .into())
}

/// Posts a journal entry **inside the caller's transaction**, once.
///
/// # Who this is for
///
/// A module that produces its own events and must post alongside them — sales
/// issuing an invoice, and every module after it. Both aggregates land in one
/// transaction, so an invoice that exists without its accounting entry is not a
/// state the system can reach, and nothing has to sweep for one afterwards.
///
/// No retry: the caller owns the transaction, so the caller owns the retry. See
/// `spa_eventlog::try_execute` for why the two cannot be separated.
///
/// The account checks run inside that transaction on purpose. Reading them
/// outside would be marginally cheaper and would let an account be closed
/// between the check and the append.
pub async fn post_entry_in(
    conn: &mut sqlx::PgConnection,
    id: &AggregateId,
    occurred_on: Timestamp,
    memo: &str,
    lines: &BalancedLines,
    metadata: &Metadata,
) -> Result<Committed<JournalEntryEvent>, ExecuteError<LedgerError>> {
    for line in lines.as_slice() {
        let account =
            spa_eventlog::load::<Account>(&mut *conn, &line.account, crate::upcasters()).await?;

        if !account.aggregate.exists {
            return Err(ExecuteError::Rejected(LedgerError::NoSuchAccount(
                line.account.as_str().to_owned(),
            )));
        }
        if !account.aggregate.accepts_postings() {
            return Err(ExecuteError::Rejected(LedgerError::AccountClosed(
                line.account.as_str().to_owned(),
            )));
        }
        if account.aggregate.currency != Some(lines.currency()) {
            return Err(ExecuteError::Rejected(LedgerError::Unbalanced(
                crate::lines::Unbalanced::MixedCurrencies {
                    index: 0,
                    expected: account
                        .aggregate
                        .currency
                        .unwrap_or_else(|| lines.currency()),
                    found: lines.currency(),
                },
            )));
        }
    }

    let memo = memo.trim().to_owned();
    spa_eventlog::try_execute::<JournalEntry, _, LedgerError>(
        conn,
        id,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.posted {
                // Already done. A no-op rather than an error, so a retried
                // request succeeds instead of confusing the caller.
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(JournalEntryEvent::Posted {
                occurred_on,
                memo: memo.clone(),
                lines: lines.clone(),
            }))
        },
    )
    .await
}

/// Undoes an entry by posting its opposite.
///
/// # Why accounting does not delete
///
/// A posted entry is a statement about what happened, and someone may have
/// filed a return against it. Correcting one means saying something *else* —
/// the same lines with the signs flipped, on a date of its own — so the books
/// show both the mistake and the correction. Deleting it would silently restate
/// a period that has already been reported.
///
/// # What happens, in one transaction
///
/// The opposite entry is posted under `reversal`, and the original records that
/// it was reversed and by what. Both, or neither: an entry marked reversed with
/// no reversal to show for it is a hole in the trial balance, and a reversal
/// with nothing marked is a double-count.
///
/// Re-running with the same `reversal` id is a no-op, so a retried request is
/// safe. Re-running with a *different* one is refused, because an entry that
/// has already been undone cannot be undone again — the second attempt would
/// swing the balance the other way.
pub async fn reverse_entry(
    db: &TenantDb,
    original: &AggregateId,
    reversal: &AggregateId,
    occurred_on: Timestamp,
    memo: &str,
    metadata: &Metadata,
) -> Outcome<JournalEntryEvent> {
    for _ in 1..=spa_eventlog::MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        match reverse_in(&mut tx, original, reversal, occurred_on, memo, metadata).await {
            Ok(committed) => {
                tx.commit().await.map_err(ExecuteError::from)?;
                return Ok(committed);
            }
            Err(e) if e.is_conflict() => {
                tx.rollback().await.map_err(ExecuteError::from)?;
            }
            Err(e) => {
                tx.rollback().await.map_err(ExecuteError::from)?;
                return Err(e.into());
            }
        }
    }

    Err(ExecuteError::Contended {
        stream: spa_types::StreamId::new(
            <JournalEntry as spa_eventlog::Aggregate>::domain(),
            original.clone(),
        ),
        attempts: spa_eventlog::MAX_ATTEMPTS,
    }
    .into())
}

/// One attempt at reversing, in the caller's transaction.
///
/// Public for the same reason [`post_entry_in`] is: a module that reverses its
/// own document — a credit note — has to do it alongside its own events.
pub async fn reverse_in(
    conn: &mut sqlx::PgConnection,
    original: &AggregateId,
    reversal: &AggregateId,
    occurred_on: Timestamp,
    memo: &str,
    metadata: &Metadata,
) -> Result<Committed<JournalEntryEvent>, ExecuteError<LedgerError>> {
    let loaded =
        spa_eventlog::load::<JournalEntry>(&mut *conn, original, crate::upcasters()).await?;

    if !loaded.aggregate.posted {
        return Err(ExecuteError::Rejected(LedgerError::NoSuchEntry(
            original.as_str().to_owned(),
        )));
    }

    // A retry, not a second reversal.
    if loaded.aggregate.reversed_by.as_deref() == Some(reversal.as_str()) {
        return Ok(Committed {
            events: Vec::new(),
            at: None,
            version: loaded.version,
            effects_enqueued: 0,
        });
    }
    if let Some(by) = loaded.aggregate.reversed_by {
        return Err(ExecuteError::Rejected(LedgerError::AlreadyReversed {
            entry: original.as_str().to_owned(),
            by,
        }));
    }

    let lines = loaded.aggregate.lines.ok_or_else(|| {
        ExecuteError::Rejected(LedgerError::NoSuchEntry(original.as_str().to_owned()))
    })?;

    // Negating a balanced set leaves it balanced, so this cannot produce an
    // entry the ledger would refuse — but it goes through `BalancedLines::new`
    // anyway, because the type's guarantee is worth more than the shortcut.
    let flipped = lines
        .as_slice()
        .iter()
        .map(|line| {
            Ok(crate::lines::Line {
                account: line.account.clone(),
                amount: line
                    .amount
                    .checked_neg()
                    .map_err(|e| ExecuteError::Rejected(LedgerError::Unbalanced(e.into())))?,
                memo: line.memo.clone(),
            })
        })
        .collect::<Result<Vec<_>, ExecuteError<LedgerError>>>()?;

    let flipped = BalancedLines::new(flipped)
        .map_err(|e| ExecuteError::Rejected(LedgerError::Unbalanced(e)))?;

    post_entry_in(&mut *conn, reversal, occurred_on, memo, &flipped, metadata).await?;

    let by = reversal.as_str().to_owned();
    spa_eventlog::try_execute::<JournalEntry, _, LedgerError>(
        conn,
        original,
        crate::upcasters(),
        metadata,
        |loaded| {
            if loaded.aggregate.is_reversed() {
                return Ok(Decision::nothing());
            }
            Ok(Decision::one(JournalEntryEvent::Reversed {
                by: by.clone(),
                occurred_on,
            }))
        },
    )
    .await
}

/// Whether an account exists and can take a posting **right now**.
///
/// Reads the log, not `proj_ledger.account`. The read model is driven by a
/// worker and lags, so a chart installed a moment ago is not in it yet —
/// validating against it tells a tenant that the account they just created does
/// not exist. This is the same question `post_entry_in` asks, asked the same
/// way, which is the point: a check that disagrees with the command it is
/// guarding is worse than no check.
pub async fn accepts_postings(
    conn: &mut sqlx::PgConnection,
    code: &AggregateId,
) -> Result<bool, spa_eventlog::LoadError> {
    let account = spa_eventlog::load::<Account>(conn, code, crate::upcasters()).await?;
    Ok(account.aggregate.accepts_postings())
}

fn rejected(error: LedgerError) -> CommandError<LedgerError> {
    CommandError::Execute(spa_eventlog::ExecuteError::Rejected(error))
}

/// Opens every account in a chart that is not already there.
///
/// # Why an existing account is skipped rather than refused
///
/// Installing eighteen accounts is eighteen commands, and the fifteenth can
/// fail. Refusing on the first duplicate would make the retry — the obvious
/// thing to do next — fail immediately and leave the chart half-built forever.
/// Skipping makes this "ensure these accounts exist", which is idempotent, and
/// idempotent is what turns recovery into retry.
///
/// It also means a tenant can install `retail` on top of `services` and get the
/// three accounts it does not already have.
pub async fn install_chart(
    db: &TenantDb,
    chart: &Chart,
    currency: CurrencyCode,
    locale: Locale,
    metadata: &Metadata,
) -> Result<Installed, CommandError<LedgerError>> {
    let mut installed = Installed::default();

    for template in chart.accounts {
        let code = AggregateId::new(template.code)
            .map_err(|e| rejected(LedgerError::BadAccountCode(e.to_string())))?;

        let outcome = open_account(
            db,
            &code,
            template.name(locale),
            template.kind,
            currency,
            metadata,
        )
        .await;

        match outcome {
            Ok(_) => installed.opened += 1,
            Err(CommandError::Execute(ExecuteError::Rejected(LedgerError::AccountExists(_)))) => {
                installed.skipped += 1;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(installed)
}
