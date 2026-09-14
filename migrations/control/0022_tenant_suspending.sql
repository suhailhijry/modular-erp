-- A tenant being suspended drains before it stops.
--
-- Suspending a tenant used to stop everything at once, its ZATCA reporting
-- included, so a suspension longer than a day pushed the simplified invoices
-- issued just before it past their 24-hour reporting window — a breach the
-- customer did not choose. Decided by the product owner on 2026-09-14: a
-- suspension has two steps. `suspending` closes every door exactly as
-- `suspended` does, but the worker keeps visiting the tenant for the two jobs
-- that sign and report its issued documents, and moves it to `suspended` once
-- nothing is left for them. Reinstating works from either.
--
-- Both constraints are **widened**, not narrowed: every row the previous
-- build can write satisfied the old rule and satisfies the new one. A CHECK
-- cannot be altered in place, so each is dropped and added back wider.
ALTER TABLE tenant
    DROP CONSTRAINT IF EXISTS tenant_status_check;
ALTER TABLE tenant
    ADD CONSTRAINT tenant_status_check
    CHECK (status IN ('provisioning', 'active', 'suspending', 'suspended', 'deleted'));

-- The reason and the instant belong to both steps: they are written when
-- staff act, and the move from `suspending` to `suspended` keeps them.
ALTER TABLE tenant
    DROP CONSTRAINT IF EXISTS tenant_suspension_is_complete;
ALTER TABLE tenant
    ADD CONSTRAINT tenant_suspension_is_complete CHECK (
        (status IN ('suspending', 'suspended')
            AND suspended_at IS NOT NULL AND suspended_reason IS NOT NULL
            AND length(btrim(suspended_reason)) BETWEEN 1 AND 500) OR
        (status NOT IN ('suspending', 'suspended')
            AND suspended_reason IS NULL AND suspended_at IS NULL)
    );

COMMENT ON COLUMN tenant.status IS
    'provisioning, active, suspending (doors shut, ZATCA reporting draining), suspended, or deleted.';
