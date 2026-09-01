# booking

Reservations: who is booked with whom, when, and what that takes.

**Depends on:** `erp-occupancy`, `crm`, plus the core.
**Depended on by:** nothing yet.

## The wedge

Three of the tools the market leader sells are *integrations to* accounting
systems — Qoyod, Odoo, Daftra. Nobody builds three of those with a ledger of
their own. So the competitor with the booking product has no books, the
competitors with books have no booking, and a salon today buys both and
reconciles them by hand.

This module is the half the accounting vendors do not have, in the same database
as the books.

## What is here and what is a layer down

`erp_occupancy` answers one question: **does one more fit?** It knows a resource
has a capacity and that intervals overlap, and nothing else.

This module knows what is being booked, who it is for, what stage it has
reached, when a resource is *offered*, and what it costs — and it calls the
engine for the one question the engine owns.

That split is what makes a salon, a clinic, a restaurant, a hotel, a class and a
museum the same code. **The moment a rule in here branches on what kind of thing
is being booked, that has stopped being true.**

## The files

| File | What is in it |
|---|---|
| [`reservation.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/reservation.rs) | `Reservation`, `ReservationEvent`, `Stage`, `Line`, `DraftLine`, `Held`, `Customer` |
| [`resource.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/resource.rs) | `Resource`, `ResourceEvent`, `Kind` |
| [`availability.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/availability.rs) | `Availability`, the recurrence |
| [`pricing.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/pricing.rs) | `price`, `Charge`, `Charged`, `Tariff`, `Band`, `Allowance` |
| [`calendar.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/calendar.rs) | `Calendar`, the tenant's offset |
| [`trades.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/trades.rs) | Six blueprints |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/commands.rs) | Ten commands |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/projections.rs) | The `Booking` group: the rota and the diary |
| [`http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/booking/src/http.rs) | Seventeen operations |

## One lifecycle, once

```rust
pub enum Stage { Reserved, Confirmed, Arrived, InService, Completed, Cancelled, NoShow }

impl Stage {
    pub const fn allows(self, next: Self) -> bool;
    pub const fn is_over(self) -> bool;
    pub const fn frees_capacity(self) -> bool;
}
```

That system spells this out three times over — once for seats, once for showers,
once for services — and most of its seventy reservation events are that
repetition. It is one list, and `ReservationEvent::Moved` is the single event
that walks it.

That it reached the same five stages independently is the reason to trust them.

**Skipping forwards is allowed.** A walk-in arrives without ever being
confirmed, and a counter that only marks things done should not have to fake
three steps first. **Not turning up is only possible before turning up** —
marking somebody a no-show after they arrived is a contradiction, and one that
would make the no-show rate a number nobody could trust.

`frees_capacity` is true for cancelled and no-show, **false for completed**. A
finished appointment held that chair, and deleting the claim would make the past
look free.

### Why this is a match and not phantom types

The plan asked for typestate per architecture §4. It is an exhaustive `match` on
a pair of stages instead, and the reasoning is in the code: every command starts
from a `load`, so the stage is only ever known at run time, and phantom types
would buy one boundary check that is this same match with seven zero-sized types
stacked on it.

What the match does buy is the thing §4 actually wants: an eighth stage is a
compile error in the one place the rules live. Nothing else in this codebase
carries phantom typestate either — `Permit<C>` is where it earns its keep.

## The customer is a resource

Held at capacity one, under the reserved id prefix `customer.`, in the same
engine as every chair. That is the whole of *they are already in another chair*:
no second table, no special case, and the same concurrency guarantee a stylist
gets.

**Once per distinct span, not once per line.** Four seats at one showing is one
person at one time and must be allowed; a haircut at ten and a massage at half
past is one person in two places and must not be.

The consequence, found by the class fixture: twelve places in a class is either
twelve customers, or one customer on one booking. What is refused is one person
holding twelve *separate simultaneous* bookings — and it has to be, because a
system that allowed it would have nothing left to catch the salon double-booking
with. They are the same query.

## Availability

```rust
impl Availability {
    pub fn from_parts(months, weekdays, days, opens_at, closes_at, from, until)
        -> Result<Self, BadRule>;
    pub fn always() -> Result<Self, BadRule>;
    pub fn daily(opens_at: u16, closes_at: u16) -> Result<Self, BadRule>;
    pub fn covers(&self, span: Span, at: FixedOffset) -> bool;
}
```

The calendar half is bit fields — months, weekdays, days of the month, where
zero means *every*. The clock half is two minutes-past-midnight bounds,
half-open.

### Where this diverges from the plan, and why

The plan specified cron: months, weekdays, days, hours and minutes as bit
fields. **Cron cannot say "half past nine."**

Its hours and minutes are independent sets, so `09:30–17:00` would need hours
`{9..16}` and minutes `{30..59} ∪ {0..29}` — which is every minute, and
therefore also matches 09:05. There is no assignment of those two fields that
means what a salon means. The fields are not wrong for *days*; they are wrong
for *times*, because a time window is an interval and cron has no intervals.

A rule that runs past midnight is refused and written as the two rules it is.
Allowing the wrap would make every day test ask *or is it yesterday's rule still
running*, which would then have to be right in six places.

**An empty rule set is open.** A resource nobody has given a timetable to takes
bookings whenever, which is what a hotel room and a museum slot are.

**The whole span, not the start.** A ninety-minute treatment starting half an
hour before closing is not half-available.

## Pricing

```rust
pub fn price(charge: &Charge, band: Option<&Band>) -> Result<Charged, PriceError>;
```

**No database, no configuration, no clock.** Everything that varies is an
argument, so the arithmetic is testable without a tenant and cannot drift when
somebody changes a setting.

The impure half is `Tariff::band_for`, resolved inside the booking's own
transaction and **frozen onto the line** (L5) — so a tenant who moves their peak
hours next month does not restate what was booked this month.

**Bands, not prices.** What a service costs is the caller's to send; *when* it
costs more is the tenant's to configure, and that is the half a client must not
be able to decide for itself.

A band's `when` is an `Availability` — the same recurrence that says when a
resource is offered. "Open Thursday evening" and "dearer Thursday evening" are
one shape, and a tenant should learn it once.

### The order of operations, which is not free

The band moves the **rate**, then quantity multiplies, then allowances come off
the total.

A 33.33 service at a quarter more is 41.66 each, so four are 166.64. Banding the
total instead gives 166.65 — and a customer who checks the arithmetic finds a
halala nobody can explain. Both numbers are in
`a_band_is_applied_to_the_rate_and_not_to_the_total`, the wrong one labelled as
such.

### Tax-exclusive, and no tax here

An allowance comes off the **net**. No tax is computed in this module, and that
is the point: a reservation is not a tax document. The allowances travel with
the line to `sales` when it is invoiced, where they reduce the band they come
off — so the tax-exclusive property falls out rather than being something two
modules each have to remember.

`Money` throughout, never a float. That system's engine takes floating-point
amounts and its own docblock records three implementations that disagreed, every
fixed discount differing by exactly the tax on it. The one place a rate is
applied here is `Money::scaled_by`, which is the only such place in the
workspace and says what its rounding is.

## Local time, and the ceiling on it

```rust
pub struct Calendar { /* minutes east of UTC */ }
impl Calendar {
    pub const KEY: &'static str = "booking.calendar";
    pub const RIYADH: Self;
    pub async fn resolve(conn) -> Result<Self, ConfigError>;
    pub fn offset(self) -> FixedOffset;
}
```

A rota is local and an instant is not, so something has to say what "nine in the
morning" means here.

A fixed offset, not a named zone. Saudi Arabia is `+03:00` all year and so is
every Gulf market next to it, so this is exact for where it ships and needs no
timezone database in the binary. A market with daylight saving needs `chrono-tz`
and a zone name, and that is a change to `calendar.rs` and to nothing else,
which is why the offset is resolved there rather than passed around.

## Commands

```rust
// The rota
pub async fn declare_resource(db, id, details, at, metadata)  -> Outcome<ResourceEvent>;
pub async fn amend_resource(db, id, amendment, at, metadata)  -> Outcome<ResourceEvent>;
pub async fn schedule_resource(db, id, availability, at, metadata) -> Outcome<ResourceEvent>;
pub async fn withdraw_resource(db, id, why, at, metadata)     -> Outcome<ResourceEvent>;
pub async fn restore_resource(db, id, at, metadata)           -> Outcome<ResourceEvent>;
pub async fn fit_out(db, trade, locale, at, metadata)         -> Result<FittedOut, _>;

// The diary
pub async fn reserve(db, id, booking, metadata)               -> Outcome<ReservationEvent>;
pub async fn move_to(db, id, stage, why, at, metadata)        -> Outcome<ReservationEvent>;
pub async fn reschedule(db, id, lines, at, metadata)          -> Outcome<ReservationEvent>;
pub async fn assign(db, id, line, unit, at, metadata)         -> Outcome<ReservationEvent>;
```

**None of these use `TenantDb::execute.`** Every one writes an event *and* moves
capacity in `erp_occupancy`, and the two have to commit together. A reservation
that exists without its claim is a double-booking waiting to happen; a claim
without its reservation is a chair nobody can free.

Each retries only a lost optimistic-concurrency race, and `settle` is the one
place that decides commit, roll back or retry.

### Idempotency, and the one that matters most

Reserving the same id twice is a no-op, and the second call **takes no claims**.
That gate matters more here than anywhere else in the codebase: taking them
again would either collide with the reservation's own rows or, on a resource
with room, quietly book it twice for one customer.

Moving to the stage it is already in is a no-op, so a retried *mark them
arrived* is harmless. Assigning the same unit again is a no-op; a different one
replaces it.

### Rescheduling, and why it is one command

`reschedule` moves a booking in time **and** onto different resources, because
underneath they are one operation: give back everything this reservation holds,
then take what it wants. Giving back first is what stops a booking colliding
with where it already was, so nudging an appointment ten minutes later works
instead of being refused by its own claim.

The claim set is always rebuilt **from the log** by `rehold`, never from what
the caller sent, so the claims and the events cannot disagree.

### Fungible pools

A hotel books "a double" and gives out room 302 at check-in; a salon books "any
stylist" and names one on the morning. The pool holds the **count** and the unit
holds the **identity**, so assigning takes a second claim on a different
resource and nothing is counted twice.

## Blueprints

Six trades in `trades.rs`, each a `const`: salon, restaurant, hotel, studio,
gym, museum. `fit_out` runs `declare_resource` and `schedule_resource` — the
same two commands a person clicking through the screens would run — so a trade
cannot produce anything the domain would refuse (D8).

They are also the module's own test. `tests/fixtures.rs` fits a tenant out from
each and books the characteristic thing:

| Trade | What it proves |
|---|---|
| salon | capacity 1, a named person plus their chair |
| restaurant | covers are the capacity |
| hotel | the pool and the unit are different resources |
| studio | capacity N, many people in one hour |
| museum | pure capacity, nobody assigned |
| gym | occupancy is optional — the rota holds the classes and nothing for the floor |

**Nothing in the module reads a trade's id.** A seventh trade is an entry in
`TRADES` and no code at all. All six passed against the module unchanged, which
is what the phase was for.

## Read models

`proj_booking` holds `resource`, `reservation` and `reservation_line`. One group
and not two, because the screen every one of these businesses opens on shows
both at once — a column per stylist, a booking in each — and a group is the unit
of consistency (L3).

```rust
pub async fn resources(conn, include_withdrawn, limit, after)      -> Page<ResourceSummary>;
pub async fn resource(conn, id)                                    -> Option<ResourceDetail>;
pub async fn reservations(conn, from, until, stage, limit, after)  -> Page<ReservationSummary>;
pub async fn reservation(conn, id)                                 -> Option<ReservationDetail>;
```

The diary's window is half-open and matches the way a claim overlaps — `ends_at
> from AND starts_at < until` — so a booking that straddles midnight shows up on
both days rather than on whichever one it happens to start in.

The customer's name is **frozen onto the reservation**, and it has to be:
`proj_booking` may not read `proj_crm` (L3), so a calendar showing the current
name would be joining two checkpoints that can disagree.

**These tables are the shadow; the engine is the record.** If the two ever
disagree the engine is right, because the engine is what a booking was accepted
against.

## What is deliberately absent

**No ledger posting.** Invoicing a completed booking is a later phase. A
reservation carries a price and no journal entry.

**No service catalogue.** `what` on a line is opaque text, which is what lets
pricing be bands rather than a price list, and what keeps the engine from
knowing what a service is.

## Routes

See [The HTTP API](./http.md#booking).
