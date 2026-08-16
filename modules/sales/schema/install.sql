-- The sales module's read models.
--
-- Derived from the event log and dropped-and-rebuilt rather than migrated, for
-- the reasons in `modules/ledger/schema/install.sql`.

CREATE TABLE IF NOT EXISTS invoice (
    -- The client's own key for this invoice, and the aggregate id. Sending the
    -- same one twice is a no-op, which is what makes a retry safe.
    id           TEXT PRIMARY KEY,

    -- **The statutory number.** Allocated from a gapless per-tenant series at
    -- issue and carried in the event, so a rebuild reproduces it rather than
    -- re-allocating (architecture L5). See `migrations/tenant/0005_numbering.sql`.
    --
    -- Unique, and not the primary key: `id` is what a client addresses and this
    -- is what the document prints. On invoices issued before this system
    -- numbered them the two are the same string, which is exactly what they
    -- were.
    number       TEXT NOT NULL,

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
    -- Taken off the whole document, positive, and **already subtracted from
    -- `net`** — so `net + discount` is what the lines came to. A tax invoice
    -- has to print both, which is why the smaller number alone will not do.
    discount     BIGINT NOT NULL DEFAULT 0 CHECK (discount >= 0),

    note         TEXT NOT NULL DEFAULT '',

    -- Cancelled by a credit note. The invoice stays: accounting does not
    -- delete, and a document that was issued was issued. What changes is that
    -- nobody owes anything on it.
    cancelled_on TIMESTAMPTZ,
    credit_note  TEXT,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL
);

-- A repeated number would mean the series went backwards, which is the one
-- failure mode gaplessness exists to prevent. A constraint rather than a test,
-- because a projection that could write it twice must fail loudly (L6).
CREATE UNIQUE INDEX IF NOT EXISTS invoice_number_is_unique ON invoice (number);

CREATE INDEX IF NOT EXISTS invoice_by_date_idx ON invoice (issued_on DESC);
CREATE INDEX IF NOT EXISTS invoice_by_customer_idx ON invoice (customer);

-- What was taken off the whole invoice, and why.
--
-- One row per `cac:AllowanceCharge`: ZATCA prints each as its own figure with
-- its own reason and tax treatment, so a customer sees the discount rather than
-- a smaller total with no explanation.
CREATE TABLE IF NOT EXISTS invoice_discount (
    -- Derived from the event's log position, so a rebuild reproduces it.
    id             UUID PRIMARY KEY,
    invoice_id     TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
    discount_index INT  NOT NULL CHECK (discount_index >= 0),

    reason         TEXT NOT NULL,
    -- Positive: the amount taken off. A negative one is a charge, which is a
    -- different element.
    amount         BIGINT NOT NULL CHECK (amount > 0),
    -- Which band it comes off. Discounting a standard-rated invoice reduces the
    -- tax; discounting an exempt one does not, because there was none.
    vat_category   TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp    INT  NOT NULL CHECK (vat_rate_bp >= 0),

    CONSTRAINT invoice_discount_is_unique UNIQUE (invoice_id, discount_index)
);

CREATE INDEX IF NOT EXISTS invoice_discount_by_invoice_idx
    ON invoice_discount (invoice_id, discount_index);

CREATE TABLE IF NOT EXISTS invoice_line (
    -- Derived from the event's log position, so a rebuild reproduces it.
    id            UUID PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
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
    ON invoice_line (invoice_id, line_index);

-- The tax breakdown a Saudi invoice has to print: one row per rate, taxed once
-- on the subtotal rather than line by line.
CREATE TABLE IF NOT EXISTS invoice_tax (
    id            UUID PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
    vat_category  TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp   INT  NOT NULL CHECK (vat_rate_bp >= 0),
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,

    CONSTRAINT invoice_tax_is_unique UNIQUE (invoice_id, vat_category, vat_rate_bp)
);

CREATE TABLE IF NOT EXISTS invoice_payment (
    id           UUID PRIMARY KEY,
    invoice_id   TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
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
    ON invoice_payment (invoice_id);

-- The output-tax side of a VAT return, as entries on a tax point.
--
-- One row per document per rate band: an invoice on the day it was issued, and
-- a credit note **on its own tax point**, negating what the invoice declared.
--
-- # Why a credit note is an entry and not a deletion
--
-- The first version of this view simply excluded cancelled invoices, and that is
-- right in exactly one case: a credit note raised in the same period as the
-- invoice, where the supply and its reversal cancel out before anything is
-- filed.
--
-- Across a period boundary it is wrong, and wrong in the direction that matters.
-- An invoice issued in Q1 and credited in Q2 was a supply in Q1 — the return was
-- filed, the tax was paid — and the credit is an *adjustment in Q2*. Dropping the
-- invoice retrospectively means re-running the Q1 return produces a different
-- number from the one filed, and nothing anywhere says why. ZATCA wants each
-- reported in the period it happened, and so does anybody reconciling the books
-- to a filed return.
--
-- So both are entries, each on its own tax point, and the period does the rest.
-- Same-period credits still net to zero; cross-period ones no longer reach back.
CREATE OR REPLACE VIEW vat_entry AS
-- The supply.
SELECT i.id            AS document_id,
       i.number        AS document_number,
       'invoice'       AS kind,
       i.issued_on     AS tax_point,
       i.currency,
       t.vat_category,
       t.vat_rate_bp,
       t.net,
       t.tax
  FROM invoice i
  JOIN invoice_tax t ON t.invoice_id = i.id

UNION ALL

-- The adjustment, negating the same bands the invoice declared. A credit note
-- cancels the whole invoice, so it reverses every band of it.
-- ponytail: partial credit notes would carry their own bands rather than
-- borrowing the invoice's, which is a table of their own and the reason they
-- are not built yet.
SELECT i.credit_note   AS document_id,
       i.credit_note   AS document_number,
       'credit_note'   AS kind,
       i.cancelled_on  AS tax_point,
       i.currency,
       t.vat_category,
       t.vat_rate_bp,
       -t.net,
       -t.tax
  FROM invoice i
  JOIN invoice_tax t ON t.invoice_id = i.id
 WHERE i.cancelled_on IS NOT NULL;

-- What is still owed, summed rather than maintained.
--
-- A `paid` column on `invoice` would be a second thing that can be wrong, and
-- keeping it in step is the projection code most likely to double-count. Same
-- reasoning as `proj_ledger.account_balance`, and the same upgrade path if a
-- tenant ever has enough invoices for the scan to matter.
CREATE OR REPLACE VIEW invoice_status AS
SELECT i.id,
       i.number,
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
  FROM invoice i
  LEFT JOIN invoice_payment p ON p.invoice_id = i.id
 GROUP BY i.id;
