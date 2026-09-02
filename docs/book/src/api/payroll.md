# payroll

What a business pays its people, and the entry it makes.

**Depends on:** `hr`, `ledger`, and the core.
**Depended on by:** nothing yet — `hr_sa` will stand on it for GOSI and the WPS
file.

## A run is a decision, not a derivation

What somebody is paid this month depends on their salary **as it was when the
run was made**, and a rise recorded next week must not restate last month's
payslip. So the amounts are frozen onto the run's events (L5) — the same
argument an invoice makes about the buyer's name, and for higher stakes: a
payslip is a document a person files with their bank.

The frozen copy includes their **name**. Somebody who marries next month does
not get a new copy of last month's payslip.

## Drafting and approving are two steps

**Drafting** computes what everybody would be paid and posts nothing.
**Approving** posts one journal entry.

A business reads the draft, finds the two people whose overtime is wrong, fixes
them and runs it over. A single-step run would have posted the first attempt to
the ledger before anybody looked at it.

Drafting again **replaces**: a run that accumulated drafts would pay somebody
twice. An approved run refuses — the entry is in the books and the payslips are
what people were told.

### A design mistake worth keeping

The first version used `try_create` and detected an existing run by catching
`AlreadyExists`. That variant is about *a different request reusing an id*, and
with no fingerprint on either side a second draft looked like a retry — so
redrafting silently did nothing and the run posted the draft nobody wanted.

`redrafting_replaces_the_previous_draft` caught it. The fix was not to handle
the case better but to stop needing the distinction: a run is named by the
caller, redrafting *is* the operation, so it is `try_execute` and there is no
create to disambiguate. The idempotency guard in `erp-api` flagged the same
thing from the other direction.

## One entry for the run, not one per person

A hundred employees is a hundred payslips and **one** journal entry.

```text
Dr  Salaries and wages      gross
    Cr  Salaries payable            net
    Cr  Payroll deductions          deductions
```

**Gross is the expense, and net is what is owed.** Deductions are money the
business is holding on somebody's behalf — a repayment of an advance, a loan
instalment — so they are a liability and not a reduction of cost. Netting them
against the expense would understate what the business spent on wages, which is
the number every management report is about.

The deduction line is omitted when it is zero, because a line that moves nothing
is a line nobody wants on a report, and most runs have none.

**The entry is dated to the last day of the period**, not the day the run was
approved. A February run approved on the 3rd of March belongs in February, and
the whole point of a period is that it does.

### The default that was wrong

`5100` is **Rent** in every shipped chart. The first version of
`PostingAccounts` used it for the wage expense, and the exit-criterion test
caught it — which is the kind of mistake that otherwise posts a year of wages
somewhere nobody looks until an audit. It is `5000 Salaries and wages`.

`2210 Payroll deductions` did not exist and now ships in every chart, for the
reason `2400` and `5910` do: a business running payroll with a single deduction
in its first month needs it in its first month.

## Who is in the run, and why the caller says

The caller supplies the list of employees. That looks like work the module
should do, and it is deliberate:

- Enumerating employees means reading `proj_hr`, and **L3 forbids a command
  reading another module's projection group**.
- A payroll run is money leaving the business, and must not be computed from a
  table that may be a second behind.

So the composition happens where every other cross-module read here happens — at
the read layer, in the client, which lists staff from `hr` and sends the ones to
pay. What the command then does is load each person's **aggregate** for their
salary, inside its own transaction, which is a write-side load and exactly what
L7 permits.

It is also honest about the domain: a payroll run is reviewed before it posts,
and who is in it is a decision somebody makes.

## Employed for the *whole* period

```rust
pub fn was_employed_throughout(&self, from: NaiveDate, until: NaiveDate) -> bool
```

Not "are they employed now". A May run approved in June must pay somebody who
resigned on the 10th of June, and must not pay a full month to somebody who left
on the 3rd of May.

**Somebody employed for part of the period refuses the whole run.** Pro-rating is
real arithmetic — working days or calendar days, and Saudi contracts differ — and
a run that silently paid a whole month to somebody who started on the 20th is the
error nobody catches until they are asked to give it back. So it stops (L6) and
names them.

The same applies to somebody with no salary recorded: the run refuses rather
than quietly paying them nothing. A run that left somebody out is a run somebody
notices on payday.

**Employment, not eligibility.** Somebody whose iqama lapsed mid-month is still
owed for the days they worked; refusing to pay them would turn a compliance
problem into wage theft.

## What is deliberately not here

**GOSI, and the WPS file.** Both are Saudi statute and belong in `hr_sa`, for the
reason VAT belongs in `tax_sa`: a country's rules are a country module's, and a
payroll module that knew about GOSI would have to learn every other country's
equivalent.

**The payment.** Money leaving the bank is a separate act, days later, in one
transfer covering everybody — and pretending the run paid people would say the
bank balance moved when it did not.

**Pro-rating**, per above.

## Routes

See [The HTTP API](./http.md#payroll).
