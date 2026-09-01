-- Places a business trades from.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_branches` during provisioning and into a staging schema
-- during `rebuild_swap`.

CREATE TABLE IF NOT EXISTS branch (
    id           TEXT PRIMARY KEY,

    name         TEXT NOT NULL,
    name_latin   TEXT,

    street       TEXT NOT NULL,
    building     TEXT,
    district     TEXT,
    city         TEXT NOT NULL,
    postal_code  TEXT,
    country      TEXT NOT NULL,

    -- **Closed and not deleted.** A branch has a year of documents behind it,
    -- and a dimension that vanishes takes the meaning of its own history with
    -- it. Null while it is trading.
    closed_at    TIMESTAMPTZ,
    closed_why   TEXT,

    opened_on    TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL
);

-- The settings screen: every branch, the trading ones first.
CREATE INDEX IF NOT EXISTS branch_by_name_idx ON branch (name, id);

-- Where a document may be dated to. Partial, because most branches trade.
CREATE INDEX IF NOT EXISTS branch_open_idx ON branch (name, id) WHERE closed_at IS NULL;
