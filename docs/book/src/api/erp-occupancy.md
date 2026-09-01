# erp-occupancy

Capacity over time. One question, answered under a lock: **does one more fit?**

**Depends on:** `erp-types`, `erp-i18n`.
**Depended on by:** `booking`.

## The concept it exists to hold once

A chair, a shower, a treatment room, a restaurant table, a hotel room type, a
class, a museum time slot and the stylist who does the work are one thing: a
resource with a capacity, and intervals during which some of that capacity is
held.

That was found by reading a working booking ERP — a Laravel system of roughly
407k lines, 74 aggregates and 863 event classes, serving salons and spas. Its
reservation aggregate writes the same lifecycle three times over,
`SeatActivated` / `ShowerActivated` / `ServiceActivated`. But its
`slot_occupancy` table is already generic: `(resource_type, resource_id, [start,
end), owner_type, owner_id)`. Somebody found the abstraction, applied it to the
write path, and never took it back into the domain model.

This crate is that concept, taken back.

## Why it is a crate and not a module

Modules own projections, and a projection is a pure function of the log (L2)
that can be dropped and rebuilt at will. Occupancy is the opposite kind of
thing.

**A read model can be rebuilt; an accepted booking cannot be un-accepted.**

These rows are write-side state, consulted inside the transaction that adds to
them — the same category as `erp_eventlog::numbering`, and they live in the
tenant migration chain for the same reason, where `rebuild_schema` cannot reach
them. `erp_projection`'s `tables_in` scopes by `schemaname`, so a shadow rebuild
cannot see `public.occupancy_claim` even by accident.

It follows that nobody enables `occupancy`. A tenant enables `booking`, and
`booking` links this.

## The files

