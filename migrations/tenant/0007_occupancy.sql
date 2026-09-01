-- Who holds what, and when.
--
-- One engine for a chair, a shower, a room, a table, a hall and the person who
-- does the work. That system — the Laravel booking ERP this phase was read
-- against — writes the same lifecycle three times over, `SeatActivated`,
-- `ShowerActivated`, `ServiceActivated`, and again for start, end, notes,
-- cancel and restore. Most of its seventy reservation events are one concept
-- spelt three ways. Its own occupancy table is already generic. This is that
-- table, taken back into the model where it belonged.
--
-- # Why this is not in a projection schema
--
-- Same argument as `0005_numbering.sql`, and it is the whole reason the engine
-- is here rather than inside a module. **A read model can be rebuilt; an
-- accepted booking cannot be un-accepted.** These rows are the write-side
-- record of what capacity has been given away, consulted inside the
-- transaction that gives away more, and `rebuild_schema` — which drops and
-- rebuilds `proj_*` — must never come near them.
--
-- That system says the same of its own: write-side state, never truncated,
-- never rebuilt by a replay. It is right.
--
-- # Why every tenant gets these tables
--
-- They are empty until somebody declares a resource, which costs nothing, and
-- the alternative is an entitlement for a thing no tenant would ever switch on
-- by name. A tenant enables `booking`; nobody enables `occupancy`.

-- A person, a place or a thing, and how much of it there is.
CREATE TABLE occupancy_resource (
    -- Client-chosen, like every other id here. The module that owns the
    -- meaning owns the id: `booking` may put a staff member's aggregate id in
    -- here, and nothing in this file knows or cares.
    id          TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),

    -- How many units may be held at any one instant.
    --
    -- **Not a boolean.** A stylist is 1, a class of ten is 10, a table seating
    -- six is 6, a room type with eight rooms is 8, a museum slot is 500. That
    -- system has no capacity at all, which is exactly why it fits one trade.
    --
    -- Zero is allowed and means the resource exists and can take nothing —
    -- a chair out for repair. Refusing zero would make the caller delete and
    -- re-declare, which loses the claims already against it.
    capacity    INTEGER NOT NULL CHECK (capacity >= 0 AND capacity <= 65535),

    declared_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The lock, and nothing else.
--
-- One row per resource per UTC day. A claim locks the row for every day it
-- touches, `FOR UPDATE`, **in sorted order**, before it looks at anything.
--
-- # Why a row and not the resource
--
-- Locking the resource would serialize every booking for that stylist across
-- all of time. Per day, March and July do not wait for each other.
--
-- # Why sorted, and why that is written in three places
--
-- Two requests, each naming resources A and B, one taking them in the order
-- A then B and the other B then A: each holds what the other wants and
-- Postgres kills one with a deadlock. That is that system's recorded bug. The
-- fix is total order, so `erp_occupancy::take` sorts the keys before it inserts
-- them and again before it locks them, and `a_deadlock_is_not_reachable`
-- fails if either sort is removed.
--
-- # Why there is no foreign key to `occupancy_resource`
--
-- Deliberate. A guard row is a lock, not a fact, and an orphan is harmless. It
-- also has to be insertable *before* the resource is checked: the check needs
-- the capacity, the capacity must be read under the lock, and the lock needs
-- this row. With a foreign key the whole thing inverts and an unknown resource
-- surfaces as a constraint violation instead of a sentence a user can read.
CREATE TABLE occupancy_guard (
    resource TEXT NOT NULL,
    on_date  DATE NOT NULL,
    PRIMARY KEY (resource, on_date)
);

-- One resource, one half-open interval, one quantity, one owner.
CREATE TABLE occupancy_claim (
    resource  TEXT NOT NULL REFERENCES occupancy_resource (id),

    -- Whoever is holding it: a reservation, a shift, a maintenance window. The
    -- engine never looks inside this. Release is by owner and nothing else,
    -- which is what makes a retried handler harmless (L8).
    owner     TEXT NOT NULL CHECK (length(owner) BETWEEN 1 AND 128),

    -- `[starts_at, ends_at)`. Half-open, so a claim ending at 11:00 and one
    -- starting at 11:00 do not overlap, and back-to-back appointments need no
    -- one-second fudge.
    --
    -- `TIMESTAMPTZ` is an instant, so there is no such thing as an
    -- unnormalised value in this column. That system stores wall-clock times
    -- and its own comment records what that costs: "comparing those
    -- unnormalised is how an overlap check silently passes."
    starts_at TIMESTAMPTZ NOT NULL,
    ends_at   TIMESTAMPTZ NOT NULL,
    CONSTRAINT occupancy_claim_is_half_open CHECK (ends_at > starts_at),

    quantity  INTEGER NOT NULL CHECK (quantity >= 1 AND quantity <= 65535),
    taken_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- One owner cannot hold the same resource from the same instant twice.
    -- Wanting two places is `quantity = 2`, which is one row. This also gives
    -- release the index it needs for free.
    PRIMARY KEY (owner, resource, starts_at)
);

-- What the probe reads: everything on this resource that has not finished yet.
--
-- `ends_at` leads because the interesting half of the predicate is
-- `ends_at > probe_start` — it prunes the past, which is where the table spends
-- the rest of its life. `starts_at < probe_end` is a recheck over what is left.
CREATE INDEX occupancy_claim_probe_idx ON occupancy_claim (resource, ends_at);
