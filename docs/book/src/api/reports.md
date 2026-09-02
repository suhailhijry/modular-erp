# reports

Figures that agree with the books.

**Depends on:** `ledger`, and the core. It links `sales`, `booking`, `pos` and
`payroll` for their **event shapes** and reads none of their read models.
**Depended on by:** nothing.

## Why this is a module and not four screens reading four groups

A dashboard mixing sales, bookings, takings and payroll looks like it must read
four projection groups. **L3 forbids that**, and not out of tidiness: a group is
the unit of consistency, so four checkpoints can sit at four positions, and a
total read across them is a number that was never true at any moment.

The system this phase was read against made exactly that mistake. Its projectors
declare which *other projections* they read, and it needed a bespoke check to
police the rebuild order that created.

So a report module **subscribes to the log**. It decodes `sales::InvoiceEvent`,
`booking::ReservationEvent`, `pos::ShiftEvent`, `payroll::RunEvent` and
`ledger::JournalEntryEvent`, and maintains its own group on one checkpoint.
Every figure on a screen built from it was true at one position, together.

## What that costs, honestly

**It keeps its own copies of what it needs**, in working tables nobody reads:

| Table | Why it exists |
|---|---|
| `invoiced` | `sales.invoice.cancelled` carries the credit note, not the invoice's amounts — rightly, because they have not changed. Netting a credit off has to remember what the issue put in. |
| `held` | `booking.reservation.moved` carries a stage and nothing else — the whole lifecycle is one event — so attributing a completion to the resources it took has to remember what they were. |
| `till` | `pos.shift.sold` carries tenders, not the operator, because the operator has not changed since the shift opened. |
| `drafted` | `payroll.run.approved` carries the journal entry and the time, because approving does not change what anybody is paid. |
| `entry` | The books, so the reconciliation below is not a cross-group read. |

Reading them back during `apply` is a database read inside a projection, which
`crates/erp-projection/tests/purity.rs` otherwise forbids. It is allowed here
under one condition, declared on the line above each read: the row was written
by **this same projection, earlier in log order**, so a rebuild reproduces the
live run exactly. `the_demo_replays_to_exactly_what_is_live` is what holds that
down.

The alternative is a report that is occasionally wrong in a way nobody can
reproduce. That is the more expensive option.

## A discrepancy is a failure, not a coloured cell

`reports::reconciles` returns what this module says that the books do not.
**Empty is healthy**, and anything else stops something: the worker's health
check (`reports_reconcile`) makes the tenant unhealthy, and
`every_figure_agrees_with_the_books` fails the build. Nothing renders a
discrepancy in amber and carries on — that is L6.

Two things are compared, both exact and neither depending on a chart of
accounts:

1. **The trial balance.** Every currency's postings sum to zero. This is the
   ledger's own invariant asserted from the report's side, which is the point:
   if this pipeline applied an event twice, the ledger would still balance and
   this would not.
2. **Every document against the entry it posted.** The debits of the journal
   entry an invoice made equal what the invoice came to, and the same for the
   entry that credited it. `sales::issue_entry_of` and `sales::credit_entry_of`
   are public so this module can name those entries without reimplementing
   `sales`' scheme.

Being account-agnostic is what makes it usable: a tenant who renamed their
revenue account, or enabled `prepaid` — which moves money in and out of revenue
as packages are redeemed — produces no false alarm. An invariant that fires on
something normal is one somebody switches off, and then it is protecting
nothing.

### What it deliberately does not report

An invoice at the very tail of what has been projected. An invoice and its
journal entry commit together and take consecutive log positions, but a
projection batch may end between them — so the document is applied and the entry
is one position away, unapplied. Reporting that as "made no entry" would be
reporting a batch boundary as a broken ledger. `invoiced.position` is what
excludes it.

## The figures

| Route | Answers | Who may read it |
|---|---|---|
| `GET /v1/reports/revenue` | What was sold, by month and branch, **net of credit notes** | everyone |
| `GET /v1/reports/utilisation` | Booked, completed, no-shows, cancelled, minutes, and lead time per resource | everyone |
| `GET /v1/reports/takings` | What the tills took, by month, person and method, with variance and what was paid out | everyone |
| `GET /v1/reports/people-cost` | The wage bill, from **approved** runs only | owner, accountant |
| `GET /v1/reports/reconciliation` | Whether these figures agree with the books | owner, accountant |

Every range is `from`/`until` as `YYYY-MM`, **both inclusive** — a report is read
by month, and an exclusive end is how a chart comes to be missing December. Ten
years is the most one request may ask for.

The wage bill is not a viewer's to read. What the people in the room are paid is
not a figure a receptionist with a dashboard should be able to total, which is
the one place this module's permissions differ from the rest of it.

## Periods are the month the event says

An invoice dated the 31st and entered on the 2nd belongs to the month it was
dated to, which is the same argument the ledger makes about `occurred_on`. A
credit note lands in the month the **credit** was dated to and against the
**invoice's** branch: a December invoice credited in January is December revenue
that January took back, and moving it would restate a month somebody has already
filed a return against.

Lead time is domain time to domain time for the same reason. `ctx.event_time()`
is when the append committed, which for a booking written up at the end of the
day would say a month's notice was none.

## What is deliberately absent

- **Revenue by product.** It needs invoice lines, which means a second working
  table the width of every line ever issued. Nobody has asked for it, and the
  same question is answerable per document today.
- **Headcount, and documents about to expire.** `hr` answers both from its own
  group, and no cross-group total is involved — so a copy here would be
  duplication for its own sake. The expiry warning already reaches somebody as a
  worker health finding.
- **Against what was banked.** Nothing in this system has seen a bank statement.
  `takings.paid_out` is the cash that left the drawer and was not a refund,
  which is as close as the log comes, and it is named for what it is.
