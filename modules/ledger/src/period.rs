//! When the books stopped taking entries.
//!
//! # What this is for
//!
//! A VAT return is filed for a period, and the tax on it is paid. A journal
//! entry back-dated into that period afterwards changes the numbers behind a
//! declaration that has already been made — and nothing anywhere records that it
//! happened. The same goes for an invoice with a back-dated tax point, and for a
//! credit note dated into a quarter that was closed months ago.
//!
//! Closing the books is the accountant saying "these numbers are final". After
//! it, corrections go into the period that is open, which is where an auditor
//! expects to find them.
//!
//! # Why one instant rather than a table of periods
//!
//! Books close in order. January, then February, then March — a business does
//! not close March while February is still open, because the March numbers are
//! built on the February ones. So the whole state is a single watermark: the
//! instant everything before which is final.
//!
//! ponytail: a non-contiguous close — a locked prior year with one adjustment
//! period left open inside it — is a table of ranges rather than a scalar, and
//! this becomes the newest row of it. Worth building when somebody has a prior
//! year to lock; guessing at the shape now would be guessing.
//!
//! # Why exclusive
//!
//! `closed_before` is the first instant that is **still open**, so closing
//! January is `2026-02-01T00:00:00Z`. The same convention as the VAT return's
//! `until`, and for the same reason: "closed through 31 January" is a comparison
//! somebody gets wrong once a month, and gets wrong by exactly one day.
//!
//! # Where the check is
//!
//! One place: [`post_entry_in`](crate::post_entry_in). Every posting in the
//! system routes through it — hand-written entries, reversals, and everything
//! sales does, because an invoice and its journal entry commit together. A check
//! per caller would be a check somebody forgets, and the one forgotten would be
//! the one that mattered.
//!
//! # Two acts: closing a period, booking a year
//!
//! Decided 2026-09-14. **Closing a period** moves the watermark to its end, and
//! periods close in order — the one to close is the one the watermark is in.
//! **Booking a year** is what an accountant calls the year-end close: once every
//! period of a fiscal year is closed, its result is moved into retained earnings
//! by one closing entry per currency, dated the year's last day, so the trading
//! accounts start the next year at zero. Reopening runs in reverse — the latest
//! closed period, a year only while no later year is booked, and a booked year's
//! periods only once the year is reopened, which reverses its entries.
//!
//! Any period may be the first one ever closed: what lies before it closes with
//! it and is never booked, and the balance sheet goes on showing that history's
//! result as a computed line. A VAT return that is filed still moves the
//! watermark ([`close_through`]) and never books anything.

use std::collections::BTreeMap;

use erp_eventlog::{ConfigError, ExecuteError, Metadata};
use erp_types::{AggregateId, CurrencyCode, Timestamp};

use crate::LedgerError;
use crate::fiscal::Period;

/// The books, as a business has closed them.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Books {
    /// The first instant still open. Everything strictly before it is final.
    ///
    /// `None` on a tenant that has never closed a period, which is every tenant
    /// until their first month end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_before: Option<Timestamp>,
    /// Every fiscal year that has ever been booked, by the calendar year it
    /// starts in — including ones reopened since, because their next close
    /// needs to know how many came before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub years: BTreeMap<i32, BookedYear>,
}

/// What booking a fiscal year left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct BookedYear {
    /// Whether the closing entries stand. `false` once reopened.
    pub booked: bool,
    /// How many times the year has been booked. Part of every closing entry's
    /// id, so a year booked, reopened and booked again gets fresh entries
    /// rather than a silent no-op on ids the log already holds.
    pub closes: u32,
    /// The closing entries standing, one per currency; empty once reopened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<String>,
}

impl Books {
    /// Whether `year`'s closing entries stand.
    #[must_use]
    pub fn is_booked(&self, year: i32) -> bool {
        self.years.get(&year).is_some_and(|y| y.booked)
    }
}