| File | What is in it |
|---|---|
| [`lib.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-occupancy/src/lib.rs) | All of it: `Span`, `Claim`, and the five functions |
| [`messages.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-occupancy/src/messages.rs) | Six codes, English and Arabic |
| [`0007_occupancy.sql`](https://github.com/suhailhijry/modular-erp/blob/main/migrations/tenant/0007_occupancy.sql) | `occupancy_resource`, `occupancy_guard`, `occupancy_claim` |

## Span

```rust
pub struct Span { /* private */ }

impl Span {
    pub fn new(from: Timestamp, until: Timestamp) -> Result<Self, BadSpan>;
    pub const fn from(&self) -> Timestamp;
    pub const fn until(&self) -> Timestamp;
}

pub const MAX_SPAN_DAYS: i64 = 366;
```

Half-open, `[from, until)`. That is what makes back-to-back bookings work
without a fudge: a claim ending at 11:00 and one starting at 11:00 do not
overlap, so a salon can run appointments end to end and a hotel's checkout and
check-in can share a date.

**Normalised on construction.** Both ends truncate to whole seconds, because
that is the granularity anything here is booked at and because a comparison
between two representations of the same moment that disagree in the sixth
decimal place is how an overlap check silently passes. That system stores
wall-clock times and records exactly that failure in its own source: *"comparing
those unnormalised is how an overlap check silently passes."*

`#[serde(try_from = …)]`, which is architecture §4's proof-carrying constructor.
A span decoded out of an event goes through `Span::new` again, so a bad
migration surfaces as a decode error instead of a booking whose interval nothing
in this crate would have accepted.

`MAX_SPAN_DAYS` is not a business rule. It bounds the number of guard rows one
claim locks, which is one per day it touches and one round trip each.

## Claims

```rust
pub struct Claim {
    pub resource: AggregateId,
    pub span:     Span,
    pub quantity: u16,
}

impl Claim {
    pub const fn one(resource: AggregateId, span: Span) -> Self;
    pub const fn many(resource: AggregateId, span: Span, quantity: u16) -> Self;
}
```

`u16` because the largest thing anyone here books is a hall, and because it
converts to the column's `INTEGER` without a fallible step.

## The five functions

```rust
pub async fn declare(conn, resource: &AggregateId, capacity: u16) -> Result<(), OccupancyError>;
pub async fn take(conn, owner: &AggregateId, claims: &[Claim])    -> Result<(), OccupancyError>;
pub async fn release(conn, owner: &AggregateId)                   -> Result<u64, OccupancyError>;
pub async fn reschedule(conn, owner: &AggregateId, claims: &[Claim]) -> Result<(), OccupancyError>;
pub async fn free(conn, resource: &AggregateId, span: Span)       -> Result<u16, OccupancyError>;
```

**Every one of them must be given a transaction.** The guards are row locks, and
a row lock outside a transaction is released at the end of the statement that
took it, which would make the whole thing silently do nothing.

**Roll back on a refusal.** `take` writes each claim before probing the next, so
a batch refused halfway leaves the first half in your transaction. The rollback
is what makes a booking all or nothing; this crate never opens a transaction
behind your back, exactly as `sales::issue_in` does not.

```rust
let mut tx = db.begin().await?;
occupancy::declare(&mut tx, &chair, 1).await?;           // once, when set up
occupancy::take(&mut tx, &reservation, &claims).await?;  // with the booking
tx.commit().await?;
```

Capacity `0` is legal and means out of service — retirement without a second
column and without losing the claims already against it.

## The conflict test is a peak, not a sum

The plan specified `SUM(quantity) over overlaps + new > capacity`. That is wrong
for capacity greater than one, and wrong in the direction that refuses real
business:

> A room type with eight units and eight one-night stays spread across a week
> sums to eight, so a guest asking for the week is turned away — even though
> only one room is taken on any given night.

The sum counts claims that never coexist. So the claims become `+quantity` at
each start and `-quantity` at each end, ordered, and run through a running
total. The largest value that total reaches is what is actually held at once,
and it is exact.

Ordering by `(at, delta)` puts the decrements first at an equal instant, which
is the half-open rule again: a claim ending at 11:00 has let go before one
starting at 11:00 takes hold.

## The guards, and the two sorts

A row per `(resource, UTC date)`, locked `FOR UPDATE` **in sorted order** before
anything is read.

Two requests each naming resources A and B, one taking them A-then-B and the
other B-then-A, each hold what the other wants and Postgres kills one. That is
that system's recorded bug, and the fix is total order and nothing else.

**Both** the insert and the lock are sorted. `ON CONFLICT DO NOTHING` waits on a
conflicting insert that has not committed, so the deadlock is reachable in the
insert, before a single `FOR UPDATE` runs. Removing either sort fails
`a_deadlock_is_not_reachable`.

UTC and not the tenant's timezone: two intervals that overlap are live at some
instant, and that instant falls on a date both of them touch. The property holds
in any single consistent calendar, and choosing the tenant's would mean this
crate had to learn what a tenant is.

## The batch is checked against itself, structurally

`take` writes each claim **before** probing the next, so the second sees the
first.

That system probed the whole request and then wrote the whole request, so a
booking naming the same chair twice at the same hour found nothing already held,
wrote both claims, and double-booked the chair against itself. Here there is no
separate self-check to forget.

A repeat accumulates rather than colliding: three lines of one reservation each
taking one place in the same class at the same hour is one owner holding three.
That was found by `booking`'s own fixtures — before the fix it arrived as
`duplicate key value violates unique constraint`, for a booking that was
perfectly legal.

## What is deliberately absent

**Slot granularity.** Instants are stored; fifteen-minute slots are validation
and display, and belong to whoever draws the calendar.

**Buffers.** A cleaning or setup allowance widens the interval at claim time, so
the probe stays one comparison and this crate never learns the word.

**Availability and downtime.** When a resource is *offered* is a recurrence, and
it belongs in `booking`. All this answers is whether one more fits.

**Retirement.** `declare` with a capacity of zero says the same thing.

## The tests that carry it

`only_one_of_two_bookings_racing_for_the_last_place_gets_it` runs at capacity 1
with two contenders and capacity 3 with four, through a barrier so they collide
on purpose. Exactly capacity wins each time. Without the guard, the probe is a
read and two concurrent reads both see the place free.

`a_deadlock_is_not_reachable` runs twenty-five rounds of two bookings naming the
same two resources in opposite orders.

Every test in the file has been falsified by breaking the rule it covers.
