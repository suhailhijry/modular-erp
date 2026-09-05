-- The diary.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_booking` during provisioning and into a staging schema
-- during `rebuild_swap`. See `modules/ledger/schema/install.sql` for the full
-- argument.
--
-- # What is not here
--
-- The claims. Who holds which resource between which two instants is
-- `erp_occupancy`'s, it lives in the tenant migration chain, and it is
-- write-side state that a rebuild must never touch. These tables are the
-- readable shadow of that: what a calendar draws and what a receptionist
-- searches. If the two ever disagree the engine is right, because the engine is
-- what a booking was accepted against.

-- Everything that can be booked.
CREATE TABLE IF NOT EXISTS resource (
    id            TEXT PRIMARY KEY,

    -- Arabic for a Saudi business, which is why the Latin spelling is a
    -- separate optional column and not a translation.
    name          TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    name_latin    TEXT,

    -- Display and filtering. **No rule branches on this**, and the day one does
    -- the engine has stopped being general.
    kind          TEXT NOT NULL CHECK (kind IN ('person', 'place', 'thing')),

    -- The number the occupancy engine holds, mirrored here so a calendar can
    -- show "3 of 8 left" without reaching into write-side state.
    capacity      INTEGER NOT NULL CHECK (capacity BETWEEN 0 AND 65535),

    -- **Where it is.** Set once at declaration; null in a single-branch
    -- business and on every resource declared before branches existed. No
    -- foreign key: `proj_branches` is another projection group, and L3 forbids
    -- joining to it.
    branch        TEXT,

    -- Which member of staff this is, when the business keeps staff records.
    -- Null for a room, a chair, and for any business that has not linked its
    -- diary to its people — which changes nothing about how the diary works.
    employee      TEXT,

    -- The timetable, as the rules were written. JSONB because it is read whole
    -- and never queried into: the question "is this resource open then" is
    -- answered from the aggregate inside the booking transaction, not from here.
    availability  JSONB NOT NULL DEFAULT '[]'::jsonb,

    withdrawn_at  TIMESTAMPTZ,
    withdrawn_why TEXT,

    declared_on   TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- "Book at Olaya": the calendar's columns for one place.
CREATE INDEX IF NOT EXISTS resource_by_branch_idx
    ON resource (branch, kind, name, id) WHERE withdrawn_at IS NULL;

CREATE INDEX IF NOT EXISTS resource_in_service_idx
    ON resource (kind, name) WHERE withdrawn_at IS NULL;

-- A booking.
CREATE TABLE IF NOT EXISTS reservation (
    id             TEXT PRIMARY KEY,

    -- The reference and the frozen copy, both. The copy is what the diary
    -- prints, and it has to be here because `proj_booking` may not read
    -- `proj_crm` (L3) — a calendar that joined across two projection groups
    -- would be reading two checkpoints that can disagree.
    customer_id    TEXT,
    customer_name  TEXT NOT NULL CHECK (length(customer_name) BETWEEN 1 AND 200),
    customer_phone TEXT,

    stage          TEXT NOT NULL CHECK (stage IN (
                       'reserved', 'confirmed', 'arrived',
                       'in_service', 'completed', 'cancelled', 'no_show')),
    -- Why it was moved, when somebody said. The cancellation reason, mostly.
    stage_why      TEXT,

    -- The envelope of every line, so a calendar can find a booking by when it
    -- is without opening its lines.
    starts_at      TIMESTAMPTZ NOT NULL,
    ends_at        TIMESTAMPTZ NOT NULL,
    CONSTRAINT reservation_is_half_open CHECK (ends_at > starts_at),

    -- **What was asked for to hold the slot**, and by when. Both null when the
    -- business asks for nothing, which is the default.
    --
    -- Before tax: receiving the money is itself a tax point, and whoever raises
    -- the document for it works the tax forward from a net.
    deposit_net      BIGINT,
    deposit_currency TEXT CHECK (length(deposit_currency) = 3),
    deposit_due_by   TIMESTAMPTZ,
    CONSTRAINT reservation_deposit_is_whole
        CHECK ((deposit_net IS NULL) = (deposit_due_by IS NULL)
           AND (deposit_net IS NULL) = (deposit_currency IS NULL)),

    -- **What paid it.** Opaque here — this module does not know what a gateway
    -- is — and null until something says the money arrived.
    secured_by     TEXT,
    secured_at     TIMESTAMPTZ,

    note           TEXT,
    reserved_on    TIMESTAMPTZ NOT NULL,
    recorded_at    TIMESTAMPTZ NOT NULL,
    position       BIGINT NOT NULL
);

-- **"Which held slots has nobody paid for?"** — the list the hold-expiry job
-- works, and the only question it has to ask.
--
-- It is answerable inside this group alone, which is the point: whether the
-- money arrived is a fact `booking` was *told*, not one it reads out of
-- `proj_payments`. A diary that joined across two projection groups would be
-- reading two checkpoints that can disagree (L3).
CREATE INDEX IF NOT EXISTS reservation_unpaid_hold
    ON reservation (deposit_due_by)
    WHERE stage = 'reserved' AND deposit_due_by IS NOT NULL AND secured_by IS NULL;

-- The diary, in the order a day is read. Ascending, because a calendar opens on
-- what is coming rather than on what already happened.
CREATE INDEX IF NOT EXISTS reservation_by_time_idx ON reservation (starts_at, id);

-- The window query, `ends_at > from AND starts_at < until`. `ends_at` leads
-- because it prunes the past, which is where this table spends its life — the
-- same argument as `occupancy_claim_probe_idx` one layer down.
CREATE INDEX IF NOT EXISTS reservation_window_idx ON reservation (ends_at);

CREATE INDEX IF NOT EXISTS reservation_by_customer_idx
    ON reservation (customer_id, starts_at DESC) WHERE customer_id IS NOT NULL;

-- What each line books.
CREATE TABLE IF NOT EXISTS reservation_line (
    reservation_id TEXT NOT NULL REFERENCES reservation (id) ON DELETE CASCADE,
    line           SMALLINT NOT NULL CHECK (line >= 0),

    -- What the business calls it. The engine never sees this.
    what           TEXT NOT NULL,

    starts_at      TIMESTAMPTZ NOT NULL,
    ends_at        TIMESTAMPTZ NOT NULL,
    CONSTRAINT reservation_line_is_half_open CHECK (ends_at > starts_at),

    -- `[{"resource": "...", "quantity": 1}, ...]`. JSONB for the same reason
    -- `availability` is: read whole, never queried into. The authoritative
    -- version of who holds what is `occupancy_claim`.
    takes          JSONB NOT NULL,

    -- The unit picked out of a pool, once one has been. Null on the lines of
    -- every business that books the thing itself, which is most of them.
    unit           TEXT,

    -- What the line came to, as it was priced. Whole, for a screen that shows
    -- the rate, the band and what was taken off.
    charge         JSONB,

    -- **And the net on its own**, because "what did we take on Thursday" is a
    -- sum and not a decode. Null on a line nobody priced, which is a business
    -- that bills elsewhere rather than one that charged zero — the difference
    -- a `0` here would lose.
    net            BIGINT,
    currency       TEXT,

    PRIMARY KEY (reservation_id, line)
);

CREATE INDEX IF NOT EXISTS reservation_line_by_unit_idx
    ON reservation_line (unit) WHERE unit IS NOT NULL;

-- Which resources a customer must not be booked with, as it is now.
--
-- **A list, not the rule.** The rule is enforced against the log in the command
-- (see `src/bars.rs`); this is so a screen can show what is in force without
-- replaying every customer's stream. A lifted bar leaves this table and stays
-- in the log, which is where "was there ever one" is answered.
CREATE TABLE IF NOT EXISTS bar (
    -- The `crm` customer. Not a reservation and not a person's name: only
    -- somebody the system can recognise next time can be barred.
    customer_id TEXT NOT NULL,
    resource_id TEXT NOT NULL,

    -- For staff, and never shown to the customer the bar is against.
    why         TEXT NOT NULL,
    raised_at   TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (customer_id, resource_id)
);

CREATE INDEX IF NOT EXISTS bar_by_resource_idx ON bar (resource_id, customer_id);
