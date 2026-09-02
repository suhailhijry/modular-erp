-- Keys, in pairs.
--
-- # Why this is rows in `authenticator` plus a table, and not a table of its own
--
-- `0004_authentication.sql` said it: *"One row per way of logging in. Password
-- today; OIDC and API keys are more rows, not more tables."* A key proves an
-- identity exactly as a password does, so it is an authenticator — and
-- everything downstream of authentication (sessions, membership, roles, the
-- audit trail) works without learning a second shape.
--
-- What a key has that a password does not is **scopes, an expiry and a
-- predecessor**, and those are what this table holds.
--
-- # A key acts as a machine identity
--
-- Not as the person who made it. A key that carried its creator's identity
-- would die when they left, and every action it took would read in the audit
-- trail as theirs — which is a lie somebody eventually relies on.
--
-- So issuing a key creates an identity with no password, joins it to the tenant
-- with a role, and records both here. `created_by` is who asked for it, which is
-- a different and equally necessary fact.

ALTER TABLE authenticator DROP CONSTRAINT authenticator_kind_check;
ALTER TABLE authenticator ADD CONSTRAINT authenticator_kind_check
    CHECK (kind IN ('password', 'api_key'));

CREATE TABLE api_key (
    id               UUID PRIMARY KEY,

    -- The row in `authenticator` that holds the hashed secret. `handle` there
    -- is the **public** key, which is what a presented private key is looked up
    -- by.
    authenticator_id UUID NOT NULL UNIQUE
                     REFERENCES authenticator(id) ON DELETE CASCADE,

    tenant_id        UUID NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    -- The machine identity this key acts as. See the note above.
    identity_id      UUID NOT NULL REFERENCES identity(id) ON DELETE CASCADE,

    -- What a person calls it. "Booking widget", "Zapier", "the accountant's
    -- exporter" — so revoking the right one does not need a guess.
    name             TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),

    -- **What this key may do**, as `module:capability` or `*:capability`.
    --
    -- An array and not a join table: it is read on every request and written
    -- when a key is issued, which is the shape a column is for. The values are
    -- checked by the type that gives them meaning before they reach here — see
    -- `erp_control::Scope`.
    --
    -- Narrowing only. A key can never do more than the role its identity holds
    -- in the tenant; the scopes are a second gate in front of it, so an
    -- integration that reads bookings cannot post journal entries **even if**
    -- somebody gives its identity the owner's role by mistake.
    scopes           TEXT[] NOT NULL CHECK (cardinality(scopes) BETWEEN 1 AND 64),

    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Who asked for it. Null once that person is erased, which is why this is
    -- not the identity the key acts as.
    created_by       UUID REFERENCES identity(id) ON DELETE SET NULL,

    -- **Best effort, and deliberately not on the request path's critical
    -- section.** Written when a key is used, so "is anything still using this"
    -- is answerable before somebody revokes it. A lost update here costs a
    -- slightly stale timestamp and nothing else.
    last_used_at     TIMESTAMPTZ,

    -- When it stops working. **Rotation sets this on the old key** rather than
    -- revoking it, which is the overlap window: a key that cannot be rotated
    -- without downtime is a key nobody rotates.
    expires_at       TIMESTAMPTZ,

    revoked_at       TIMESTAMPTZ,
    revoked_why      TEXT,

    -- The key this one replaced, so a rotation is visible as one.
    rotated_from     UUID REFERENCES api_key(id) ON DELETE SET NULL,

    CONSTRAINT api_key_revocation_is_complete CHECK (
        (revoked_at IS NULL AND revoked_why IS NULL) OR
        (revoked_at IS NOT NULL AND revoked_why IS NOT NULL)
    )
);

-- "What keys does this tenant have", which is the only listing there is.
CREATE INDEX api_key_tenant_idx ON api_key (tenant_id, created_at DESC);
