-- How an identity proves it is itself, and how it stays proven.

-- One row per way of logging in. Password today; OIDC and API keys are more
-- rows, not more tables.
CREATE TABLE authenticator (
    id          UUID PRIMARY KEY,
    identity_id UUID NOT NULL REFERENCES identity(id) ON DELETE CASCADE,

    kind        TEXT NOT NULL CHECK (kind IN ('password')),
    -- The login handle. Lowercased by the caller; compared exactly.
    handle      TEXT NOT NULL CHECK (length(handle) BETWEEN 3 AND 320),

    -- Argon2id PHC string: algorithm, parameters and salt travel with it, so
    -- raising the cost later does not need a migration.
    secret      TEXT NOT NULL,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT authenticator_handle_is_unique_per_kind UNIQUE (kind, handle)
);

CREATE INDEX authenticator_identity_idx ON authenticator (identity_id);

-- Live sessions.
--
-- The token itself is never stored — only its SHA-256. A token is 256 bits of
-- entropy, so unlike a password there is nothing to brute-force and no need for
-- a slow hash; the point is only that a leaked database dump cannot be replayed.
CREATE TABLE session (
    token_hash  BYTEA PRIMARY KEY CHECK (length(token_hash) = 32),
    identity_id UUID NOT NULL REFERENCES identity(id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Logging out everywhere, and sweeping what has expired.
CREATE INDEX session_identity_idx ON session (identity_id);
CREATE INDEX session_expiry_idx ON session (expires_at);
