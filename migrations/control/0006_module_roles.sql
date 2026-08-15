-- A different role in a different module.
--
-- # Why one role per tenant stopped being enough
--
-- The second module made it concrete: "Sara does the invoicing, Khalid does the
-- books" is the most ordinary arrangement in a small company, and with a single
-- role one of them has to be an accountant for both. The tenant-wide role stays
-- the default — most people have exactly one job — and this table is the
-- exception, not the replacement.
--
-- Additive on purpose: every existing membership keeps working, with no rows
-- here and no migration of anybody's access.

CREATE TABLE membership_module_role (
    membership_id UUID NOT NULL REFERENCES membership (id) ON DELETE CASCADE,

    -- Not a foreign key to `entitlement`: a role can be set before a module is
    -- enabled and must survive it being turned off and on again, which is the
    -- same reason disabling a module does not delete its data.
    module_id     TEXT NOT NULL CHECK (length(module_id) BETWEEN 1 AND 48),
    role          TEXT NOT NULL CHECK (length(role) BETWEEN 1 AND 64),

    set_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (membership_id, module_id)
);
