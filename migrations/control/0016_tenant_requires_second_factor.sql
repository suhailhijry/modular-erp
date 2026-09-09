-- A tenant that insists its members protect their accounts.
--
-- **On the tenant row, not in the tenant's own configuration**, because the
-- question is asked *during* entry: `ControlPlane::enter` already reads a
-- cached tenant, so this costs nothing to check and needs no second lookup —
-- and a flag stored inside the tenant database would have to be read after
-- deciding whether the caller may reach that database, which is the wrong way
-- round.
--
-- Expand-only: a new column with a default, which existing pods neither select
-- nor insert.
ALTER TABLE tenant
    ADD COLUMN IF NOT EXISTS requires_second_factor BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN tenant.requires_second_factor IS
    'When true, a member with no second factor is refused at entry. The session stays valid and other tenants stay reachable — see ControlPlane::enter.';
