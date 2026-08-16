-- The purchases module's read models.
--
-- Derived from the event log and dropped-and-rebuilt rather than migrated, for
-- the reasons in `modules/ledger/schema/install.sql`.

CREATE SCHEMA IF NOT EXISTS proj_purchases;

CREATE TABLE IF NOT EXISTS proj_purchases.bill (
    -- Our own key for the bill, and the aggregate id. Not the supplier's number:
    -- two suppliers can both call something `INV-001`, and there is no gapless
    -- series here because **we did not issue this document**.
    id            TEXT PRIMARY KEY,

    -- The supplier as they were on the bill, never a foreign key. Same reason a
    -- customer is a snapshot on an invoice.
    supplier      TEXT NOT NULL,
    -- Their VAT registration number. Input tax is not reclaimable without one,
    -- which is why a bill carrying tax cannot be recorded without it.
    supplier_vat  TEXT,
    -- Their invoice number. What a reclaim is evidenced by.
    reference     TEXT NOT NULL,

    -- The tax point from their document, and when payment is due.
    billed_on     TIMESTAMPTZ NOT NULL,
    due_on        TIMESTAMPTZ,

    currency      CHAR(3) NOT NULL,
    -- Minor units, matching `ledger`. As the supplier stated them.
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,
    gross         BIGINT NOT NULL CHECK (gross = net + tax),

    note          TEXT NOT NULL DEFAULT '',

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at   TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS bill_by_date_idx ON proj_purchases.bill (billed_on DESC);
CREATE INDEX IF NOT EXISTS bill_by_supplier_idx ON proj_purchases.bill (supplier);

-- The same supplier invoice recorded twice is a duplicate reclaim, which is the
-- kind of mistake an inspection finds. A constraint rather than a check in code:
-- it holds against a rebuild and against anything that writes here later.
--
-- Scoped to the supplier, because two of them numbering from 1 is normal.
CREATE UNIQUE INDEX IF NOT EXISTS bill_reference_is_unique_per_supplier
    ON proj_purchases.bill (supplier, reference);

CREATE TABLE IF NOT EXISTS proj_purchases.bill_line (
    -- Derived from the event's log position, so a rebuild reproduces it.
    id            UUID PRIMARY KEY,
    bill_id       TEXT NOT NULL REFERENCES proj_purchases.bill (id) ON DELETE CASCADE,
    line_index    INT  NOT NULL CHECK (line_index >= 0),

    description   TEXT NOT NULL,
    -- The expense or asset account it landed in. One bill routinely covers
    -- several.
    account       TEXT NOT NULL,
    net           BIGINT NOT NULL,
    vat_category  TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    -- The rate the *supplier* charged, not today's statutory one. Stored so a
    -- disagreement with it is something a person can see.
    vat_rate_bp   INT  NOT NULL CHECK (vat_rate_bp >= 0),
    -- What they charged. Recorded, not computed — see `modules/purchases/src/bill.rs`.
    tax           BIGINT NOT NULL CHECK (tax >= 0),

    CONSTRAINT bill_line_is_unique UNIQUE (bill_id, line_index)
);

CREATE INDEX IF NOT EXISTS bill_line_by_bill_idx
    ON proj_purchases.bill_line (bill_id, line_index);

CREATE TABLE IF NOT EXISTS proj_purchases.bill_payment (
    id            UUID PRIMARY KEY,
    bill_id       TEXT NOT NULL REFERENCES proj_purchases.bill (id) ON DELETE CASCADE,
    -- Our own reference. Unique per bill, which is what makes recording one
    -- twice a no-op.
    reference     TEXT NOT NULL,
    amount        BIGINT NOT NULL CHECK (amount > 0),
    paid_on       TIMESTAMPTZ NOT NULL,
    -- The ledger account it left.
    account       TEXT NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,

    CONSTRAINT bill_payment_is_unique UNIQUE (bill_id, reference)
);

CREATE INDEX IF NOT EXISTS bill_payment_by_bill_idx
    ON proj_purchases.bill_payment (bill_id);

-- The input-tax side of a VAT return, as entries on a tax point.
--
-- The mirror of `proj_sales.vat_entry`, and deliberately the same shape: the API
-- nets the two into one return, and two shapes that have to be reconciled at the
-- point of composition is how the reconciliation goes wrong.
--
-- One row per line rather than per band, because a bill's tax comes per line
-- from the supplier and is never re-banded — there is nothing to group before
-- the return groups it.
--
-- **Only reclaimable tax appears here.** Input tax on an exempt supply is a cost
-- of the purchase, not a debt ZATCA owes back; claiming it is a reclaim that
-- gets disallowed. The `net` still appears, because exempt purchases are
-- reported even though their tax is not recovered.
CREATE OR REPLACE VIEW proj_purchases.vat_entry AS
SELECT b.id                                                 AS document_id,
       b.reference                                          AS document_number,
       'bill'                                               AS kind,
       b.billed_on                                          AS tax_point,
       b.currency,
       l.vat_category,
       l.vat_rate_bp,
       l.net,
       CASE WHEN l.vat_category = 'exempt' THEN 0 ELSE l.tax END AS tax
  FROM proj_purchases.bill b
  JOIN proj_purchases.bill_line l ON l.bill_id = b.id;

-- What is still owed to suppliers, summed rather than maintained.
--
-- Same reasoning as `proj_sales.invoice_status`: a `paid` column is a second
-- number that can be wrong, and keeping it in step is the projection code most
-- likely to double-count.
CREATE OR REPLACE VIEW proj_purchases.bill_status AS
SELECT b.id,
       b.supplier,
       b.supplier_vat,
       b.reference,
       b.billed_on,
       b.due_on,
       b.currency,
       b.net,
       b.tax,
       b.gross,
       b.note,
       b.recorded_at,
       COALESCE(sum(p.amount), 0)::BIGINT      AS paid,
       (b.gross - COALESCE(sum(p.amount), 0))::BIGINT AS outstanding,
       count(p.id)                             AS payments
  FROM proj_purchases.bill b
  LEFT JOIN proj_purchases.bill_payment p ON p.bill_id = b.id
 GROUP BY b.id;
