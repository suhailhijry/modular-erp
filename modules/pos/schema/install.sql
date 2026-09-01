-- The counter: what each till took, and what its drawer disagreed by.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_pos` during provisioning and into a staging schema during
-- `rebuild_swap`.

-- A till, from open to count.
--
-- **There is no `sale` table here**, and its absence is the module's whole
-- design: a till sale is a `sales` invoice, and it is in `proj_sales.invoice`
-- with every other one. A second copy would be a second answer to "what did we
-- sell", and the VAT return would have to pick one.
CREATE TABLE IF NOT EXISTS shift (
    id            TEXT PRIMARY KEY,

    -- The business's own name for the counter, and whoever was on it.
    till          TEXT NOT NULL,
    operator      TEXT NOT NULL,

    -- All in minor units. `float` is what the drawer held before anything sold.
    float         BIGINT NOT NULL,
    -- What the drawer *should* hold: float + cash taken - cash refunded - cash
    -- paid out. Maintained as sales land so a manager can read it mid-shift.
    expected      BIGINT NOT NULL,
    -- Counted at the close, and the difference. Null until then; `variance` is
    -- negative when the drawer is short.
    declared      BIGINT,
    variance      BIGINT,
    currency      TEXT NOT NULL,

    sales_count   INTEGER NOT NULL DEFAULT 0,

    opened_at     TIMESTAMPTZ NOT NULL,
    closed_at     TIMESTAMPTZ,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- The till report: today's shifts, newest first.
CREATE INDEX IF NOT EXISTS shift_by_opening_idx ON shift (opened_at DESC, id DESC);

-- Which tills are still taking money, which is what a manager checks at
-- closing time. Partial, because most shifts are shut.
CREATE INDEX IF NOT EXISTS shift_open_idx ON shift (till, opened_at DESC)
    WHERE closed_at IS NULL;

-- What each shift took, split by how it arrived.
--
-- A row per (shift, method) rather than per tender: takings are read as "how
-- much cash, how much card" and never as a sequence. `refunded` is money handed
-- back the same way it came in.
CREATE TABLE IF NOT EXISTS taking (
    shift         TEXT NOT NULL REFERENCES shift (id) ON DELETE CASCADE,
    method        TEXT NOT NULL CHECK (method IN ('cash', 'card', 'transfer')),

    taken         BIGINT NOT NULL DEFAULT 0,
    refunded      BIGINT NOT NULL DEFAULT 0,
    currency      TEXT NOT NULL,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    PRIMARY KEY (shift, method)
);
