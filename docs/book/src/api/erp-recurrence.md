# erp-recurrence

When something repeats: **which days, and between which two times on those
days.**

**Depends on:** `erp-occupancy` (for `Span`), `erp-eventlog`, `erp-i18n`.
**Depended on by:** `booking`, `hr`.

## Why this is a crate

Opening hours, a stylist's shifts, a room's out-of-service week, a studio's
Ramadan timetable and an employee's working pattern are one shape.

It lived in `booking` while `booking` was the only thing that needed it — and
`erp-occupancy` said so at the time, in a comment that is now half wrong: *"when
a resource is offered is a recurrence, and it belongs in `booking`."* True then.

`hr` needs the same shape for a shift and cannot reach it there, because
**`booking` already depends on `hr`**: a bookable resource names an employee, so
a lapsed work document stops the rota. The other direction would close a cycle.

So it moves below both — the same argument that made `erp-occupancy` a crate
rather than part of `booking`. One idea two modules need belongs underneath
them.

## The shape, and the one place it diverges from cron

The plan specified the recurrence as *"months, weekdays, days, hours, minutes as
bit fields"*. That is cron, and **cron cannot say "half past nine"**.

Cron matches an instant when every field matches, so hours and minutes are
independent sets. "Open 09:30 to 17:00" would need hours `{9..16}` and minutes
`{30..59} ∪ {0..29}` — which is every minute, and so also matches 09:05. No
assignment of those two fields means what a salon means.

So the calendar half stays bit fields, which is what makes it compact and
indexable, and the clock half becomes what it actually is: two
minutes-past-midnight bounds, half-open like every interval here.

```rust
Availability::from_parts(
    &[],              // months: empty is every month
    &[1,2,3,4,5],     // weekdays: 1 is Monday
    &[],              // days of the month: empty is every day
    9 * 60, 17 * 60,  // minutes past local midnight; the close is exclusive
    None, None,       // from, until — the until is inclusive
)
```

**A rule that runs past midnight is refused**, and is written as the two rules it
is. Allowing the wrap would make every day test in the file ask "or is it
yesterday's rule still running", which would then have to be right in six places.

## One clock, and whose it is

A `Span` is UTC because instants are; opening hours and shifts are local. The
tenant's offset is `Calendar`, and its key is **`tenant.calendar`**, not any one
module's — a business has one timezone, and both the diary and the rota read it.

```rust
pub const KEY: &'static str = "tenant.calendar";
pub const RIYADH: Self = /* +03:00 */;
```

A fixed offset, not a timezone database: Saudi Arabia is `+03:00` all year and
so is every Gulf market next to it. A market with daylight saving needs
`chrono-tz` and a named zone, and that is a change to one file.

## Why the error codes changed

They were `booking.not_a_window` and six more. A shift refused with a code naming
a module the tenant may not have enabled is a client's problem, so the codes
moved with the type and are `recurrence.` now.

**A code is a client-facing identifier and this API tells clients to branch on
it**, so that is a breaking change. It was free because nothing is released, and
it would not have been in six months.

The same pass separated a second namespace that had grown into the first: `hr`
claims are `hr:approve_leave` with a colon, because `hr.approve_leave` is what an
error code looks like — and the openapi guard read one as the other, which is
exactly the confusion the shape invites.

## What is deliberately not here

**Anything that claims capacity.** A rule says when something is *offered*;
whether one more fits is `erp-occupancy`. Keeping the two apart is what lets a
rule be a pure predicate over a `Span`.
