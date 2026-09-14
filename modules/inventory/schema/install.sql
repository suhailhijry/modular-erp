-- What is on the shelf, which lot it came in, and every movement that put it
-- there.
--
-- Schema-relative, like every module's install: every name here is unqualified,
-- so the same file installs into `proj_inventory` during provisioning and into
-- a staging schema during `rebuild_swap`.
--
-- **Derived entirely from the log** (L2). What is on hand is aggregate state,
-- rehydrated from the stream inside a command; these tables are what a screen
-- reads and never what a decision is taken from (L3, L7).
--
-- # Why some columns are here before anything fills them
--
-- A shape change to this file costs a `VERSION` bump and a rebuild of the group
-- on every tenant that has the module. The cost is nothing today, when nobody
-- has it, and is a fleet rebuild the day after it ships — so the columns the
-- next slices need were declared before anything filled them: the `consumed`
-- kind, the `sold` state and the nullable `lot` a shortfall leaves empty were
-- all here a slice before `sales::issue_in` first wrote one, and the slice that
-- did cost no bump at all.

-- A thing the business keeps, in the unit it keeps it in.
--
-- Declared once and never amended: the unit and the tracking mode are frozen,
-- because changing either after movements exist restates history.
CREATE TABLE IF NOT EXISTS product (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,

    -- **The unit quantities are counted in**, in the business's own words —
    -- `gram`, `piece`, `bottle`. Never matched on by any rule here; it is what
    -- a screen prints beside a number so 18 is not mistaken for 18 kilos.
    unit         TEXT NOT NULL,

    --   none    lots exist as FIFO layers nobody names
    --   lot     each delivery carries the tenant's batch code and its date
    --   serial  each unit carries a name the caller gave it
    tracking     TEXT NOT NULL CHECK (tracking IN ('none', 'lot', 'serial')),

    declared_at  TIMESTAMPTZ NOT NULL,
    -- The writer's instant is `declared_at`; this is when the projection saw
    -- it. Never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL
);

-- **One delivery.** Every receipt makes one, whatever the tracking mode,
-- because cost is carried per lot and a product with no lots would need a
-- second costing method. The one lot no receipt makes is the one a return
-- opens for units a count had cleared (`inventory::returned_lot_of`): its
-- `quantity`, `received_at` and `position` are that return's.
--
-- The aggregate keeps only the lots with something still on them; this table
-- keeps all of them, which is what "why is the shelf worth that" is answered
-- from a year later.
CREATE TABLE IF NOT EXISTS lot (
    id           TEXT PRIMARY KEY,
    stock        TEXT NOT NULL,
    product      TEXT NOT NULL,
    branch       TEXT,

    -- **The tenant's own batch code**, on a lot-tracked product. Null
    -- otherwise, and **not unique**: two deliveries may carry one code and `id`
    -- is what keeps them apart.
    code         TEXT,
    -- Null on a lot of something that does not spoil. Those go out after
    -- everything dated, oldest first.
    expires_on   DATE,

    -- What arrived, and what is still here. `remaining` is what the movements
    -- below add up to; it may reach zero and stops there.
    quantity     BIGINT NOT NULL,
    remaining    BIGINT NOT NULL,

    -- What `remaining` is carried at, in minor units. Exactly zero when the lot
    -- empties, because the last portion off a lot takes the remainder.
    value        BIGINT NOT NULL,
    currency     TEXT NOT NULL,

    received_at  TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,

    -- **The position of the receipt that made this lot**, and it is never
    -- updated when a movement draws the lot down. That makes it the received
    -- order, which is the tiebreak the listing below sorts by — and the
    -- aggregate's own order, because a lot is pushed onto `Stock::lots` as its
    -- receipt is applied. A count that bumped it would put the lots on a screen
    -- in a different order from the one stock actually leaves in.
    position     BIGINT NOT NULL
);

-- **What expires when** — and it is the order stock goes out in, which is not a
-- coincidence: the listing a manager reads and the rule `inventory::pick`
-- follows are the same sort, so the screen answers "what goes next" without
-- anybody reimplementing the rule in SQL.
--
-- The sentinel date is how "undated sorts last" is expressed to a btree, which
-- `NULLS LAST` cannot do in a keyset page; `position` is the received order and
-- breaks the tie the way the rule does. Partial on `remaining > 0` because the
-- listing shows open lots only — a closed one cannot expire into anything, and
-- its history is in `stock_movement`.
CREATE INDEX IF NOT EXISTS lot_by_expiry_idx
    ON lot (COALESCE(expires_on, DATE '9999-12-31'), position)
    WHERE remaining > 0;

-- The same listing, narrowed to one product.
CREATE INDEX IF NOT EXISTS lot_by_product_idx
    ON lot (product, COALESCE(expires_on, DATE '9999-12-31'), position)
    WHERE remaining > 0;

-- One named unit, and where it is.
--
-- Only ever written for a serial-tracked product. The state is the whole of
-- what it is for: a serial that is not `on_hand` is refused by name, which is
-- the one refusal decision 7's "never refuse for stock" does not cover, because
-- a count corrects a quantity and nothing corrects an identity.
CREATE TABLE IF NOT EXISTS serial (
    product      TEXT NOT NULL,
    serial       TEXT NOT NULL,
    lot          TEXT NOT NULL,
    stock        TEXT NOT NULL,
    branch       TEXT,

    --   on_hand      it is here — including a unit a credit note put back
    --   written_off  thrown away, with a reason on the movement
    --   sold         it went out on a document, through
    --                `inventory::consume_in`, which every sales invoice calls
    --                for a line that names a product.
    --   missing      a count of the shelf was on hand with it and did not name
    --                it. Not `written_off`: nobody threw it away or said why.
    state        TEXT NOT NULL
                 CHECK (state IN ('on_hand', 'written_off', 'sold', 'missing')),

    received_at  TIMESTAMPTZ NOT NULL,
    -- When it stopped being on hand. Null while it still is.
    left_at      TIMESTAMPTZ,
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL,

    -- **Per product and per shelf, not globally.** Two manufacturers may stamp
    -- the same number on two different things, and a business that keeps both
    -- is not wrong. The shelf is in the key because the write side cannot say
    -- otherwise: one `Stock` aggregate per branch, and neither sees the other's
    -- serials, so a tenant-wide uniqueness this table claimed would be a claim
    -- no command enforces. Keyed on the stream rather than on `(product,
    -- branch)` for the reason `stock_item` is: a branch may be null.
    PRIMARY KEY (product, stock, serial)
);

