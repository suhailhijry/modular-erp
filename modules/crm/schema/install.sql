-- The customer list.
--
-- Schema-relative on purpose: every name here is unqualified, so the same file
-- installs into `proj_crm` during provisioning and into a staging schema during
-- `rebuild_swap`. See `modules/ledger/schema/install.sql` for the full argument.
--
-- `IF NOT EXISTS` throughout, because everything here is derived from the log.
-- A changed read model is answered by dropping the schema and replaying, never
-- by a migration.

CREATE TABLE IF NOT EXISTS customer (
    -- The client-chosen id, which is what a document references.
    id           TEXT PRIMARY KEY,

    -- What a document would print. Arabic for a Saudi business, which is why
    -- the Latin spelling is a separate optional column and not a translation.
    name         TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    name_latin   TEXT,

    kind         TEXT NOT NULL CHECK (kind IN ('person', 'company')),

    phone        TEXT,
    email        TEXT,
    -- One of the two, enforced here as well as in the command.
    --
    -- Not belt and braces: this table is rebuilt from the log, and a rule that
    -- lives only in the command is a rule an older event can walk straight
    -- past. The constraint is what makes a replay of a bad event fail loudly
    -- instead of quietly producing a customer nobody can reach.
    CONSTRAINT customer_is_reachable CHECK (phone IS NOT NULL OR email IS NOT NULL),

    -- Address, as they are now. The copy on a document is `proj_sales`'s and
    -- does not change when this does.
    street       TEXT,
    building     TEXT,
    district     TEXT,
    city         TEXT,
    postal_code  TEXT,
    country      TEXT,

    -- The field that decides standard against simplified on their next invoice.
    vat_number   TEXT,
    id_scheme    TEXT,
    identifier   TEXT,

    -- A person does not hold a VAT registration, and an invoice to one is
    -- simplified. Allowing both would make that decision ambiguous at the
    -- moment it is taken.
    CONSTRAINT customer_person_has_no_vat_number CHECK (
        vat_number IS NULL OR kind = 'company'
    ),

    registered_on TIMESTAMPTZ NOT NULL,
    archived_at   TIMESTAMPTZ,
    archived_why  TEXT,

    -- Where this row came from in the log. For the differ, and for answering
    -- "when did this change" without loading the aggregate.
    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- The list, newest first, which is what a customer screen opens on.
CREATE INDEX IF NOT EXISTS customer_by_registered_idx
    ON customer (registered_on DESC, id DESC)
    WHERE archived_at IS NULL;

-- Finding somebody by what you remember of them. `text_pattern_ops` so a
-- prefix search uses the index; a full-text or trigram index is the upgrade
-- when somebody complains, and it is a rebuild rather than a migration.
CREATE INDEX IF NOT EXISTS customer_by_name_idx ON customer (name text_pattern_ops);
CREATE INDEX IF NOT EXISTS customer_by_phone_idx ON customer (phone) WHERE phone IS NOT NULL;
CREATE INDEX IF NOT EXISTS customer_by_email_idx ON customer (email) WHERE email IS NOT NULL;

-- **Not unique.** Two customers can share a VAT number: a group with several
-- trading entities, or the same company entered twice by two branches. That
-- second case is a reconciliation the tenant has to make, and refusing the
-- insert would stop them recording a real invoice until they had made it.
CREATE INDEX IF NOT EXISTS customer_by_vat_idx ON customer (vat_number)
    WHERE vat_number IS NOT NULL;
