-- Values for the fields a business added to its customers.
--
-- # Why this is not in the event log
--
-- **Because an append-only log cannot forget, and this has to be forgettable.**
-- A wellness centre's custom fields hold a person's health details; under the
-- PDPL those carry a right to erasure, and this system has already had to fix
-- one place where the answer was "our schema will not let us".
--
-- Writing them into `crm`'s log would make that answer permanent. So the
-- *shape* is declared — `crm.fields`, versioned configuration, replayed with
-- everything else — and the *values* live here, where a delete is a delete.
-- The same call `payments` made for a card token, for the same reason.
--
-- # What that costs, and what it does not
--
-- A rebuild does not reproduce these. That is correct rather than unfortunate:
-- a rebuild is a function of the log, and if it could reproduce them the delete
-- would not have been one. `crm::install` must therefore never drop this table,
-- which is why it is here and not in `proj_crm`.
--
-- What is **not** lost is the history. A value is superseded rather than
-- overwritten, so "what did this say in March, and who changed it" is
-- answerable — and erasing takes the superseded rows with it, because a
-- deletion that left the old value behind would not be one.
CREATE TABLE customer_field (
    id           UUID PRIMARY KEY,

    -- The `crm` customer. A reference and not a foreign key: `proj_crm` is a
    -- projection that is dropped and rebuilt, and a constraint into it would
    -- take these with it.
    customer     TEXT NOT NULL,
    -- The field's stable key, from `crm.fields`. Not the label: a label is what
    -- a person reads and may be corrected or translated at any time, and a
    -- stored value must not stop being findable because somebody fixed a typo.
    field        TEXT NOT NULL,

    -- **One column per kind, exactly one filled.** A single text column would
    -- make "everybody over sixty" a string comparison, and the whole reason
    -- these are typed is that they can be asked about.
    text_value   TEXT,
    number_value BIGINT,
    date_value   TIMESTAMPTZ,
    flag_value   BOOLEAN,
    CONSTRAINT customer_field_holds_one_thing CHECK (
        (text_value   IS NOT NULL)::int
      + (number_value IS NOT NULL)::int
      + (date_value   IS NOT NULL)::int
      + (flag_value   IS NOT NULL)::int = 1
    ),

    -- Who set it and when. This is the audit trail these values get, and it is
    -- the reason a change supersedes rather than overwrites.
    set_at       TIMESTAMPTZ NOT NULL,
    set_by       TEXT,
    -- Null on the value that is current. Set when another replaces it.
    superseded_at TIMESTAMPTZ
);

-- **One current value per field per customer**, enforced rather than assumed:
-- two rows claiming to be current is a customer page that shows whichever came
-- back first.
CREATE UNIQUE INDEX customer_field_current
    ON customer_field (customer, field)
    WHERE superseded_at IS NULL;

-- "Everything on this customer's page", which is the read every screen makes.
CREATE INDEX customer_field_by_customer
    ON customer_field (customer)
    WHERE superseded_at IS NULL;

-- **"Who is allergic to latex?"** — the question a blob cannot answer. One
-- index per kind that is worth filtering on, over the current values only.
CREATE INDEX customer_field_by_text
    ON customer_field (field, text_value)
    WHERE superseded_at IS NULL AND text_value IS NOT NULL;

CREATE INDEX customer_field_by_number
    ON customer_field (field, number_value)
    WHERE superseded_at IS NULL AND number_value IS NOT NULL;

CREATE INDEX customer_field_by_date
    ON customer_field (field, date_value)
    WHERE superseded_at IS NULL AND date_value IS NOT NULL;
