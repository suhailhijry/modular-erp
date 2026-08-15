-- The sales module's read models.
--
-- Derived from the event log and dropped-and-rebuilt rather than migrated, for
-- the reasons in `modules/ledger/schema/install.sql`.

CREATE SCHEMA IF NOT EXISTS proj_sales;

CREATE TABLE IF NOT EXISTS proj_sales.invoice (
    -- The invoice number, as the tenant chose it. Also the aggregate id.
    id           TEXT PRIMARY KEY,

    -- The buyer as they were when it was issued, never a foreign key. A tax
    -- invoice is a legal document; last year's copy must not change when a
    -- customer record does.
    customer     TEXT NOT NULL,
    customer_vat TEXT,

    -- The tax point, and when payment is due. Both dates the business chose,
    -- not clock readings.
    issued_on    TIMESTAMPTZ NOT NULL,
    due_on       TIMESTAMPTZ,

    currency     CHAR(3) NOT NULL,
    -- Minor units, matching `ledger`. Excluding tax, the tax, and the total.
    net          BIGINT NOT NULL,
    tax          BIGINT NOT NULL,
    gross        BIGINT NOT NULL CHECK (gross = net + tax),

    note         TEXT NOT NULL DEFAULT '',

    -- Cancelled by a credit note. The invoice stays: accounting does not
    -- delete, and a document that was issued was issued. What changes is that
    -- nobody owes anything on it.
    cancelled_on TIMESTAMPTZ,
    credit_note  TEXT,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS invoice_by_date_idx ON proj_sales.invoice (issued_on DESC);
CREATE INDEX IF NOT EXISTS invoice_by_customer_idx ON proj_sales.invoice (customer);

CREATE TABLE IF NOT EXISTS proj_sales.invoice_line (
    -- Derived from the event's log position, so a rebuild reproduces it.
    id            UUID PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES proj_sales.invoice (id) ON DELETE CASCADE,
    line_index    INT  NOT NULL CHECK (line_index >= 0),

    description   TEXT NOT NULL,
    net           BIGINT NOT NULL,
    -- The category and the rate that applied when the invoice was issued. Both,
    -- because zero-rated and exempt are both 0% and mean different things on a
    -- VAT return.
    vat_category  TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp   INT  NOT NULL CHECK (vat_rate_bp >= 0),

    CONSTRAINT invoice_line_is_unique UNIQUE (invoice_id, line_index)
);

CREATE INDEX IF NOT EXISTS invoice_line_by_invoice_idx
    ON proj_sales.invoice_line (invoice_id, line_index);

-- The tax breakdown a Saudi invoice has to print: one row per rate, taxed once
-- on the subtotal rather than line by line.
CREATE TABLE IF NOT EXISTS proj_sales.invoice_tax (
    id            UUID PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES proj_sales.invoice (id) ON DELETE CASCADE,
    vat_category  TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp   INT  NOT NULL CHECK (vat_rate_bp >= 0),
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,

    CONSTRAINT invoice_tax_is_unique UNIQUE (invoice_id, vat_category, vat_rate_bp)
);

CREATE TABLE IF NOT EXISTS proj_sales.invoice_payment (
    id           UUID PRIMARY KEY,
    invoice_id   TEXT NOT NULL REFERENCES proj_sales.invoice (id) ON DELETE CASCADE,
    -- The payer's own reference. Unique per invoice, which is what makes
    -- recording one twice a no-op.
    reference    TEXT NOT NULL,
    amount       BIGINT NOT NULL CHECK (amount > 0),
    received_on  TIMESTAMPTZ NOT NULL,
    -- The ledger account it landed in.
    account      TEXT NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,

    CONSTRAINT invoice_payment_is_unique UNIQUE (invoice_id, reference)
);

CREATE INDEX IF NOT EXISTS invoice_payment_by_invoice_idx
    ON proj_sales.invoice_payment (invoice_id);

-- The output-tax side of a VAT return.
--
-- One row per invoice per rate, carrying the tax point so a return can be run
-- for a period. A view rather than a table for the same reason balances are:
-- summing is exact and needs no code, and a maintained total is a second thing
-- that can be wrong.
--
-- **Cancelled invoices are excluded**, not netted to zero. A credit note in the
-- same period removes the supply; one in a *later* period is a supply and then
-- an adjustment, and ZATCA wants those reported in the periods they happened.
-- ponytail: that distinction needs the credit note to be a document with its own
-- tax point, which is the partial-credit-note work. Until then a credited
-- invoice leaves the return entirely, which is right when the credit lands in
-- the same period and wrong across a boundary — so this view is honest about
-- being the simple case and `vat_return` refuses to span one silently.
CREATE OR REPLACE VIEW proj_sales.taxable_supply AS
SELECT i.id           AS invoice_id,
       i.issued_on,
       i.currency,
       t.vat_category,
       t.vat_rate_bp,
       t.net,
       t.tax
  FROM proj_sales.invoice i
  JOIN proj_sales.invoice_tax t ON t.invoice_id = i.id
 WHERE i.cancelled_on IS NULL;

-- What is still owed, summed rather than maintained.
--
-- A `paid` column on `invoice` would be a second thing that can be wrong, and
-- keeping it in step is the projection code most likely to double-count. Same
-- reasoning as `proj_ledger.account_balance`, and the same upgrade path if a
-- tenant ever has enough invoices for the scan to matter.
CREATE OR REPLACE VIEW proj_sales.invoice_status AS
SELECT i.id,
       i.customer,
       i.customer_vat,
       i.issued_on,
       i.due_on,
       i.currency,
       i.net,
       i.tax,
       i.gross,
       i.note,
       i.cancelled_on,
       i.credit_note,
       i.recorded_at,
       COALESCE(sum(p.amount), 0)::BIGINT            AS paid,
       -- A cancelled invoice owes nothing. Without this it keeps appearing in
       -- a receivables list, and somebody chases a customer for money that was
       -- credited back to them.
       CASE WHEN i.cancelled_on IS NOT NULL THEN 0
            ELSE (i.gross - COALESCE(sum(p.amount), 0))
       END::BIGINT                                   AS outstanding,
       count(p.id)                                   AS payments
  FROM proj_sales.invoice i
  LEFT JOIN proj_sales.invoice_payment p ON p.invoice_id = i.id
 GROUP BY i.id;
