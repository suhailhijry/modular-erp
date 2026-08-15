-- Tenant configuration: the values a business chooses that are not documents.
--
-- Which accounts a sale posts to, today. Posting rules and account
-- determination later (ARCHITECTURE §6).
--
-- # Why this lives in the tenant database
--
-- Two reasons, and the second is the one that matters. It is tenant business
-- data, changed by the tenant, not by billing — entitlements live in the
-- control plane precisely because they are the other kind. And a command has to
-- resolve configuration **inside its own transaction**, so what it wrote and
-- what it resolved against cannot disagree; a value one round trip away in
-- another database cannot give that.
--
-- # Why there is a version
--
-- Architecture L5: an event carries the *outcome* of a decision, never a
-- reference to the configuration that produced it — so replaying an old invoice
-- must not pick up today's accounts. The resolved values go into the event, and
-- this version goes into its metadata, which makes "what was configured when
-- this was decided?" answerable without making the answer load-bearing.

-- Shared by every key, so `max(version)` is a single number describing the
-- whole of a tenant's configuration at a moment. Not gapless and not required
-- to be: unlike the event log, nothing counts these.
CREATE SEQUENCE configuration_version;

CREATE TABLE configuration (
    -- Namespaced by module — `sales.posting_accounts`. The module that owns the
    -- meaning owns the prefix.
    key       TEXT PRIMARY KEY CHECK (length(key) BETWEEN 1 AND 128),

    -- Validated by the type it deserializes into, never trusted as read. Data
    -- written by an older version of the system is exactly where "it was valid
    -- when we wrote it" stops being a guarantee.
    value     JSONB NOT NULL,

    version   BIGINT NOT NULL,
    set_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The identity that set it, as text. No foreign key: identities live in the
    -- control plane, across a database boundary (ARCHITECTURE §2).
    set_by    TEXT
);

CREATE INDEX configuration_version_idx ON configuration (version);
