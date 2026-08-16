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

-- **The Saudi rate, seeded when the module is enabled.**
--
-- This is the whole reason a country is a module: `ledger` owns the shape of a
-- rate and has no opinion about the number, and this is where the number comes
-- from. 15% since July 2020.
--
-- `DO NOTHING`, so enabling this never overwrites a rate a tenant set. A
-- business that corrected it keeps their correction.
--
-- ponytail: seeding data from the schema install is riding on the only hook a
-- module has. A module wants a `seed` step distinct from its DDL — this insert
-- is idempotent so a rebuild re-running it is harmless, which is what makes the
-- shortcut safe rather than merely convenient.
--
-- `configuration` lives in the tenant's `public` schema, which is on the
-- `search_path` this runs under; everything above is schema-relative and lands
-- in `proj_tax_sa`.
INSERT INTO public.configuration (key, value, version, set_by)
VALUES (
    'ledger.vat_rates',
    '{"standard":1500}'::jsonb,
    nextval('public.configuration_version'),
    'module:tax_sa'
)
ON CONFLICT (key) DO NOTHING;
