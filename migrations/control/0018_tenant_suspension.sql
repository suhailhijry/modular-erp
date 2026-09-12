-- Why a tenant is suspended, and since when.
--
-- The same pair `identity` has had from the start, with the same rule: a
-- suspended tenant says why and since when, and any other one carries neither.
-- **In the schema, not in `suspend_tenant`**, so a hand edit cannot leave a
-- tenant suspended for no recorded reason either. The audit entry is the
-- history; these columns are the current state and the thing the rule hangs on.
--
-- `suspended_reason IS NOT NULL` is load-bearing: `length(btrim(NULL))` is NULL,
-- and a CHECK that comes out NULL passes.
ALTER TABLE tenant
    ADD COLUMN IF NOT EXISTS suspended_reason TEXT,
    ADD COLUMN IF NOT EXISTS suspended_at     TIMESTAMPTZ;

-- Nothing before this migration suspended a tenant, but a row set by hand would
-- stop the constraint below from being added at all. Complete it instead.
UPDATE tenant
   SET suspended_reason = 'suspended before reasons were recorded',
       suspended_at     = now()
 WHERE status = 'suspended' AND suspended_reason IS NULL;

ALTER TABLE tenant ADD CONSTRAINT tenant_suspension_is_complete CHECK (
    (status = 'suspended' AND suspended_at IS NOT NULL AND suspended_reason IS NOT NULL
                          AND length(btrim(suspended_reason)) BETWEEN 1 AND 500) OR
    (status <> 'suspended' AND suspended_reason IS NULL AND suspended_at IS NULL)
);

COMMENT ON COLUMN tenant.suspended_reason IS
    'Why staff suspended this tenant, written for its owner. Also recorded in the audit trail as tenant.suspended.';
