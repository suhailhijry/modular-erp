//! Drafting a run, and approving it.
//!
//! # Who is in the run, and why the caller says
//!
//! The caller supplies the list of employees. That looks like work the module
//! should do, and it is deliberate: enumerating employees means reading
//! `proj_hr`, and L3 forbids one module's command from reading another's
//! projection group — while a *payroll run* is money leaving the business and
//! must not be computed from a table that may be a second behind.
//!
//! So the composition happens where every other cross-module read in this
//! system happens: at the read layer, in the client, which lists staff from
//! `hr` and sends the ones to pay. What the command then does is load each
//! person's **aggregate** for their salary, inside its own transaction, which
//! is a write-side load and exactly what L7 permits.
//!
//! It is also honest about the domain. A payroll run is reviewed before it
//! posts, and who is in it is a decision somebody makes.

use erp_eventlog::{
    Committed, Decision, ExecuteError, Loaded, MAX_ATTEMPTS, Metadata, load, try_execute,
};
use erp_tenant::{CommandError, TenantDb};
use erp_types::{AggregateId, CurrencyCode, Timestamp};

use crate::posting::{PostingAccounts, entry_for_run};
use crate::run::{NotAPeriod, Payslip, Period, Run, RunEvent, total};

#[derive(Debug, thiserror::Error)]
pub enum PayrollError {
    #[error("there is no payroll run {0}")]
    NoSuchRun(String),
    #[error("payroll run {0} has been approved and cannot be redrafted")]
    Approved(String),
    #[error("a payroll run needs somebody to pay")]
    NobodyToPay,
    #[error("{0} is not on the books, or has no salary recorded")]
    NotPayable(String),
    #[error(transparent)]
    Period(#[from] NotAPeriod),
    #[error(transparent)]
    Money(#[from] erp_types::MoneyError),
    #[error(transparent)]
    Unbalanced(#[from] ledger::Unbalanced),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    #[error(transparent)]
    Ledger(#[from] ledger::LedgerError),
}

impl erp_i18n::Localize for PayrollError {
    fn message(&self) -> erp_i18n::Message {
        use crate::messages;
        use erp_i18n::{Message, MessageArg};
        match self {
            Self::NoSuchRun(id) => {
                Message::new(messages::NO_SUCH_RUN).with("id", MessageArg::text(id))
            }
            Self::Approved(id) => Message::new(messages::APPROVED).with("id", MessageArg::text(id)),
            Self::NobodyToPay => Message::new(messages::NOBODY_TO_PAY),
            Self::NotPayable(id) => {
                Message::new(messages::NOT_PAYABLE).with("id", MessageArg::text(id))
            }
            Self::Period(e) => {
                Message::new(messages::NOT_A_PERIOD).with("period", MessageArg::text(&e.0))
            }
            Self::Money(_) => Message::new(messages::AMOUNT_OUT_OF_RANGE),
            // Each already says the right thing in both languages.
            Self::Unbalanced(e) => e.message(),
            Self::Config(e) => e.message(),
            Self::Ledger(e) => e.message(),
        }
    }
}

type Refusal = CommandError<PayrollError>;
type Outcome = Result<Committed<RunEvent>, Refusal>;

/// Computes what everybody would be paid, and **posts nothing**.
///
/// Drafting again replaces the previous draft: a business fixes two payslips
/// and runs it over, and a run that accumulated drafts would pay somebody
/// twice. An approved run refuses — the entry is in the books and the payslips
/// are what people were told.
pub async fn draft_run(
    db: &TenantDb,
    id: &AggregateId,
    period: Period,
    employees: &[AggregateId],
    at: Timestamp,
    metadata: &Metadata,
) -> Outcome {
    if employees.is_empty() {
        return Err(rejected(PayrollError::NobodyToPay));
    }

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let payslips = compute(&mut *conn, employees, period).await?;
            let currency = payslips
                .first()
                .map(|p| p.gross.currency())
                .ok_or(ExecuteError::Rejected(PayrollError::NobodyToPay))?;
            let totals =
                total(&payslips, currency).map_err(|e| ExecuteError::Rejected(e.into()))?;

            // **`try_execute`, not `try_create`.** A run is named by the caller
            // — `2026-05` reads well — and redrafting is the operation, so
            // there is no "this id is taken by something else" to distinguish.
            //
            // The first version used `try_create` and leaned on `AlreadyExists`
            // to detect an existing run. That variant is about a *different
            // request* reusing an id, and with no fingerprint on either side a
            // second draft looked like a retry: redrafting silently did
            // nothing, and the run posted the draft nobody wanted. The test
            // caught it; the fix was to stop needing the distinction.
            let (gross, deductions, net) = totals;
            try_execute::<Run, _, PayrollError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded: &Loaded<Run>| {
                    let held = &loaded.aggregate;
                    if held.is_approved() {
                        return Err(PayrollError::Approved(id.to_string()));
                    }
                    // The same draft again writes nothing: recomputing a run
                    // that has not changed is what a screen does every time
                    // somebody opens it.
                    if held.exists() && held.payslips == payslips {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(RunEvent::Drafted {
                        period,
                        payslips: payslips.clone(),
                        gross,
                        deductions,
                        net,
                        at,
                    }))
                },
            )
            .await
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// Approves a run and **posts it**.
///
/// The entry is dated to the last day of the period, not to the day the run was
/// approved: a February run approved on the 3rd of March belongs in February,
/// and the whole point of a period is that it does.
///
/// Idempotent: approving an approved run reports the entry it already made.
pub async fn approve_run(db: &TenantDb, id: &AggregateId, metadata: &Metadata) -> Outcome {
    let entry = derived("pr", id)?;

    for _ in 1..=MAX_ATTEMPTS {
        let mut tx = db.begin().await?;
        let outcome = async {
            let conn = &mut *tx;
            let held = load::<Run>(&mut *conn, id, crate::upcasters())
                .await
                .map_err(ExecuteError::Load)?
                .aggregate;
            if !held.exists() {
                return Err(ExecuteError::Rejected(PayrollError::NoSuchRun(
                    id.to_string(),
                )));
            }

            let committed = try_execute::<Run, _, PayrollError>(
                &mut *conn,
                id,
                crate::upcasters(),
                metadata,
                |loaded| {
                    if loaded.aggregate.is_approved() {
                        return Ok(Decision::nothing());
                    }
                    Ok(Decision::one(RunEvent::Approved {
                        entry: entry.clone(),
                        at: chrono::Utc::now(),
                    }))
                },
            )
            .await?;

            // **Only when this call is the one that approved it.** A retry
            // reports the same entry and posts nothing, which is what makes an
            // approval that timed out safe to send again.
            if committed.at.is_some() {
                post(&mut *conn, &held, &entry, metadata).await?;
            }
            Ok(committed)
        }
        .await;

        if let Some(done) = settle(tx, outcome).await? {
            return Ok(done);
        }
    }
    contended(id)
}

/// The journal entry, through the same seam `sales` posts an invoice through.
async fn post(
    conn: &mut sqlx::PgConnection,
    run: &Run,
    entry: &AggregateId,
    metadata: &Metadata,
) -> Result<(), ExecuteError<PayrollError>> {
    let accounts = PostingAccounts::resolve(&mut *conn)
        .await
        .map_err(|e| ExecuteError::Rejected(e.into()))?;

    let (Some(gross), Some(deductions), Some(net), Some(period)) =
        (run.gross, run.deductions, run.net, run.period)
    else {
        return Err(ExecuteError::Rejected(PayrollError::NobodyToPay));
    };

    let lines = entry_for_run(gross, deductions, net, &accounts)
        .map_err(|e| ExecuteError::Rejected(e.into()))?;

    let occurred_on = period
        .ends_on()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc())
        .ok_or_else(|| {
            ExecuteError::Rejected(PayrollError::Period(NotAPeriod(period.to_string())))
        })?;

    ledger::post_entry_in(
        &mut *conn,
        entry,
        occurred_on,
        &format!("Payroll {period}"),
        &lines,
        metadata,
    )
    .await
    .map_err(lift)?;
    Ok(())
}

/// What each person is paid, read from `hr`'s **log**.
///
/// Not `proj_hr`: money leaving the business on the strength of a table that
/// may be a second behind is the one kind of lag nobody accepts.
///
/// Somebody who was not on the books for the **whole period**, or who has no
/// salary recorded, **refuses the whole run** rather than being skipped. A run
/// that quietly left somebody out is a run somebody notices on payday — and one
/// that silently paid a full month to a half-month joiner is worse, because
/// nobody notices it at all.
async fn compute(
    conn: &mut sqlx::PgConnection,
    employees: &[AggregateId],
    period: Period,
) -> Result<Vec<Payslip>, ExecuteError<PayrollError>> {
    let (from, until) = period.starts_on().zip(period.ends_on()).ok_or_else(|| {
        ExecuteError::Rejected(PayrollError::Period(NotAPeriod(period.to_string())))
    })?;

    let mut payslips = Vec::with_capacity(employees.len());
    for employee in employees {
        let salary = hr::salary_for(&mut *conn, employee, from, until)
            .await
            .map_err(ExecuteError::Load)?
            .ok_or_else(|| {
                ExecuteError::Rejected(PayrollError::NotPayable(employee.to_string()))
            })?;

        let held = load::<hr::Employee>(&mut *conn, employee, hr::upcasters())
            .await
            .map_err(ExecuteError::Load)?
            .aggregate;

        let gross = salary
            .gross()
            .map_err(|e| ExecuteError::Rejected(PayrollError::Money(e)))?;
        let net = salary
            .net()
            .map_err(|e| ExecuteError::Rejected(PayrollError::Money(e)))?;
        let deductions = gross
            .checked_sub(net)
            .map_err(|e| ExecuteError::Rejected(PayrollError::Money(e)))?;

        payslips.push(Payslip {
            employee: employee.clone(),
            // **Frozen.** A payslip says who it was for, and somebody who
            // marries next month does not get a new copy of last month's.
            name: held.name,
            basic: salary.basic,
            gross,
            deductions,
            net,
        });
    }

    // **One currency per run.** A business paying two runs payroll twice, which
    // is what its books do anyway — and `Money::checked_sum` would refuse the
    // total later, with a message about arithmetic rather than about the
    // salary somebody entered in the wrong currency.
    let currency: Option<CurrencyCode> = payslips.first().map(|p| p.gross.currency());
    if let Some(currency) = currency
        && let Some(odd) = payslips.iter().find(|p| p.gross.currency() != currency)
    {
        return Err(ExecuteError::Rejected(PayrollError::Money(
            erp_types::MoneyError::CurrencyMismatch {
                left: currency,
                right: odd.gross.currency(),
            },
        )));
    }

    Ok(payslips)
}

// ------------------------------------------------------------------ plumbing

fn derived(prefix: &str, id: &AggregateId) -> Result<AggregateId, Refusal> {
    AggregateId::new(format!("{prefix}.{id}"))
        .map_err(|_| rejected(PayrollError::NoSuchRun(id.to_string())))
}

/// Lifts a `ledger` refusal into this module's error.
fn lift(error: ExecuteError<ledger::LedgerError>) -> ExecuteError<PayrollError> {
    match error {
        ExecuteError::Rejected(e) => ExecuteError::Rejected(PayrollError::Ledger(e)),
        ExecuteError::Load(e) => ExecuteError::Load(e),
        ExecuteError::Append(e) => ExecuteError::Append(e),
        ExecuteError::Enqueue(e) => ExecuteError::Enqueue(e),
        ExecuteError::Database(e) => ExecuteError::Database(e),
        ExecuteError::Contended { stream, attempts } => {
            ExecuteError::Contended { stream, attempts }
        }
        ExecuteError::AlreadyExists { stream } => ExecuteError::AlreadyExists { stream },
    }
}

async fn settle<T>(
    tx: erp_tenant::Tx,
    outcome: Result<T, ExecuteError<PayrollError>>,
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

fn rejected(error: PayrollError) -> Refusal {
    CommandError::Execute(ExecuteError::Rejected(error))
}

fn contended<T>(id: &AggregateId) -> Result<T, Refusal> {
    Err(CommandError::Execute(ExecuteError::Contended {
        stream: erp_types::StreamId::new(<Run as erp_eventlog::Aggregate>::domain(), id.clone()),
        attempts: MAX_ATTEMPTS,
    }))
}
