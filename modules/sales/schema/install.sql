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

    -- **The reference, beside the copy above.** Points at a `crm` record when
    -- the invoice named one.
    --
    -- Deliberately not a foreign key, and it could not be one: `proj_crm` is a
    -- different projection group on its own checkpoint, and a constraint across
    -- them would make one group's rebuild depend on another's (L3). The check
    -- that this names a real customer happens once, at issue, against the log.
    --
    -- Nullable for ever. Invoices issued before customers were records have
    -- none, and a walk-in at a till has none.
    customer_id  TEXT,

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

    -- **Whether this billed for money taken before the supply.** A deposit.
    -- It changes what the document is to the authority — a prepayment invoice
    -- rather than an ordinary one — because receiving consideration is itself a
    -- tax point.
    prepayment   BOOLEAN NOT NULL DEFAULT FALSE,

    note         TEXT NOT NULL DEFAULT '',

    -- Cancelled by a credit note. The invoice stays: accounting does not
    -- delete, and a document that was issued was issued. What changes is that
    -- nobody owes anything on it.
    cancelled_on TIMESTAMPTZ,
    credit_note  TEXT,
    -- **The prepayment invoice this one deducted**, by number, when it is the
    -- final invoice after a deposit. Its own net, tax and gross are what it
    -- charges — the rest of the supply — and the lines say the whole.
    prepaid_number TEXT,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL
);

-- A repeated number would mean the series went backwards, which is the one
-- failure mode gaplessness exists to prevent. A constraint rather than a test,
-- because a projection that could write it twice must fail loudly (L6).
CREATE UNIQUE INDEX IF NOT EXISTS invoice_number_is_unique ON invoice (number);

CREATE INDEX IF NOT EXISTS invoice_by_date_idx ON invoice (issued_on DESC);
CREATE INDEX IF NOT EXISTS invoice_by_customer_idx ON invoice (customer);
CREATE INDEX IF NOT EXISTS invoice_by_customer_id_idx ON invoice (customer_id)
    WHERE customer_id IS NOT NULL;

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

-- What was taken off **one line**, and why.
--
-- Its own table rather than columns on `invoice_line`, because a line may carry
-- several and each is printed as its own figure — UBL's `cac:AllowanceCharge`
-- inside `cac:InvoiceLine`.
--
-- **No tax treatment here**, unlike `invoice_discount`. A line allowance
-- reduces the line, and the line already says how it is taxed; a document
-- discount is attached to nothing, so it has to name what it comes off.
CREATE TABLE IF NOT EXISTS invoice_line_allowance (
    id             UUID PRIMARY KEY,
    invoice_id     TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
    line_index     INT  NOT NULL CHECK (line_index >= 0),
    allowance_index INT NOT NULL CHECK (allowance_index >= 0),

    reason         TEXT NOT NULL,
    -- Positive: what comes off. `invoice_line.net` is already net of it.
    amount         BIGINT NOT NULL CHECK (amount > 0),

    CONSTRAINT invoice_line_allowance_is_unique
        UNIQUE (invoice_id, line_index, allowance_index)
);

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
    -- recording one twice a no-op — and which means a **refund must not reuse a
    -- payment's reference on the same invoice**: they are two movements of
    -- money and cannot be one fact.
    reference    TEXT NOT NULL,
    -- **Signed.** Positive is money in, negative is money handed back. One
    -- table rather than two, so `paid` is a single sum and no read has to
    -- remember to consult a second place before saying what an invoice holds.
    amount       BIGINT NOT NULL CHECK (amount <> 0),
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
-- A credit note against part of an invoice.
--
-- **Its own table because it is its own document.** A whole-invoice
-- cancellation is a fact *about* the invoice — it reverses that invoice's entry
-- and negates that invoice's bands — so it lives as two columns on `invoice`.
-- A partial credit note is not: it has its own lines, its own tax breakdown and
-- its own tax point, and the authority computes its VAT from those rather than
-- from the invoice it references.
CREATE TABLE IF NOT EXISTS credit_note (
    -- The statutory number, from the same gapless series a cancellation draws
    -- on. Both are credit notes and ZATCA does not care which shape made one.
    id            TEXT PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES invoice (id) ON DELETE CASCADE,
    -- The client's own key. What makes a retry a no-op.
    reference     TEXT NOT NULL,

    currency      CHAR(3) NOT NULL,
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,
    gross         BIGINT NOT NULL CHECK (gross = net + tax),

    reason        TEXT NOT NULL DEFAULT '',
    -- **The credit note's own tax point**, not the invoice's. A credit note
    -- falls in the period it was issued in, which is the whole argument the
    -- `vat_entry` view makes below.
    issued_on     TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL,

    CONSTRAINT credit_note_reference_is_unique UNIQUE (invoice_id, reference)
);

CREATE INDEX IF NOT EXISTS credit_note_by_invoice_idx
    ON credit_note (invoice_id, issued_on DESC);