/// Where a year's result goes when it is booked, by currency.
///
/// An account holds one currency and the closing entry is per currency, so a
/// business trading in two needs two. `3100` — retained earnings in every
/// shipped chart — serves any currency it holds without being configured; a
/// currency with no account here and none at `3100` refuses the year close
/// rather than closing into the wrong currency.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ClosingAccounts {
    #[serde(flatten)]
    pub by_currency: BTreeMap<CurrencyCode, AggregateId>,
}

impl ClosingAccounts {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "ledger.closing_accounts";

    /// The code every shipped chart keeps retained earnings under.
    pub const CONVENTIONAL: &'static str = "3100";

    /// What this tenant has configured, or nothing.
    ///
    /// # Errors
    /// If the stored value cannot be read.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map(|configured| configured.value)
            .unwrap_or_default())
    }

    /// The account a currency's result closes into: the configured one, or
    /// [`Self::CONVENTIONAL`] when that account holds the currency, or none.
    ///
    /// # Errors
    /// If the log cannot be read.
    pub async fn for_currency(
        &self,
        conn: &mut sqlx::PgConnection,
        currency: CurrencyCode,
    ) -> Result<Option<AggregateId>, erp_eventlog::LoadError> {
        if let Some(code) = self.by_currency.get(&currency) {
            return Ok(Some(code.clone()));
        }
        let Ok(conventional) = AggregateId::new(Self::CONVENTIONAL) else {
            return Ok(None);
        };
        Ok(
            (crate::posting_currency(conn, &conventional).await? == Some(currency))
                .then_some(conventional),
        )
    }
}

impl Books {
    /// Where a tenant's choice is stored.
    pub const KEY: &'static str = "ledger.books";

    /// Whether an entry dated `occurred_on` may still be posted.
    #[must_use]
    pub fn accepts(&self, occurred_on: Timestamp) -> bool {
        self.closed_before
            .is_none_or(|closed_before| occurred_on >= closed_before)
    }
}

/// How the books stand, **read inside the caller's transaction**.
///
/// Not cached and not read once at startup: a period closed a second ago has to
/// refuse the next posting, and a check against a stale copy is a check that
/// lets exactly the entries through that somebody just closed the books to keep
/// out.
pub async fn books(conn: &mut sqlx::PgConnection) -> Result<Books, ConfigError> {
    Ok(erp_eventlog::configuration::get::<Books>(conn, Books::KEY)
        .await?
        .map(|configured| configured.value)
        .unwrap_or_default())
}

/// Closes the books before `closed_before`, or reopens them by moving it back.
///
/// **Reopening is allowed on purpose.** An accountant who closes the wrong month
/// has to be able to put it right, and a system that refuses would be one they
/// route around by editing the database. What it must not be is quiet, which is
/// what `set_by` and `set_at` on the stored value are for.
///
/// ponytail: the store keeps the current value and who set it, not a history. A
/// tenant who wants "every close and reopen, in order" needs a history table
/// beside `configuration` serving every key — a better shape than one per
/// consumer, and not worth building for a single one.
pub async fn close(
    conn: &mut sqlx::PgConnection,
    closed_before: Option<Timestamp>,
    by: Option<&str>,
) -> Result<Books, ConfigError> {
    let books = Books {
        closed_before,
        ..books(&mut *conn).await?
    };
    erp_eventlog::configuration::set(conn, Books::KEY, &books, by, None).await?;
    Ok(books)
}

