-- Somewhere a module can keep a secret.
--
-- # Why a module needed this
--
-- A module has three places to put state, and until now all three were wrong
-- for a private key:
--
--   the event log     immutable and replayed forever — a key written here can
--                     never be rotated out, and every replica of the log has it
--   `configuration`   a tenant's settings: read by anything that can read the
--                     tenant, and meant to be looked at
--   `proj_<module>`   dropped and rebuilt from the log by `rebuild_swap`, so
--                     anything not derived from the log is destroyed by an
--                     ordinary maintenance operation
--
-- ZATCA onboarding produces exactly what none of those can hold: an ECDSA
-- private key and a CSID secret, which are **not derived from anything**, must
-- survive a rebuild, must be rotatable, and must not be readable by everything
-- that can read the tenant.
--
-- # Why core owns the table and no module owns the schema
--
-- The *mechanism* is core — the same argument as `configuration` and
-- `document_number`, which are also core tables that only modules use. What
-- core does not hold is any opinion about what is in them: this table stores
-- opaque sealed bytes under a module's own key, and core cannot read them.
--
-- # Sealed, not stored
--
-- `sealed` is AES-256-GCM ciphertext under a key this database never sees — it
-- comes from the deployment's environment. A stolen backup, a replica, or a
-- `SELECT *` yields ciphertext and nothing else, which is the property that
-- makes putting a signing key in Postgres defensible at all.
--
-- The nonce is the first 12 bytes and the GCM tag the last 16, so a row is
-- self-describing and there is no second column to get out of step with the
-- first. See `erp_eventlog::secrets`.
CREATE TABLE IF NOT EXISTS module_secret (
    -- `<module>.<name>` — `tax_sa.zatca.production` and so on. The module
    -- prefix is convention, not enforcement: core has no list of modules and
    -- is not the right place to grow one.
    key         TEXT PRIMARY KEY CHECK (key <> ''),

    -- nonce ‖ ciphertext ‖ tag. Opaque here by construction.
    sealed      BYTEA NOT NULL CHECK (octet_length(sealed) > 28),

    -- Which sealing key this was sealed under, so a rotation can find what it
    -- has not re-sealed yet. An identifier, never the key.
    sealed_with TEXT NOT NULL,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- What a rotation sweeps.
CREATE INDEX IF NOT EXISTS module_secret_by_sealing_key_idx
    ON module_secret (sealed_with);