CREATE TABLE IF NOT EXISTS credit_note_line (
    id             UUID PRIMARY KEY,
    credit_note_id TEXT NOT NULL REFERENCES credit_note (id) ON DELETE CASCADE,
    line_index     INT  NOT NULL CHECK (line_index >= 0),

    -- **Which line of the invoice this credits.** The description and the rate
    -- below were taken from it, which is what stops a credit note describing
    -- something the invoice never charged for.
    against        INT  NOT NULL CHECK (against >= 0),

    description    TEXT NOT NULL,
    net            BIGINT NOT NULL,
    -- **The rate the invoice charged**, never today's. A 2019 invoice is
    -- credited at 5% for ever.
    vat_category   TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp    INT  NOT NULL CHECK (vat_rate_bp >= 0),

    CONSTRAINT credit_note_line_is_unique UNIQUE (credit_note_id, line_index)
);

-- The credit note's own tax breakdown. **Not the invoice's negated** — that is
-- what a whole-invoice cancellation does, and it is exactly what a partial one
-- must not do.
CREATE TABLE IF NOT EXISTS credit_note_tax (
    id             UUID PRIMARY KEY,
    credit_note_id TEXT NOT NULL REFERENCES credit_note (id) ON DELETE CASCADE,
    vat_category   TEXT NOT NULL CHECK (vat_category IN ('standard', 'zero', 'exempt')),
    vat_rate_bp    INT  NOT NULL CHECK (vat_rate_bp >= 0),
    net            BIGINT NOT NULL,
    tax            BIGINT NOT NULL,

    CONSTRAINT credit_note_tax_is_unique UNIQUE (credit_note_id, vat_category, vat_rate_bp)
);

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

-- The adjustment, negating the same bands the invoice declared. A **whole
-- invoice** cancellation reverses every band of it, so it can borrow the
-- invoice's own breakdown.
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
 WHERE i.cancelled_on IS NOT NULL

UNION ALL

-- **A partial credit note carries its own bands**, which is why it needed a
-- table rather than a pair of columns. Borrowing the invoice's would credit the
-- whole supply for a document that credited part of it — and the return would
-- be wrong by the difference, in the direction of tax the business never got
-- back.
SELECT c.id            AS document_id,
       c.id            AS document_number,
       'credit_note'   AS kind,
       c.issued_on     AS tax_point,
       c.currency,
       t.vat_category,
       t.vat_rate_bp,
       -t.net,
       -t.tax
  FROM credit_note c
  JOIN credit_note_tax t ON t.credit_note_id = c.id;

-- What is still owed, summed rather than maintained.
--
-- A `paid` column on `invoice` would be a second thing that can be wrong, and
-- keeping it in step is the projection code most likely to double-count. Same
-- reasoning as `proj_ledger.account_balance`.
--
-- **This shape is for readers that scan**: the receivables report and the
-- overpayment health check both visit every unpaid invoice, and a merge join
-- with one grouped pass is the cheapest way to do that. `invoice_row` below is
-- the same numbers for readers that want one invoice or one page, and the two
-- are asserted equal in `sales/tests`.
CREATE OR REPLACE VIEW invoice_status AS
SELECT i.id,
       i.number,
       i.customer,
       i.customer_vat,
       i.customer_id,
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
       -- Net of refunds, because `amount` is signed. This is what the business
       -- is holding, which is the number `outstanding` below is derived from.
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

-- The same numbers, for a reader that wants one invoice or one page of them.
--
-- # Why this exists rather than one view for everything
--
-- `GROUP BY i.id` cannot preserve `issued_on DESC` order, so a paged read
-- through `invoice_status` has to aggregate **every** invoice in the tenant,
-- sort the lot, and throw away all but twenty rows. Measured on 200,000
-- invoices and 400,000 payments: 410 ms and 443,000 buffers to return one page.
--
-- Correlating the sum per row instead lets the planner walk `invoice_by_date_idx`,
-- stop after twenty, and look up only those twenty invoices' payments. Same
-- measurement: **0.3 ms and 118 buffers**.
--
-- It is not the better shape in general, which is why it is a second view and
-- not a replacement. Run the receivables report through this one and each of
-- the 200,000 invoices costs an index lookup: 760 ms against the grouped view's
-- 292 ms. One view per access pattern, each fast at the thing it is for.
CREATE OR REPLACE VIEW invoice_row AS
SELECT i.id,
       i.number,
       i.customer,
       i.customer_vat,
       i.customer_id,
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
       p.paid,
       CASE WHEN i.cancelled_on IS NOT NULL THEN 0
            ELSE (i.gross - p.paid)
       END::BIGINT AS outstanding,
       p.payments,
       -- Last, because a view can only ever gain columns at its end: a
       -- `CREATE OR REPLACE` that inserts one in the middle is refused.
       i.prepaid_number
  FROM invoice i
  LEFT JOIN LATERAL (
      SELECT COALESCE(sum(amount), 0)::BIGINT AS paid,
             count(*)                         AS payments
        FROM invoice_payment
       WHERE invoice_id = i.id
  ) p ON TRUE;
