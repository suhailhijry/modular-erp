# ledger

Double-entry accounting. The first module, and the proof that the module seam
works: it was built as a module from the start, never extracted from the kernel later,
so a tenant that does not want accounting simply does not enable it.

**Depends on:** `erp-tenant`, `erp-web`, `erp-projection`, `erp-eventlog`.
**Depended on by:** `sales`, `purchases`, `tax_sa`.

## The one invariant

**Debits equal credits, per currency.** Enforced twice, on purpose.

At the **type level**, by `BalancedLines`. An unbalanced entry cannot be
constructed, cannot be stored, and cannot be decoded back out of storage, so
posting code has nothing to remember.

At the **data level**, by `trial_balance`. That query can only be non-zero if the
*pipeline* is broken, which makes it a canary for a whole class of bug rather
than a check on the posting rules.

## The files

| File | What is in it |
|---|---|
| [`account.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/account.rs) | `Account`, `AccountEvent`, `AccountKind` |
| [`entry.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/entry.rs) | `JournalEntry`, `JournalEntryEvent` |
| [`lines.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/lines.rs) | `Line`, `BalancedLines`, `Unbalanced` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/commands.rs) | Everything a caller can ask the ledger to do |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/projections.rs) | `Ledger` group, `Accounts`, `Postings`, `trial_balance` |
| [`charts.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/charts.rs) | Ready-made charts of accounts |
| [`vat.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/vat.rs) | `VatCategory` and `Rates` |
| [`period.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/period.rs) | `Books`, closing the books |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/src/http.rs) | The routes |
| [`schema/install.sql`](https://github.com/suhailhijry/modular-erp/blob/main/modules/ledger/schema/install.sql) | `proj_ledger` |

## Lines

```rust
pub struct Line {
    pub account: AggregateId,
    pub amount: Money,          // positive debits, negative credits
    pub memo: Option<String>,
}
impl Line {
    pub const fn new(account: AggregateId, amount: Money) -> Self;
    pub fn with_memo(self, memo: impl Into<String>) -> Self;
    pub const fn is_debit(&self) -> bool;
}
```

**Signed, not a debit/credit pair.** A debit is positive and a credit negative, so
"debits equal credits" becomes "the amounts sum to zero": one check on one number
instead of two sums and a comparison, and no way to write a line that is somehow
both. Statements render the two columns from the sign, which is a presentation
concern.

```rust
pub struct BalancedLines { … }      // private field

impl BalancedLines {
    pub fn new(lines: Vec<Line>) -> Result<Self, Unbalanced>;
    pub fn as_slice(&self) -> &[Line];
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub const fn currency(&self) -> CurrencyCode;
    pub fn total_debits(&self) -> Result<Money, MoneyError>;
}
```

`new` rejects fewer than two lines, mixed currencies, a zero line, and any set
whose amounts do not sum to zero.

**The private field is the whole point.** Every path that produces one goes
through `new`, and the event payload holds this type, so an unbalanced entry is
not something the posting code has to remember to refuse. And `Deserialize`
revalidates, so a replay cannot resurrect one. That last part matters most: data
written by an older version of this system is exactly where "it was valid when we
wrote it" stops being a guarantee.

```rust
let lines = BalancedLines::new(vec![
    Line::new(cash.clone(),    Money::from_minor( 11_500, sar)),
    Line::new(revenue.clone(), Money::from_minor(-10_000, sar)),
    Line::new(vat.clone(),     Money::from_minor( -1_500, sar)),
])?;
```

## Accounts

```rust
pub enum AccountKind { Asset, Liability, Equity, Revenue, Expense }
impl AccountKind {
    pub const fn is_debit_normal(self) -> bool;
    pub const fn as_str(self) -> &'static str;
}

pub enum AccountEvent {
    Opened { name: String, kind: AccountKind, currency: CurrencyCode },
    Renamed { name: String },
    Closed,
    Reopened,
}
impl AccountEvent { pub const NAMES: [&'static str; 4]; }

pub struct Account { … }
impl Aggregate for Account { … }
impl Account { pub const fn accepts_postings(&self) -> bool; }
```

The five kinds are fixed and not configurable, because they are what double-entry
accounting *is*, and every statement format in every jurisdiction is built from
them. Tenant vocabulary, "Cash at bank", is the account's *name*; this is its
*behaviour*.

`is_debit_normal` is used only for presentation. Nothing refuses a balance on the
"wrong" side, because a contra account and an overdrawn bank account are both
real.

**An account holds one currency.** A business with two currencies has two cash
accounts, which is how every accounting system works and what makes the trial
balance checkable per currency.

## Journal entries

```rust
pub enum JournalEntryEvent {
    Posted { occurred_on: Timestamp, memo: String, lines: BalancedLines, closing: bool },
    Reversed { by: String, occurred_on: Timestamp },
}
impl JournalEntryEvent {
    pub const NAMES: [&'static str; 2] = ["ledger.entry.posted", "ledger.entry.reversed"];
}

pub struct JournalEntry { … }
impl JournalEntry { pub const fn is_reversed(&self) -> bool; }
```

`occurred_on` is the accounting date, which is **not** when it was recorded. A
March invoice entered in April belongs to March.

A posted entry never changes, so most of the aggregate is about answering two
questions a command needs: has this id been used, which makes posting the same
entry twice a no-op, and what did it say, which is what reversing one needs in
order to write its opposite.

## Commands

```rust
type Outcome<E> = Result<Committed<E>, CommandError<LedgerError>>;

pub async fn open_account(db: &TenantDb, code: &AggregateId, name: &str,
    kind: AccountKind, currency: CurrencyCode, metadata: &Metadata)
    -> Outcome<AccountEvent>;

pub async fn rename_account(db: &TenantDb, code: &AggregateId, name: &str,
    metadata: &Metadata) -> Outcome<AccountEvent>;

pub async fn close_account(db: &TenantDb, code: &AggregateId, metadata: &Metadata)
    -> Outcome<AccountEvent>;

pub async fn post_entry(db: &TenantDb, id: &AggregateId, occurred_on: Timestamp,
    memo: &str, lines: BalancedLines, metadata: &Metadata)
    -> Outcome<JournalEntryEvent>;

pub async fn reverse_entry(db: &TenantDb, original: &AggregateId,
    reversal: &AggregateId, occurred_on: Timestamp, memo: &str, metadata: &Metadata)
    -> Outcome<JournalEntryEvent>;

pub async fn posted_lines(db: &TenantDb, id: &AggregateId)
    -> Result<BalancedLines, CommandError<LedgerError>>;

pub async fn install_chart(db: &TenantDb, chart: &Chart, currency: CurrencyCode,
    locale: Locale, metadata: &Metadata) -> Result<Installed, CommandError<LedgerError>>;
```

`open_account` is idempotent **by refusal, not by silence**. Re-opening an
existing code is an error, because the second caller almost certainly meant a
different account.

`rename_account` is a no-op if the name already matches.

`post_entry` runs two checks in two places. That the lines balance is
`BalancedLines`'s job, done before this is called, because the type cannot hold
an unbalanced set. That every account exists and is open is `post_entry_in`'s
job, because it needs state the type cannot see. Re-posting the same `id` is a
no-op, which is what makes a retried request safe without an idempotency table.

### Why accounting does not delete

A posted entry is a statement about what happened, and somebody may have filed a
return against it. Correcting one means saying something *else*: the same lines
with the signs flipped, on a date of its own, so the books show both the mistake
and the correction. Deleting it would silently restate a period that has already
been reported.

`reverse_entry` posts the opposite entry under `reversal` and records on the
original that it was reversed and by what. Both, or neither.

**Both routes show a permission limit the entry's size first.** Debits, which
equal credits. A reversal is as large as the entry it undoes, so the route reads
that entry with `posted_lines` before reversing it; without that, a bookkeeper
limited to ten thousand riyals could post anybody's fifty thousand backwards.

### install_chart skips, and does not refuse

Installing eighteen accounts is eighteen commands, and the fifteenth can fail.
Refusing on the first duplicate would make the retry, the obvious next thing to
do, fail immediately and leave the chart half-built forever.

Skipping makes this "ensure these accounts exist", which is idempotent, and
idempotent is what turns recovery into retry. It also means a tenant can install
`retail` on top of `services` and get the three accounts it does not already
have.

## The seams other modules use

```rust
pub async fn post_entry_in(conn: &mut PgConnection, id: &AggregateId,
    occurred_on: Timestamp, memo: &str, lines: &BalancedLines, metadata: &Metadata)
    -> Result<Committed<JournalEntryEvent>, ExecuteError<LedgerError>>;

pub async fn reverse_in(conn: &mut PgConnection, original: &AggregateId,
    reversal: &AggregateId, occurred_on: Timestamp, memo: &str, metadata: &Metadata)
    -> Result<Committed<JournalEntryEvent>, ExecuteError<LedgerError>>;

pub async fn accepts_postings(conn: &mut PgConnection, code: &AggregateId)
    -> Result<bool, LoadError>;
```

These are for a module that produces its own events and must post alongside them.
Sales issuing an invoice, and every module after it. Both aggregates land in one
transaction, so an invoice that exists without its accounting entry is not a
state the system can reach, and nothing has to sweep for one afterwards.

**No retry.** The caller owns the transaction, so the caller owns the retry.

`accepts_postings` reads the **log**, not `proj_ledger.account`. The read model is
driven by a worker and lags, so a chart installed a moment ago is not in it yet,
and validating against it would tell a tenant that the account they just created
does not exist. This is the same question `post_entry_in` asks, asked the same
way, which is the point: a check that disagrees with the command it is guarding
is worse than no check.

## Charts of accounts

```rust
pub struct TemplateAccount { pub name_en: &'static str, pub name_ar: &'static str, … }
impl TemplateAccount { pub const fn name(&self, locale: Locale) -> &'static str; }

pub struct Chart { pub id: &'static str, … }
impl Chart {
    pub const fn name(&self, locale: Locale) -> &'static str;
    pub const fn description(&self, locale: Locale) -> &'static str;
}

pub static CHARTS: &[Chart];              // "services" and "retail"
pub fn chart(id: &str) -> Option<&'static Chart>;
pub struct Installed { … }
```

A tenant that has just signed up has an empty ledger, which is technically
correct and useless.

**The accounts are bilingual** because account names are what a bookkeeper reads
all day, and the first market is Saudi Arabia. Installing a chart in English and
telling people to rename eighteen accounts is not a starting point, it is a
chore. Installation picks by the caller's locale, and the tenant can rename
anything afterwards, because these are ordinary accounts from the moment they
exist.

**VAT and Zakat are in every chart.** Saudi VAT is 15% and ZATCA-reported, and
Zakat applies to Saudi and GCC-owned businesses. A chart without those accounts
is one a Saudi business has to fix before its first invoice, so they are the
baseline and not an advanced template.

There is deliberately no "empty" chart. Not installing one is already that, and a
template that creates nothing is a menu item that does nothing.

## VAT treatment

```rust
pub enum VatCategory { Standard, Zero, Exempt }
impl VatCategory {
    pub const ALL: [Self; 3];
    pub const fn input_is_reclaimable(self) -> bool;
    pub const fn as_str(self) -> &'static str;
}

pub struct Rates { … }
impl Rates {
    pub const KEY: &'static str = "ledger.vat_rates";
    pub const fn saudi_arabia() -> Self;          // 15%, since July 2020
    pub const fn of(self, category: VatCategory) -> i32;
    pub async fn resolve(conn: &mut PgConnection) -> Result<Self, ConfigError>;
}
```

**Why zero and exempt are not the same thing.** Both are 0%. On a return they are
different lines, and the difference is money: input tax attached to a zero-rated
supply is reclaimable, and input tax attached to an exempt one is not. Collapsing
them is a decision that cannot be undone later without asking a bookkeeper to
reclassify every historic line.

**Why this is in the ledger and not in sales.** It started in `sales`, which was
right while sales was the only module that knew about VAT. `purchases`
classifies input tax by the same three categories, and two sibling modules must
not depend on each other, so it moved to the module they both already depend on.

That is not a filing decision. The ledger is this system's accounting kernel for
a jurisdiction: it ships the Saudi chart templates, and every one carries a
`1200 Input VAT` and a `2100 VAT payable` account. The tax treatment of a line
belongs beside the accounts that treatment posts to.

**Why the rate is configuration.** It used to be `VatCategory::rate_now()`
returning 1500 from the accounting kernel, which is a fact about one country
living in the code every country would use, and a business in the UAE at 5% could
not issue a correct invoice at all. Now the rate is a value a tenant holds and a
country module seeds. `ledger` keeps the shape and has no opinion about the
number.

`resolve` is **read inside the command's transaction**, because a rate that
changed between the read and the write would leave an invoice stamped with one
that was never current.

## Closing the books

```rust
pub struct Books { pub closed_before: Option<Timestamp>, pub years: BTreeMap<i32, BookedYear> }
pub struct BookedYear { pub booked: bool, pub closes: u32, pub entries: Vec<String> }
impl Books {
    pub const KEY: &'static str = "ledger.books";
    pub fn accepts(&self, occurred_on: Timestamp) -> bool;
    pub fn is_booked(&self, year: i32) -> bool;
}

pub async fn books(conn: &mut PgConnection) -> Result<Books, ConfigError>;
pub async fn close_period_in(conn, id: &str, by) -> Result<Books, CloseError>;
pub async fn reopen_period_in(conn, id: &str, by) -> Result<Books, CloseError>;
pub async fn close_year_in(conn, year: i32, memo, metadata, by) -> Result<Books, CloseError>;
pub async fn reopen_year_in(conn, year: i32, memo, metadata, by) -> Result<Books, CloseError>;
pub async fn close_through(conn, until: Timestamp, by) -> Result<Books, ConfigError>;

pub struct ClosingAccounts { pub by_currency: BTreeMap<CurrencyCode, AggregateId> }
```

A VAT return is filed for a period and the tax on it is paid. A journal entry
back-dated into that period afterwards changes the numbers behind a declaration
that has already been made, and nothing anywhere records that it happened.
Closing the books is the accountant saying these numbers are final.

**One instant, not a table of periods.** Books close in order: January, then
February, then March, because a business does not close March while February is
still open. So the whole state is a single watermark.

**Exclusive.** `closed_before` is the first instant that is still open, so closing
January is `2026-02-01T00:00:00Z`. The same convention as the VAT return's
`until`, and for the same reason: "closed through 31 January" is a comparison
somebody gets wrong once a month, and gets wrong by exactly one day.

**Two acts** (decided 2026-09-14). *Closing a period* moves the watermark to the
period's end, in order: the one to close is the one the watermark is in, and any
period may be the first ever closed — what lies before it closes with it.
*Booking a year* is the year-end close: once every period of a fiscal year is
closed, its result is moved into retained earnings by one closing entry per
currency, dated the year's last day, so the trading accounts start the next year
at zero. Where it goes is `ClosingAccounts` — `3100` serves any currency it
holds without being configured, and a currency with nowhere to go refuses the
close before anything is posted. The figures come from the read model, which
must have caught up with the log, or the close says so and asks for a retry.

**Closing entries are the one posting allowed into closed time**, through
`post_entry_in` still, flagged — the flag is what the profit and loss reads to
leave them out, while every balance counts them. A closing entry is not
reversed by hand: reopening the year reverses it and records that the year is
open again. Ids are `closing-2025-SAR-1`, the count making a year booked,
reopened and booked again post fresh entries rather than a silent no-op.

**Reopening is allowed on purpose, in reverse.** An accountant who closes the
wrong month has to be able to put it right, and a system that refuses is one
they route around by editing the database. Only the latest closed period
reopens, not while its year is booked; a year reopens only while no later one
is booked; and `close_through` — what a filed VAT return calls — still moves
the watermark forward and never books anything. What none of it must be is
quiet, which is what `set_by` and `set_at` are for.

**Where the check is:** one place, `post_entry_in`. Every posting in the system
routes through it, including everything sales does, because an invoice and its
journal entry commit together. A check per caller would be a check somebody
forgets, and the one forgotten would be the one that mattered.

`books` is read inside the caller's transaction. Not cached and not read once at
startup: a period closed a second ago has to refuse the next posting.

## Read models

```rust
pub struct Ledger;
impl ProjectionGroup for Ledger {
    const NAME: &'static str = "ledger";
    const SCHEMA: &'static str = "proj_ledger";
}

pub struct Accounts;    // the chart of accounts
pub struct Postings;    // every line of every entry

pub fn projections() -> Vec<Arc<dyn Projection<Group = Ledger>>>;

pub struct TrialBalance { … }
impl TrialBalance { pub const fn balances(&self) -> bool; }

pub async fn trial_balance(conn: &mut PgConnection) -> Result<Vec<TrialBalance>, sqlx::Error>;
pub async fn imbalances(conn: &mut PgConnection) -> Result<Vec<TrialBalance>, sqlx::Error>;

pub struct AccountBalance { … }
pub async fn account_balances(conn: &mut PgConnection) -> Result<Vec<AccountBalance>, sqlx::Error>;
```

Accounts and postings are **one group** because they must agree. A posting
referencing an account that has not appeared yet is a state nobody should be able
to query, and separate groups would replay at different rates and produce exactly
that.

**What a non-zero trial balance row means.** Not "somebody posted badly", because
`BalancedLines` makes that unconstructable. It means the pipeline is broken: a
projection applied an event twice, or a rebuild diverged, or rows were written by
something that is not this code. It is the canary for an entire class of bug,
which is why it is checked continuously and not at month end.

`imbalances` is the health check this module contributes, registered in
`bin/worker.rs` as the `TrialBalance` invariant. Empty is healthy.

## Routes

Statements and the calendar: `GET`/`PUT /v1/ledger/fiscal-calendar`,
`GET /v1/ledger/periods?year=`, `GET /v1/ledger/balances?until=`,
`GET /v1/ledger/statements/profit-and-loss` and `…/balance-sheet` (a `period`,
or `from`/`until` and `as_at`), and the journal at `GET /v1/ledger/entries` and
`GET /v1/ledger/entries/{entry}`. All reads; the calendar's `PUT` is
`ManageAccounts`, like closing a period. The close: `POST
/v1/ledger/periods/{period}/close` and `…/reopen`, `GET /v1/ledger/years/{year}`
with `POST …/close` and `…/reopen`, and `GET`/`PUT /v1/ledger/closing-accounts`.

| Method | Path | Capability |
|---|---|---|
| `GET` `POST` | `/v1/ledger/accounts` | Read / ManageAccounts |
| `GET` | `/v1/ledger/trial-balance` | Read |
| `POST` | `/v1/ledger/entries` | PostEntries |
| `POST` | `/v1/ledger/entries/{entry}/reversal` | PostEntries |
| `GET` | `/v1/ledger/charts` | Read |
| `POST` | `/v1/ledger/chart` | ManageAccounts |
| `GET` | `/v1/ledger/books` | Read |
| `POST` | `/v1/ledger/periods/{period}/close`, `…/reopen` | ManageAccounts |
| `GET` | `/v1/ledger/years/{year}` | Read |
| `POST` | `/v1/ledger/years/{year}/close`, `…/reopen` | ManageAccounts |
| `GET` `PUT` | `/v1/ledger/closing-accounts` | Read / ManageAccounts |
| `GET` `PUT` | `/v1/ledger/vat-rates` | Read / ManageAccounts |

### VAT rates carry more than a rate

`/v1/ledger/vat-rates` also holds **why** a supply carries no tax:

```json
{ "standard": 1500, "exempt_reason": "VATEX-SA-30", "zero_reason": "VATEX-SA-32" }
```

A category says *that* there is no tax; only the taxpayer knows *which article*
covers them. A landlord letting residential property is `VATEX-SA-30` (real
estate transactions); an exporter is `VATEX-SA-32`. The code is stamped onto
every line at issue time, exactly as the rate is, so a replay reproduces what
was declared rather than what is configured today.

**Issuing an exempt or zero-rated line is refused until one is set.** Not
defaulted: this build used to derive the reason from the category, which
declared every exempt supply in the system to be a financial service.

`ledger` stores the code and never interprets it — the list is ZATCA's and
`tax_sa` owns it, the same division `crm::TaxRegistration.scheme` already uses.

`http.rs` is translation only. The aggregates, the invariant and the read models
are the module; that file turns a request into a call and a result into JSON.

## The fiscal calendar

```rust
pub struct FiscalCalendar { pub starts_on: NaiveDate, pub pattern: Pattern }
pub enum Pattern { Monthly, Quarterly, FourFourFive, FourFiveFour, FiveFourFour, Yearly }
pub struct Period { pub id: String, pub year: i32, pub index: u32,
                    pub from: NaiveDate, pub until: NaiveDate }

impl FiscalCalendar {
    pub const KEY: &'static str = "ledger.fiscal_calendar";
    pub fn year_start(&self, year: i32) -> NaiveDate;
    pub fn fiscal_year_of(&self, day: NaiveDate) -> i32;
    pub fn periods(&self, year: i32) -> Vec<Period>;
    pub fn period_containing(&self, day: NaiveDate) -> Period;
    pub fn period(&self, id: &str) -> Option<Period>;
}

pub async fn fiscal_calendar(conn: &mut PgConnection) -> Result<FiscalCalendar, ConfigError>;
pub async fn set_fiscal_calendar(conn: &mut PgConnection, calendar: FiscalCalendar,
                                 by: Option<&str>) -> Result<FiscalCalendar, CalendarError>;
```

**The calendar is the tenant's** (decided 2026-09-14): a start date and a
pattern, and every period is generated from the two rather than typed in. Month
patterns start each period on the start date's day of the month, clamped where a
month is short. The 4-4-5 family counts weeks — each quarter four, four and five
of them, or 4-5-4, or 5-4-4 — so a year is 52 weeks and every few years 53; each
fiscal year starts on the start date's weekday nearest its anniversary, which is
what stops a week calendar drifting, and the 53rd week goes in the last period. A
fiscal year is named by the calendar year it starts in; periods read `2026-P03`.
A tenant that never chose gets calendar months from 1 January.

Periods are computed, not stored, so nothing here says which are closed: that is
the watermark above, moved by the period close. **A change is a segment.** A
calendar that changed under a closed year would change what its closed periods
*were*, so `FiscalCalendars` keeps a list: a new calendar takes effect from its
start date, which must be in open time and on a fiscal-year boundary of the
calendar before it (400 `ledger.not_a_year_start` otherwise, 409
`ledger.calendar_locked` inside closed time). Every year is generated by the
segment in force for it; the first segment reads all history before the second,
whatever its own anchor; while nothing is closed a change simply replaces. A
short transition year is not supported.

## Statements

```rust
pub async fn balances_at(conn, until: Option<Timestamp>) -> Result<Vec<AccountBalance>, _>;
pub async fn profit_and_loss(conn, from: Timestamp, until: Timestamp,
                             branch: Option<&str>) -> Result<Vec<StatementLine>, _>;
pub async fn balance_sheet(conn, as_at: Timestamp,
                           fiscal_year_start: Timestamp) -> Result<SheetParts, _>;
pub async fn journal(conn, filter: &JournalFilter, limit: i64,
                     after: Option<&Cursor>) -> Result<Page<JournalEntryView>, _>;
pub async fn journal_entry(conn, id: &str) -> Result<Option<JournalEntryView>, _>;
```

Every figure is a sum over `posting` at the moment it is asked, never a
maintained total — the same argument the balance views make — and every range is
exclusive at the far end, the convention the VAT return and `closed_before` use.
**One statement per currency**: postings carry a currency and nothing here
converts, so a tenant with riyal and dollar postings gets two of each. Converting
to one currency was **deferred on 2026-09-15**: the business handles its foreign
currency itself, and the one place that could not be left to it — a Saudi tax
document — must be in riyals (`sales::DocumentCurrency`).

The balance sheet carries two equity lines no account holds: the trading result
since the fiscal year started, and the result of every year before it. Neither
has been closed into retained earnings — the formal close will post that entry —
so the sheet computes them from the trading accounts at the instant asked, split
at the fiscal year's start on the tenant's own calendar. **It is refused, not
rendered, when the postings up to that instant do not sum to zero**: the ledger's
one invariant, and a sheet that did not balance would be a sheet somebody acts
on. The profit and loss takes a branch (a confined member gets theirs); the
balance sheet is company-wide only, because a branch's books do not balance on
their own.

The journal lists entries newest first, paged on `(occurred_on, entry_id)` so a
page is stable under new postings, filterable by range, by an account the entry
touches, and by branch.

## What is deliberately absent

Drafts, and posting rules driven by configuration. Each is real, and each needs
somebody to want it before its shape is decided. Fiscal periods and the close
were wanted on 2026-09-14 and are built; cost centers are next. FX was wanted
and then deferred on 2026-09-15 — per-currency books, the exchange itself left
to the business — until a tenant asks for the gain or loss on it.