CREATE INDEX IF NOT EXISTS serial_by_lot_idx ON serial (lot, serial);

-- What is on hand, per product **per branch**.
--
-- Keyed on the stock stream rather than on `(product, branch)`, because a
-- branch is absent on a single-branch business and a null cannot be part of a
-- primary key. The stream id is `{product}` or `{product}.{branch}` and the two
-- columns beside it are split back out of it — one answer, written down twice
-- only because SQL cannot index into a string cheaply.
CREATE TABLE IF NOT EXISTS stock_item (
    stock        TEXT PRIMARY KEY,
    product      TEXT NOT NULL,
    -- Null on a business that sends no `X-Branch`, which is most of them.
    branch       TEXT,

    -- The open lots added up, **less what the shelf owes**. **Signed, and it
    -- goes negative**: a plain product's sale takes what the lots cannot cover
    -- as a shortfall (R1), because a till does not stop for a bad count, and
    -- the negative number is the report. A lot- or serial-tracked product
    -- cannot get here — such a sale is refused — and neither can a write-off.
    on_hand      BIGINT NOT NULL,

    -- What that quantity is carried at, in minor units: the sum of the open
    -- lots' own values, never an average across them.
    value        BIGINT NOT NULL,
    -- Null while nothing has ever been received, because there is no currency
    -- to state a zero in.
    currency     TEXT,

    last_at      TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL
);

-- What the business holds, across branches — the list a manager opens.
CREATE INDEX IF NOT EXISTS stock_item_by_product_idx
    ON stock_item (product, branch);

-- One movement, **against one lot**: why the quantity above is what it is.
--
-- **Signed deltas, not running totals.** `quantity` is what this movement did
-- to what is on hand — plus on a receipt, minus on a write-off, and the
-- variance itself on a count — so the sum of the movements *is* the quantity on
-- hand, lot by lot, and nothing has to be read to write a row (L2).
--
-- One event can produce several rows, because one movement can come off several
-- lots: `seq` orders them within the event and completes the key.
CREATE TABLE IF NOT EXISTS stock_movement (
    stock        TEXT NOT NULL,

    -- **The log position**, which is also the order. A per-stock counter would
    -- have to be derived and could disagree with the log a rebuild follows.
    position     BIGINT NOT NULL,
    -- Which portion of that event this row is.
    seq          INTEGER NOT NULL,

    product      TEXT NOT NULL,
    branch       TEXT,
    -- **Null only where no lot was involved** — units that crossed the shelf's
    -- edge with no lot behind them: a plain product sold below zero (R1), or the
    -- part of a return or a count of the shelf that settles that debt — and the
    -- one row a count of the shelf leaves when it moved nothing. A write-off and
    -- a tracked product's sale are refused instead, so they always name a lot.
    lot          TEXT,

    --   received     bought in, or put back by a credit note
    --   written_off  expired or damaged, and thrown away
    --   counted      somebody counted the shelf, or one lot on it
    --   consumed     sold or used against a document, by
    --                `inventory::consume_in`
    kind         TEXT NOT NULL
                 CHECK (kind IN ('received', 'written_off', 'counted', 'consumed')),

    -- Why it was thrown away. Null on everything but a write-off.
    reason       TEXT CHECK (reason IS NULL OR reason IN ('expired', 'damaged')),

    quantity     BIGINT NOT NULL,
    value        BIGINT NOT NULL,
    currency     TEXT,

    -- What a count found, and what the books said it would. Null on everything
    -- else. Frozen as the event recorded them (L5) — never recomputed from a
    -- valuation that has moved since.
    expected     BIGINT,
    declared     BIGINT,

    -- **What this movement was for**: the request that made it, or the document
    -- whose line consumed it. What makes hearing the same movement twice
    -- nothing.
    reference    TEXT NOT NULL,

    moved_at     TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (stock, position, seq)
);

-- The movements behind one number, newest first.
--
-- **On `position`, which is what the listing orders by**, and not on `moved_at`
-- — nothing here orders or filters by when a movement happened, and an index
-- leading on it cannot serve `ORDER BY position DESC` at all. The listings are
-- keyset-paged on `position`, so the unfiltered one takes the first index, one
-- product's history takes the second and one lot's takes the third; without
-- them every page of every request is a sequential scan of every movement the
-- tenant has ever recorded.
CREATE INDEX IF NOT EXISTS stock_movement_by_position_idx
    ON stock_movement (position DESC, seq DESC);

CREATE INDEX IF NOT EXISTS stock_movement_by_product_idx
    ON stock_movement (product, position DESC, seq DESC);

-- **The canary's own index**: what one lot holds is the sum of what moved on
-- it.
CREATE INDEX IF NOT EXISTS stock_movement_by_lot_idx
    ON stock_movement (lot, position DESC);

-- Everything one document took, which is what a return has to give back.
CREATE INDEX IF NOT EXISTS stock_movement_by_reference_idx
    ON stock_movement (reference);
