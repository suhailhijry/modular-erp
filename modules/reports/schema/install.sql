-- Figures a dashboard reads, built from the log.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_reports` during provisioning and into a staging schema
-- during `rebuild_swap`.
--
-- ===========================================================================
-- Why these tables exist at all, when the numbers are already elsewhere
-- ===========================================================================
--
-- Because a dashboard mixing sales, bookings and payroll looks like it must
-- read three projection groups, and **L3 forbids that**: a group is the unit of
-- consistency, and three groups on three checkpoints can disagree while
-- somebody is reading a total across them.
--
-- So this module subscribes to the *log* — it decodes `sales::InvoiceEvent`,
-- `booking::ReservationEvent`, `pos::ShiftEvent` and `payroll::RunEvent` — and
-- maintains its own group with one checkpoint. Every figure on a screen built
-- from these tables was true at one position in the log, together.

-- What was sold, by the period and place it was sold in.
--
-- One row per (period, branch, currency). A month is `YYYY-MM`, which is what a
-- report is read by and what sorts correctly as text.
CREATE TABLE IF NOT EXISTS revenue (
    period        TEXT NOT NULL CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    -- Where it was sold. Empty string rather than null so it can be in the
    -- primary key — a single-branch business has one row per period and it is
    -- the empty one, which is honest about there being no branch rather than
    -- inventing a name for it.
    branch        TEXT NOT NULL DEFAULT '',
    currency      TEXT NOT NULL,

    -- All in minor units. **Net of credit notes**: a cancelled invoice takes
    -- its own numbers back out, so what this says is what the business kept.
    net           BIGINT NOT NULL DEFAULT 0,
    tax           BIGINT NOT NULL DEFAULT 0,
    -- Documents issued less those credited. Can reach zero on a period where
    -- everything was cancelled, and the row stays — "we issued four and
    -- credited four" is a different fact from "we issued nothing".
    documents     INTEGER NOT NULL DEFAULT 0,
    credited      INTEGER NOT NULL DEFAULT 0,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (period, branch, currency)
);

-- "This year by month", which is the shape of every revenue chart.
CREATE INDEX IF NOT EXISTS revenue_by_period_idx ON revenue (period DESC);

-- What each invoice came to, so a credit note can take out exactly what the
-- issue put in.
--
-- A working table, like `held` below. `sales.invoice.cancelled` carries the
-- credit note and the reason, not the amounts — rightly, because they have not
-- changed — so a report that nets credits off has to remember. Asking
-- `proj_sales` would be the cross-group read L3 forbids.
CREATE TABLE IF NOT EXISTS invoiced (
    id            TEXT PRIMARY KEY,
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,
    currency      TEXT NOT NULL,
    -- Where and when it was issued, so a credit lands where it should even if
    -- the crediting request carried a different branch header.
    period        TEXT NOT NULL,
    branch        TEXT NOT NULL DEFAULT '',

    -- **The journal entries this document made**, named by `sales`' own scheme
    -- (`sales::issue_entry_of`, `sales::credit_entry_of`). Stored rather than
    -- recomputed at read time so the reconciliation in §10b is one join inside
    -- one schema instead of an `IN` list with a row per invoice ever issued.
    --
    -- `credit_entry` is null until the document is credited, which is what
    -- almost every invoice stays.
    entry         TEXT NOT NULL,
    credit_entry  TEXT,

    -- **Where in the log this invoice was seen.** The reconciliation needs it
    -- for one reason: an invoice and its journal entry commit together and so
    -- take consecutive positions, but a projection batch may end between them.
    -- Reporting the invoice at the very tail as "made no entry" would be
    -- reporting a batch boundary as a broken ledger — which is precisely the
    -- crying wolf that gets an invariant switched off. See `reconcile.rs`.
    position      BIGINT NOT NULL
);

-- How well the diary was used.
--
-- One row per (period, resource). Utilisation needs both halves — what was
-- booked and what came of it — and a rate computed from one of them is the
-- number that makes a business think it is busier than it is.
CREATE TABLE IF NOT EXISTS utilisation (
    period        TEXT NOT NULL CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    resource      TEXT NOT NULL,

    booked        INTEGER NOT NULL DEFAULT 0,
    completed     INTEGER NOT NULL DEFAULT 0,
    -- **Counted, not inferred.** A no-show is a stage somebody moved a booking
    -- to; deriving it from "booked and not completed" would count everything
    -- still in the diary as a no-show the moment the month ended.
    no_shows      INTEGER NOT NULL DEFAULT 0,
    cancelled     INTEGER NOT NULL DEFAULT 0,

    -- Minutes of diary time the completed bookings took, which is what
    -- "revenue per resource-hour" needs as its denominator.
    minutes       BIGINT NOT NULL DEFAULT 0,

    -- **Lead time**: minutes between the booking being taken and the work
    -- starting, summed over what was booked. Divided by `booked` it is the
    -- average notice this resource gets, which is the number that says whether
    -- a diary is planned or walked into.
    lead_minutes  BIGINT NOT NULL DEFAULT 0,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (period, resource)
);

CREATE INDEX IF NOT EXISTS utilisation_by_period_idx ON utilisation (period DESC, resource);

-- What each booking holds, so a completion can be attributed to a resource.
--
-- # Why this module keeps its own copy
--
-- `booking.reservation.moved` carries a stage and nothing else — deliberately,
-- because the stage is a field rather than seven event names. So a report that
-- wants to say "the Olaya chair completed forty jobs" has to know what the
-- booking held, and it cannot ask `proj_booking`: **L3 forbids reading another
-- group**, and two checkpoints could disagree in the middle of a total.
--
-- So the report keeps what it needs. It is a working table and not a figure
-- anybody reads, which is why it is not in the module's public reads.
CREATE TABLE IF NOT EXISTS held (
    reservation   TEXT NOT NULL,
    resource      TEXT NOT NULL,

    -- The period and length are frozen when the booking is made or moved, so a
    -- completion months later still lands in the month it was for.
    period        TEXT NOT NULL,
    minutes       BIGINT NOT NULL,
    -- The last stage this module saw, so a booking moved twice is not counted
    -- twice — `reserved → confirmed → completed` is one completion.
    stage         TEXT NOT NULL DEFAULT 'reserved',

    PRIMARY KEY (reservation, resource)
);

-- What the tills took, by how the money arrived and who was on the counter.
CREATE TABLE IF NOT EXISTS takings (
    period        TEXT NOT NULL CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    operator      TEXT NOT NULL,
    method        TEXT NOT NULL,
    currency      TEXT NOT NULL,

    taken         BIGINT NOT NULL DEFAULT 0,
    refunded      BIGINT NOT NULL DEFAULT 0,
    -- **What the drawer disagreed by**, summed over the shifts this operator
    -- closed. Negative is short. The number a manager actually reads, and the
    -- reason `pos` posts it rather than only recording it.
    variance      BIGINT NOT NULL DEFAULT 0,
    -- Cash that left the drawer and was not a refund — a banking run, a float
    -- moved to another till, a supplier paid in notes. **The closest the log
    -- comes to "what was banked"**, and it is called what it is rather than
    -- claiming a bank confirmed anything: nothing in this system has ever seen
    -- a bank statement. Only ever on the cash row, because only cash is in the
    -- box.
    paid_out      BIGINT NOT NULL DEFAULT 0,
    shifts        INTEGER NOT NULL DEFAULT 0,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (period, operator, method, currency)
);

CREATE INDEX IF NOT EXISTS takings_by_period_idx ON takings (period DESC, operator);

-- What people cost.
--
-- From approved payroll runs only: a draft is not a cost, and counting one
-- would make a report move when somebody opened a screen.
CREATE TABLE IF NOT EXISTS people_cost (
    period        TEXT NOT NULL CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    currency      TEXT NOT NULL,

    gross         BIGINT NOT NULL DEFAULT 0,
    commission    BIGINT NOT NULL DEFAULT 0,
    deductions    BIGINT NOT NULL DEFAULT 0,
    net           BIGINT NOT NULL DEFAULT 0,
    people        INTEGER NOT NULL DEFAULT 0,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (period, currency)
);

-- Which operator has which till open, so a sale can be attributed to a person.
--
-- A working table. `pos.shift.sold` carries the sale and the tenders and not
-- the operator — rightly, because the operator has not changed since the shift
-- opened — so a report that groups takings by person has to remember. Asking
-- `proj_pos` would be the cross-group read L3 forbids.
CREATE TABLE IF NOT EXISTS till (
    shift         TEXT PRIMARY KEY,
    operator      TEXT NOT NULL
);

-- What a payroll run came to while it was still a draft.
--
-- A working table, for the same reason as the two above:
-- `payroll.run.approved` carries the journal entry and the time, not the
-- amounts, because approving does not change them. A cost report counts
-- **approved** runs only — a draft is not a cost, and counting one would make a
-- report move when somebody opened a screen — so it has to hold the figures
-- from drafting until approval arrives.
--
-- Drafting again replaces, which is what the aggregate does too.
CREATE TABLE IF NOT EXISTS drafted (
    run           TEXT PRIMARY KEY,
    period        TEXT NOT NULL,
    currency      TEXT NOT NULL,
    gross         BIGINT NOT NULL,
    commission    BIGINT NOT NULL,
    deductions    BIGINT NOT NULL,
    net           BIGINT NOT NULL,
    people        INTEGER NOT NULL
);

-- ===========================================================================
-- What the books say, kept here so the reconciliation is not a cross-group read
-- ===========================================================================
--
-- The invariant this module owes (§10b) is that its figures agree with the
-- ledger. Reading `proj_ledger` to check would be exactly the thing L3 forbids,
-- and worse than forbidden: the two groups sit on two checkpoints, so a
-- disagreement would as often mean "one is behind" as "one is wrong" — and an
-- invariant that cries wolf gets switched off.
--
-- So this group subscribes to `ledger.entry.posted` like it subscribes to
-- everything else, and reconciles **against its own copy, at its own
-- checkpoint**. Every row below was written by the same projection run, at one
-- position, so a difference is a difference and never a race.
CREATE TABLE IF NOT EXISTS entry (
    -- The journal entry's id, which is the ledger aggregate's id.
    id            TEXT PRIMARY KEY,
    currency      TEXT NOT NULL,
    -- Debits are positive, credits negative, and a posted entry balances — so
    -- `debits` is the entry's size and `debits + credits` must be zero.
    debits        BIGINT NOT NULL,
    credits       BIGINT NOT NULL,
    occurred_on   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS entry_by_currency_idx ON entry (currency);
