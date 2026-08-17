-- The Saudi tax module's read models.
--
-- Derived from the event log and dropped-and-rebuilt rather than migrated, for
-- the reasons in `modules/ledger/schema/install.sql`.

CREATE TABLE IF NOT EXISTS filed_return (
    -- The period, as the aggregate id: `SAR.2026-01-01.2026-04-01`. A period is
    -- filed once, and making the period the identity is what says so.
    id           TEXT PRIMARY KEY,

    period_from  TIMESTAMPTZ NOT NULL,
    -- Exclusive, so consecutive returns neither overlap nor leave a day out.
    period_until TIMESTAMPTZ NOT NULL CHECK (period_until > period_from),
    currency     CHAR(3) NOT NULL,

    -- Minor units, as they stood when this was filed. **Not recomputed on
    -- read**: the point of recording a filing is to have what went to ZATCA,
    -- which is a different question from what the system says today.
    output_tax   BIGINT NOT NULL,
    input_tax    BIGINT NOT NULL,
    payable      BIGINT NOT NULL CHECK (payable = output_tax - input_tax),

    -- The date the business treats the filing as made.
    filed_on     TIMESTAMPTZ NOT NULL,
    -- ZATCA's acknowledgement, once clearance exists to produce one.
    reference    TEXT,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS filed_return_by_period_idx
    ON filed_return (currency, period_from DESC);

-- ---------------------------------------------------------------------------
-- ZATCA
-- ---------------------------------------------------------------------------

-- **Who the business is, as ZATCA knows them.**
--
-- Projected from `tax_sa.taxpayer.registered` rather than read from
-- `configuration`, and the difference is the hash chain: a projection that read
-- a mutable setting would render every historic invoice with today's address on
-- the next rebuild, produce a different hash for each, and break the chain
-- silently. See `modules/tax_sa/src/taxpayer.rs`.
--
-- One row, `id = 'self'`: one solution per tenant, so one certificate and one
-- counter. A business with per-till device certificates is a real ZATCA shape
-- and not this one.
CREATE TABLE IF NOT EXISTS taxpayer (
    id            TEXT PRIMARY KEY,
    -- The whole registration, as the event carried it. Read back through
    -- `tax_sa::Registration`, which is the only thing that writes it.
    registration  JSONB NOT NULL,
    registered_on TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL
);

-- **The document ZATCA sees, and where it stands with them.**
--
-- One row per invoice and per credit note, built from `sales` events by this
-- module's projection — `sales` does not know Saudi Arabia exists, and the
-- dependency runs the other way.
CREATE TABLE IF NOT EXISTS zatca_document (
    -- The statutory number. It is the identity here because it is the identity
    -- on the document: gapless, unique, and what every other reference uses.
    id            TEXT PRIMARY KEY,
    -- The invoice this was built from, for asking "where is invoice X?".
    source_id     TEXT NOT NULL,

    -- Which obligation. `standard` is cleared before the buyer gets it;
    -- `simplified` is reported within 24 hours of issue.
    kind          TEXT NOT NULL CHECK (kind IN ('standard', 'simplified')),
    -- 388 invoice, 381 credit note, 383 debit note.
    type_code     INT  NOT NULL,

    issued_at     TIMESTAMPTZ NOT NULL,
    currency      CHAR(3) NOT NULL,
    net           BIGINT NOT NULL,
    tax           BIGINT NOT NULL,
    gross         BIGINT NOT NULL CHECK (gross = net + tax),

    -- The chain. Null only on a document that was never built, which is what
    -- `unregistered` means below.
    icv           BIGINT,
    previous_hash TEXT,
    invoice_hash  TEXT,
    -- The canonical bytes that were hashed, and the QR that goes on the print.
    -- Stored rather than re-rendered: the hash has to be over what was
    -- submitted, and a renderer that changes later would disagree with what
    -- ZATCA holds. Same argument as recording a filing.
    xml           TEXT,
    qr            TEXT,
    -- Everything the renderer worked from, for rebuilding a credit note against
    -- its invoice and for showing a person the document as structure.
    document      JSONB,

    -- The signature, once there is a certificate to make one with. Recorded
    -- rather than recomputed: ECDSA is randomised, so re-signing would produce
    -- a different signature from the one ZATCA holds.
    signature     TEXT,
    -- The document as submitted: the hashed bytes plus the signature, the QR
    -- and the `cac:Signature` that points at it.
    signed_xml    TEXT,
    signed_at     TIMESTAMPTZ,

    --   unregistered  issued before the business registered with ZATCA; no
    --                 chain position, and it cannot be cleared retrospectively
    --   pending       built, waiting to be submitted
    --   cleared       ZATCA stamped it (standard)
    --   reported      ZATCA acknowledged it (simplified)
    --   refused       ZATCA said no, and the document is what is wrong
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('unregistered', 'pending', 'cleared', 'reported', 'refused')),
    -- The signed document **the buyer must be given** — ZATCA's signature is on
    -- this one and not on ours. Clearance only.
    stamped_xml   TEXT,
    -- Whatever ZATCA had to say: warnings on an accepted document, errors on a
    -- refused one.
    remarks       JSONB,
    settled_at    TIMESTAMPTZ,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at   TIMESTAMPTZ NOT NULL
);

-- The chain is a chain: two documents cannot share a position in it.
CREATE UNIQUE INDEX IF NOT EXISTS zatca_document_icv_is_unique
    ON zatca_document (icv) WHERE icv IS NOT NULL;

CREATE INDEX IF NOT EXISTS zatca_document_by_source_idx
    ON zatca_document (source_id);

-- What the submitter sweeps: oldest first, so the 24-hour clock is respected.
-- Only signed documents — ZATCA refuses an unsigned one, so submitting it would
-- spend the tenant's rate limit to be told so.
CREATE INDEX IF NOT EXISTS zatca_document_pending_idx
    ON zatca_document (issued_at) WHERE status = 'pending' AND signed_xml IS NOT NULL;

-- And what the signer sweeps: built, chained, and not yet signed.
CREATE INDEX IF NOT EXISTS zatca_document_unsigned_idx
    ON zatca_document (issued_at) WHERE status = 'pending' AND signed_xml IS NULL;