/// Why a period or a year did not close, or reopen.
#[derive(Debug, thiserror::Error)]
pub enum CloseError {
    #[error("{0} is not a period of this calendar")]
    NoSuchPeriod(String),
    /// Periods close in order, and `next` is the one the watermark is in.
    #[error("{period} cannot close while {next} is still open")]
    OutOfOrder { period: String, next: String },
    /// Only the latest closed period reopens.
    #[error("{period} is not the latest closed period; {latest} is")]
    NotLatest { period: String, latest: String },
    /// A booked year's periods stay closed until the year is reopened.
    #[error("{year} is booked; reopen the year before its periods")]
    YearBooked { year: i32, period: String },
    /// A year books once every period of it is closed.
    #[error("{year} cannot be booked while {period} is still open")]
    YearOpen { year: i32, period: String },
    /// Years reopen in reverse order.
    #[error("{year} cannot be reopened while {later} is booked")]
    LaterYearBooked { year: i32, later: i32 },
    /// A trading balance in a currency nothing closes into.
    #[error("no retained-earnings account holds {currency}; {year} cannot be booked")]
    NeedsAccount { year: i32, currency: CurrencyCode },
    /// The closing entry is computed from the read model, which is behind the
    /// log; booking now would book the wrong figures.
    #[error("the ledger's read model is {behind} events behind")]
    ReadModelBehind { behind: i64 },
    /// The closing entry itself was refused — an account closed, say.
    #[error(transparent)]
    Ledger(#[from] ExecuteError<LedgerError>),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Pool(#[from] erp_tenant::PoolError),
}

/// [`close_period_in`] on its own transaction.
///
/// # Errors
/// As [`close_period_in`], or the transaction.
pub async fn close_period(
    db: &erp_tenant::TenantDb,
    id: &str,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut tx = db.begin().await?;
    let books = close_period_in(&mut tx, id, by).await?;
    tx.commit().await?;
    Ok(books)
}

/// [`reopen_period_in`] on its own transaction.
///
/// # Errors
/// As [`reopen_period_in`], or the transaction.
pub async fn reopen_period(
    db: &erp_tenant::TenantDb,
    id: &str,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut tx = db.begin().await?;
    let books = reopen_period_in(&mut tx, id, by).await?;
    tx.commit().await?;
    Ok(books)
}

/// [`close_year_in`] on its own transaction: the entries and the record of
/// them commit together, or neither does.
///
/// # Errors
/// As [`close_year_in`], or the transaction.
pub async fn close_year(
    db: &erp_tenant::TenantDb,
    year: i32,
    memo: &str,
    metadata: &Metadata,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut tx = db.begin().await?;
    let books = close_year_in(&mut tx, year, memo, metadata, by).await?;
    tx.commit().await?;
    Ok(books)
}

/// [`reopen_year_in`] on its own transaction.
///
/// # Errors
/// As [`reopen_year_in`], or the transaction.
pub async fn reopen_year(
    db: &erp_tenant::TenantDb,
    year: i32,
    memo: &str,
    metadata: &Metadata,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut tx = db.begin().await?;
    let books = reopen_year_in(&mut tx, year, memo, metadata, by).await?;
    tx.commit().await?;
    Ok(books)
}

impl From<erp_eventlog::LoadError> for CloseError {
    fn from(e: erp_eventlog::LoadError) -> Self {
        Self::Ledger(ExecuteError::Load(e))
    }
}

/// What every close and reopen needs: the calendar, the clock, and the books
/// as they stand, with the version they stand at so two accountants closing
/// at once conflict rather than overwrite.
struct Standing {
    fiscal: crate::fiscal::FiscalCalendars,
    clock: erp_types::Calendar,
    books: Books,
    version: Option<i64>,
}

async fn standing(conn: &mut sqlx::PgConnection) -> Result<Standing, CloseError> {
    let fiscal = crate::fiscal::fiscal_calendar(&mut *conn).await?;
    let clock = erp_eventlog::configuration::calendar(&mut *conn).await?;
    let stored = erp_eventlog::configuration::get::<Books>(&mut *conn, Books::KEY).await?;
    let (books, version) = stored.map_or((Books::default(), None), |c| (c.value, Some(c.version)));
    Ok(Standing {
        fiscal,
        clock,
        books,
        version,
    })
}

impl Standing {
    fn period(&self, id: &str) -> Result<Period, CloseError> {
        self.fiscal
            .period(id)
            .ok_or_else(|| CloseError::NoSuchPeriod(id.to_owned()))
    }

