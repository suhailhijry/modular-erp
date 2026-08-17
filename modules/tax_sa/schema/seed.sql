-- What this module cannot work without, as data.
--
-- Runs after `install.sql`, under the same `search_path` — so `public.` names
-- the tenant's own tables and an unqualified name would land in `proj_tax_sa`.
-- Idempotent, because a projection rebuild runs it again.

-- **The Saudi rate.**
--
-- This is the whole reason a country is a module: `ledger` owns the shape of a
-- rate and has no opinion about the number, and this is where the number comes
-- from. 15% since July 2020.
--
-- `DO NOTHING`, so enabling this never overwrites a rate a tenant set. A
-- business that corrected it keeps their correction — which is also what makes
-- it safe for `refresh_module` to run this again on a rebuild.
INSERT INTO public.configuration (key, value, version, set_by)
VALUES (
    'ledger.vat_rates',
    '{"standard":1500}'::jsonb,
    nextval('public.configuration_version'),
    'module:tax_sa'
)
ON CONFLICT (key) DO NOTHING;
