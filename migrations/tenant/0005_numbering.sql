-- Gapless document numbers.
--
-- Saudi law requires a tax invoice to carry "a sequential number which uniquely
-- identifies the invoice" (VAT Implementing Regulations, Article 53), and
-- ZATCA's e-invoicing rules require the counter to advance by exactly one per
-- document so the cryptographic chain has no holes. Not "unique". Not "mostly
-- ordered". **Gapless.**
--
-- # Why this is not a SEQUENCE
--
-- Postgres sequences are deliberately transaction-independent: `nextval` is not
-- rolled back, because that is what lets concurrent writers take numbers without
-- blocking each other. It is exactly the wrong property here — every failed
-- issue would burn a number, and a business would have to explain to an auditor
-- why invoice 4108 does not exist.
--
-- So this is an ordinary row, read `FOR UPDATE` and incremented in the same
-- transaction as the document that uses it. A rollback releases the number
-- because the number was never really taken.
--
-- # What that costs
--
-- Issuing serializes per (tenant, series). Everything else in the system is
-- concurrent; this one thing cannot be, and no implementation can make it so —
-- "gapless" and "concurrent" are the same contradiction whatever holds the
-- counter. The lock is held from the reservation to the end of the transaction,
-- which is one aggregate load and two inserts.
--
-- ponytail: one row per series and a plain `FOR UPDATE`. If a tenant ever issues
-- fast enough for that to queue, the answer is not a cleverer lock — it is more
-- series (per branch, per point of sale), which is also how the paper world
-- solved it.
--
-- # Why this is not in a projection schema
--
-- It is not derived from the log; the log depends on *it*. The number is written
-- into the event, so a replay reproduces the same numbers without ever consulting
-- this table (architecture L5), and `rebuild_schema` — which drops and rebuilds
-- `proj_*` — must never touch it.

CREATE TABLE document_number (
    -- Namespaced by module, like `configuration`: `sales.invoice`. The module
    -- that owns the document owns the series.
    series     TEXT PRIMARY KEY CHECK (length(series) BETWEEN 1 AND 64),

    -- The number this series will hand out next. Starts at 1; a business that
    -- migrated from another system sets it to where they left off.
    next       BIGINT NOT NULL CHECK (next >= 1),

    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