    /// The period the watermark is in — the one to close next, and the one
    /// that reopens — or `None` while nothing is closed.
    fn at_watermark(&self, closed_before: Timestamp) -> Period {
        self.fiscal.period_containing(self.clock.day(closed_before))
    }

    async fn store(
        self,
        conn: &mut sqlx::PgConnection,
        by: Option<&str>,
    ) -> Result<Books, CloseError> {
        erp_eventlog::configuration::set(conn, Books::KEY, &self.books, by, self.version).await?;
        Ok(self.books)
    }
}

/// Closes a period: moves the watermark to its end.
///
/// The period to close is the one the watermark is in, or any period when
/// nothing is closed yet. A period already behind the watermark is a no-op —
/// the retry of a close that succeeded — and a later one is refused, naming
/// the one to close first.
///
/// # Errors
/// [`CloseError::NoSuchPeriod`], [`CloseError::OutOfOrder`], or the store.
pub async fn close_period_in(
    conn: &mut sqlx::PgConnection,
    id: &str,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut standing = standing(&mut *conn).await?;
    let period = standing.period(id)?;
    let until = standing.clock.start_of(period.until);
    let from = standing.clock.start_of(period.from);
    if let Some(closed_before) = standing.books.closed_before {
        if closed_before >= until {
            return Ok(standing.books);
        }
        if closed_before < from {
            return Err(CloseError::OutOfOrder {
                period: period.id,
                next: standing.at_watermark(closed_before).id,
            });
        }
    }
    standing.books.closed_before = Some(until);
    standing.store(conn, by).await
}

/// Reopens a period: moves the watermark back to its start.
///
/// Only the latest closed period — the one the watermark is in, or ends — can
/// reopen, and not while its year is booked. A period that is already open is
/// a no-op.
///
/// # Errors
/// [`CloseError::NoSuchPeriod`], [`CloseError::NotLatest`],
/// [`CloseError::YearBooked`], or the store.
pub async fn reopen_period_in(
    conn: &mut sqlx::PgConnection,
    id: &str,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut standing = standing(&mut *conn).await?;
    let period = standing.period(id)?;
    let until = standing.clock.start_of(period.until);
    let from = standing.clock.start_of(period.from);
    let Some(closed_before) = standing.books.closed_before else {
        return Ok(standing.books);
    };
    if closed_before <= from {
        return Ok(standing.books);
    }
    if closed_before > until {
        // The last closed instant is `closed_before - 1`; the period holding
        // it is the one that reopens.
        let latest = standing.at_watermark(closed_before - chrono::Duration::seconds(1));
        return Err(CloseError::NotLatest {
            period: period.id,
            latest: latest.id,
        });
    }
    if standing.books.is_booked(period.year) {
        return Err(CloseError::YearBooked {
            year: period.year,
            period: period.id,
        });
    }
    standing.books.closed_before = Some(from);
    standing.store(conn, by).await
}

/// **Books a fiscal year**: posts its closing entries and records them.
///
/// Every period of the year must be closed. Then, per currency, every revenue
/// and expense account's balance over the year is posted away and the sum
/// posted to the retained-earnings account for that currency — one entry per
/// currency, dated the year's last day, flagged as closing so the profit and
/// loss leaves it out. A year already booked is a no-op.
///
/// The balances come from the read model, which must be at the head of the
/// log: the entries a close is about are all before the watermark, so nothing
/// can still be arriving for them, but a read model that has not caught up
/// would book the wrong figures.
///
/// # Errors
/// [`CloseError::YearOpen`], [`CloseError::NeedsAccount`],
/// [`CloseError::ReadModelBehind`], a refused posting, or the store.
pub async fn close_year_in(
    conn: &mut sqlx::PgConnection,
    year: i32,
    memo: &str,
    metadata: &Metadata,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut standing = standing(&mut *conn).await?;
    if standing.books.is_booked(year) {
        return Ok(standing.books);
    }
    let from = standing.clock.start_of(standing.fiscal.year_start(year));
    let until = standing
        .clock
        .start_of(standing.fiscal.year_start(year + 1));
    if standing.books.closed_before.is_none_or(|w| w < until) {
        let open = standing.books.closed_before.map_or_else(
            || standing.fiscal.periods(year).remove(0),
            |w| standing.at_watermark(w.max(from)),
        );
        return Err(CloseError::YearOpen {
            year,
            period: open.id,
        });
    }
    caught_up(&mut *conn).await?;

    let accounts = ClosingAccounts::resolve(&mut *conn).await?;
    let lines = crate::profit_and_loss(&mut *conn, from, until, None).await?;
    let mut by_currency: BTreeMap<CurrencyCode, Vec<crate::Line>> = BTreeMap::new();
    for line in lines {
        if line.balance.minor() == 0 {
            continue;
        }
        let account = AggregateId::new(&line.code)
            .map_err(|_| ExecuteError::Rejected(LedgerError::BadAccountCode(line.code.clone())))?;
        let amount = line
            .balance
            .checked_neg()
            .map_err(|e| ExecuteError::Rejected(LedgerError::Unbalanced(e.into())))?;
        by_currency
            .entry(line.balance.currency())
            .or_default()
            .push(crate::Line::new(account, amount));
    }

    // Every currency's entry is built — and every refusal found — before any
    // is posted, so a year with nowhere to close its dollars into leaves no
    // riyal entry behind whether or not the caller's transaction rolls back.
    let closes = standing.books.years.get(&year).map_or(0, |y| y.closes) + 1;
    let mut prepared = Vec::new();
    for (currency, mut lines) in by_currency {
        let Some(retained) = accounts.for_currency(&mut *conn, currency).await? else {
            return Err(CloseError::NeedsAccount { year, currency });
        };
        // The other side: what the trading accounts net to, with the sign
        // flipped so the entry balances — a profit credits retained earnings.
        let mut result = erp_types::Money::zero(currency);
        for line in &lines {
            result = result.checked_add(line.amount).map_err(overflow)?;
        }
        let result = result.checked_neg().map_err(overflow)?;
        lines.push(crate::Line::new(retained, result));
        let balanced = crate::BalancedLines::new(lines)
            .map_err(|e| ExecuteError::Rejected(LedgerError::Unbalanced(e)))?;
        let id = AggregateId::new(format!("closing-{year}-{currency}-{closes}"))
            .map_err(|_| ExecuteError::Rejected(LedgerError::BadAccountCode(year.to_string())))?;
        prepared.push((id, balanced));
    }

    let last_day = standing.fiscal.year_start(year + 1) - chrono::Duration::days(1);
    let occurred_on = standing.clock.start_of(last_day);
    let mut entries = Vec::new();
    for (id, balanced) in prepared {
        crate::commands::post_closing_in(&mut *conn, &id, occurred_on, memo, &balanced, metadata)
            .await?;
        entries.push(id.as_str().to_owned());
    }

    standing.books.years.insert(
        year,
        BookedYear {
            booked: true,
            closes,
            entries,
        },
    );
    standing.store(conn, by).await
}

/// **Reopens a fiscal year**: reverses its closing entries and records that
/// they no longer stand. Its periods stay closed; reopen those separately.
///
/// Refused while a later year is booked, because that year's opening figures
/// were built on this one's close. A year not booked is a no-op.
///
/// # Errors
/// [`CloseError::LaterYearBooked`], a refused reversal, or the store.
pub async fn reopen_year_in(
    conn: &mut sqlx::PgConnection,
    year: i32,
    memo: &str,
    metadata: &Metadata,
    by: Option<&str>,
) -> Result<Books, CloseError> {
    let mut standing = standing(&mut *conn).await?;
    if !standing.books.is_booked(year) {
        return Ok(standing.books);
    }
    if let Some(later) = standing
        .books
        .years
        .iter()
        .find(|(y, b)| **y > year && b.booked)
        .map(|(y, _)| *y)
    {
        return Err(CloseError::LaterYearBooked { year, later });
    }
    let last_day = standing.fiscal.year_start(year + 1) - chrono::Duration::days(1);
    let occurred_on = standing.clock.start_of(last_day);
    let entries = standing
        .books
        .years
        .get(&year)
        .map(|y| y.entries.clone())
        .unwrap_or_default();
    for entry in &entries {
        let original = AggregateId::new(entry)
            .map_err(|_| ExecuteError::Rejected(LedgerError::BadAccountCode(entry.clone())))?;
        let reversal = AggregateId::new(format!("{entry}-reversal"))
            .map_err(|_| ExecuteError::Rejected(LedgerError::BadAccountCode(entry.clone())))?;
        crate::commands::reverse_closing_in(
            &mut *conn,
            &original,
            &reversal,
            occurred_on,
            memo,
            metadata,
        )
        .await?;
    }
    if let Some(booked) = standing.books.years.get_mut(&year) {
        booked.booked = false;
        booked.entries.clear();
    }
    standing.store(conn, by).await
}

/// A year's result too large to add up — the ledger has become nonsense, and
/// the close says so rather than posting part of it.
fn overflow(e: erp_types::MoneyError) -> ExecuteError<LedgerError> {
    ExecuteError::Rejected(LedgerError::Unbalanced(e.into()))
}

/// Refuses unless the ledger's read model has projected every event in the
/// log — the figures a year close posts are read from it.
async fn caught_up(conn: &mut sqlx::PgConnection) -> Result<(), CloseError> {
    let head = sqlx::query_scalar!(r#"SELECT COALESCE(max(position), 0) as "head!" FROM event"#)
        .fetch_one(&mut *conn)
        .await?;
    let reached = erp_projection::checkpoint::<crate::Ledger>(&mut *conn)
        .await?
        .get();
    if reached < head {
        return Err(CloseError::ReadModelBehind {
            behind: head - reached,
        });
    }
    Ok(())
}

/// **Closes the books through `until`, and never reopens them.**
///
/// What a filed return calls: the period it declared may not change under it,
/// so the watermark moves forward to the end of that period — and only
/// forward. A return filed for an earlier period after a later one was filed
/// must not pull the watermark back and reopen the later one. `close` is for a
/// person deciding where the books stand; this is for a fact that has already
/// left the building.
pub async fn close_through(
    conn: &mut sqlx::PgConnection,
    until: Timestamp,
    by: Option<&str>,
) -> Result<Books, ConfigError> {
    let current = books(&mut *conn).await?;
    if current
        .closed_before
        .is_some_and(|already| already >= until)
    {
        return Ok(current);
    }
    close(conn, Some(until), by).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> Timestamp {
        rfc3339.parse().unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn nothing_is_closed_until_something_is() {
        let open = Books::default();
        assert!(open.accepts(at("2020-01-01T00:00:00Z")));
        assert!(open.accepts(at("2099-01-01T00:00:00Z")));
    }

    /// The boundary, which is the whole reason the field is named `before`.
    #[test]
    fn the_instant_named_is_the_first_one_still_open() {
        let january_closed = Books {
            closed_before: Some(at("2026-02-01T00:00:00Z")),
            ..Books::default()
        };

        assert!(
            !january_closed.accepts(at("2026-01-31T23:59:59Z")),
            "the last moment of January is closed"
        );
        assert!(
            january_closed.accepts(at("2026-02-01T00:00:00Z")),
            "and the first moment of February is not — off by one here is off by \
             a day in a filed return"
        );
        assert!(january_closed.accepts(at("2026-03-15T00:00:00Z")));
    }
}
