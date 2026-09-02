# hr_sa

What the Kingdom requires of an employer: GOSI, and end of service.

**Depends on:** `hr`, and the core.
**Depended on by:** nothing.

## Why this is a country module, mirroring `tax_sa`

For the same reason VAT is not in `sales`: a country's rules are that country's,
and a payroll module that knew about GOSI would have to learn every other
country's equivalent. `payroll` computes what a business pays its people; this
computes what the Kingdom then requires of it.

## The rates are configuration, and that is the load-bearing decision

**Read this before trusting a number out of this module.**

GOSI's schedule is set by the authority and has changed — most recently for
people entering after the 2024 pension reform, who are on a different and rising
scale from those already in it. A build that hard-coded a percentage would be
quietly wrong for some employees from the day it shipped, and for everybody
eventually.

So the rates are a tenant configuration value with shipped defaults, exactly as
`tax_sa` treats the Saudi VAT rate as seeded data rather than a constant. **The
defaults must be checked against the authority's current schedule before a tenant
runs payroll against them.**

`GET /v1/hr_sa/gosi/schedule` answers with `configured`, which says whether
anybody has confirmed the numbers or the tenant is still on what shipped. That
field exists so nobody discovers the answer on a payslip.

```rust
pub struct Schedule {
    pub saudi_employee_bp: u32,      // withheld
    pub saudi_employer_bp: u32,      // borne by the business
    pub non_saudi_employee_bp: u32,  // zero: hazards cover is the employer's
    pub non_saudi_employer_bp: u32,
    pub ceiling_minor: Option<i64>,
}
```

**The ceiling caps the base, not the contribution.** Capping the contribution
instead would give the employee and the employer different effective bases —
the subtle version of this bug, and the one a payslip does not show.

`Footing` — Saudi or not — is **a fact about the person, stated rather than
inferred**. Nothing here can work it out from a name or an iqama number, and a
module that tried would be wrong about somebody on their first payslip.

The **base** is basic plus housing, not the whole salary and not net. Which
allowances count is a question about the contract and the authority's
definition, so the caller passes the figure rather than this guessing from a
`Salary`.

## End of service

Articles 84 and 85 of the Labour Law, as a pure function. Two rules stacked.

**The entitlement**, on the wage at the end of service:

| service | award |
|---|---|
| each of the first five years | half a month |
| each year after that | one month |

**The reduction, when the employee resigns.** A dismissal pays in full; a
resignation is scaled:

| service on resigning | paid |
|---|---|
| under 2 years | nothing |
| 2 to under 5 | one third |
| 5 to under 10 | two thirds |
| 10 or more | in full |

The worked example everybody checks: 10,000 a month, ten years, dismissed. Five
years at half a month is 2.5; five more at a full month is 5. Seven and a half
months — **75,000**.

### Integers, and one rounding

```text
(days in the first five years × 5,000  +  days after that × 10,000)
────────────────────────────────────────────────────────────────────  of one month
                        365 × 10,000
```

One numerator over one denominator, so `Money::apportioned` rounds **once**, away
from zero. Computing months as a decimal and rounding along the way is how a
gratuity comes out a halala short of what the employee's own calculator says —
and this workspace forbids floating-point arithmetic for exactly that reason.
The first version of this used `f64` and the lint refused it, which turned out to
be a precision improvement as well as a rule: 912 days of service is
1.24931506… months, not the 1.2493 the decimal version produced.

**365 days and not 360.** Some Gulf practice uses a 360-day year for
convenience; the Labour Law speaks in years and months, and a calendar year is
what a court would read. A tenant who needs the other convention has an argument
to make, and it should be an argument rather than a constant nobody noticed.

**Rounding goes to the employee.** Rounding a statutory entitlement downward is
the version that ends up in front of a labour office.

### What the caller decides, not this module

**Which wage.** The award is on "the last wage", and what counts — basic alone,
or basic plus which allowances — is a question about the contract. The route
uses `Salary::gross`, which is basic plus allowances, because basic alone is the
common shortcut and it underpays.

**Why they left.** Article 87 pays a woman leaving within six months of marriage
or three of childbirth in full; Article 80 dismissals for cause pay nothing.
Both are facts about *why*, which this module is not told and must not guess —
so they are `Leaving::InFull` and `Leaving::ForCause`, stated by the caller.

## Leave entitlement

The half `hr` deliberately does not have. **`hr` records what was taken**, which
is the same in every country; **this says what was owed**, which is Article 109
and Article 117.

### Annual leave

Twenty-one days a year, rising to **thirty after five years' service** with the
same employer, pro-rated for a part year — somebody who joined in July is owed
half a year's worth because the law says so, not because a business is generous.

**The step can land mid-year**, so the entitlement is the two rates over the days
each applied to:

```text
(days below five years × 21  +  days above × 30)
────────────────────────────────────────────────  , rounded up
                     365
```

An implementation that picked one rate by comparison would be wrong by up to
nine days in exactly the year somebody notices. Rounding goes **up**, for the
reason the gratuity's does: rounding a statutory entitlement down is the version
that ends up in front of a labour office.

`GET .../leave-entitlement` combines both halves and answers `annual_left`,
which **may be negative**. Somebody who took three weeks in January and left in
March has overdrawn, and clamping it to zero would hide money the business is
owed back.

### Sick leave

Article 117 is a **pay scale, not a day count**: the first thirty days at full
pay, the next sixty at three quarters, the thirty after that unpaid — one
hundred and twenty in a year, after which there is no further entitlement.

```rust
pub fn sick_days(already_taken: i64, days: i64) -> SickPay
```

`already_taken` is what makes the bands land right: **a second illness in the
same year does not start again at full pay**. Twenty-five days used and ten more
is five at full and five at three quarters, not ten at full.

`SickPay::value` prices it, with three quarters as `apportioned(3, 4)` — the one
rounding policy this codebase uses, which here rounds toward the employee.

Running past the entitlement is reported by `exhausts_entitlement` and **not
refused**: being ill longer than the law provides for is a real thing, and what
follows is a conversation rather than an error.

## No aggregate, no projection, no schema

This module holds **no state**. Every function is arithmetic over what it is
given, and the one thing it stores — the GOSI schedule — is a configuration
value in the shared store.

That is why `install` does nothing and there is no projection group. Two guards
in the worker had to learn it: "every module has a projection job" and "every
module can be rebuilt" now key off `setup.groups.is_empty()` rather than a list
of exceptions, so the next arithmetic-only module needs no edit.

## WPS is not built, and that is deliberate

The monthly salary file the Ministry mandates has a specification — field order,
encoding, and each bank's own variations — that this build cannot verify from
where it stands. **A file that is almost right is a file the bank rejects on the
day wages are due.**

It is the same position `tax_sa` was in before somebody had a sandbox to submit
against, and it is recorded as not built rather than guessed at.

## Routes

See [The HTTP API](./http.md#gosi-and-end-of-service).
