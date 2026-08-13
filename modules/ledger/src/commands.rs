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
/// and is open is *this* function's job, because it needs state the type cannot
/// see.
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
    // Read once, outside the retry loop: accounts change far more slowly than
    // entries are posted, and re-reading them on every attempt would triple the
    // cost of contention for nothing.
    let mut conn = db.acquire().await?;
    for line in lines.as_slice() {
        let account = spa_eventlog::load::<Account>(&mut conn, &line.account, crate::upcasters())
            .await
            .map_err(|e| CommandError::Execute(e.into()))?;

        if !account.aggregate.exists {
            return Err(rejected(LedgerError::NoSuchAccount(
                line.account.as_str().to_owned(),
            )));
        }
        if !account.aggregate.accepts_postings() {
            return Err(rejected(LedgerError::AccountClosed(
                line.account.as_str().to_owned(),
            )));
        }
        if account.aggregate.currency != Some(lines.currency()) {
            return Err(rejected(LedgerError::Unbalanced(
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
    drop(conn);

    let memo = memo.trim().to_owned();
    db.execute::<JournalEntry, _, LedgerError>(id, crate::upcasters(), metadata, |loaded| {
        if loaded.aggregate.posted {
            // Already done. A no-op rather than an error, so a retried request
            // succeeds instead of confusing the caller.
            return Ok(Decision::nothing());
        }
        Ok(Decision::one(JournalEntryEvent::Posted {
            occurred_on,
            memo: memo.clone(),
            lines: lines.clone(),
        }))
    })
    .await
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
