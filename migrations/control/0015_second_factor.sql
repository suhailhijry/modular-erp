-- A second factor, and the codes that get somebody back in without one.
--
-- **More rows, not more tables**, which is what `0004_authentication.sql`
-- predicted when it said "OIDC and API keys are more rows". The prediction was
-- half wrong for API keys, which needed their own table for the public half and
-- the scopes; it is exactly right here. A TOTP enrolment and a recovery code
-- are both *a way an identity proves it is itself*, which is what this table
-- is.
--
-- The `kind` check is widened rather than dropped: an unknown kind is a bug in
-- a caller, and a column that accepts anything would let one land silently.
-- **Every kind that already exists is carried forward.** `0012` added
-- `api_key` and `0013` added `phone`; a widening that lists only what this
-- migration cares about would silently take those away and break every API key
-- and every phone login on the next deploy. The list is cumulative, always.
ALTER TABLE authenticator
    DROP CONSTRAINT IF EXISTS authenticator_kind_check;

ALTER TABLE authenticator
    ADD CONSTRAINT authenticator_kind_check
    CHECK (kind IN (
        'password',
        'api_key',
        'phone',
        'totp_pending',
        'totp',
        'recovery'
    ));

-- **What `secret` holds, per kind**, because it is three different things and
-- a reader deserves to be told rather than to infer it:
--
--   password  an Argon2id PHC string. Verified, never read back.
--   totp      the shared secret, **sealed** (AES-256-GCM) and base64'd. It has
--             to be recoverable to compute a code, so it cannot be hashed —
--             which is exactly why it is encrypted instead.
--   totp_pending
--             the same, for an enrolment nobody has proved yet. A separate
--             kind rather than a `confirmed_at` column: a pending enrolment
--             must never satisfy a login, and a row that is simply *not there*
--             under `kind = 'totp'` cannot be missed by a query that forgot to
--             check a nullable column. It is also trivially sweepable.
--   recovery  the SHA-256 of one single-use code, hex. High entropy, so a fast
--             hash is right for the same reason it is right for a session
--             token: there is nothing to brute-force.
--
-- **And what `handle` holds**, since `UNIQUE (kind, handle)` makes it the key:
--
--   password  the login address, lowercased.
--   totp      the identity's own id. One enrolment per person; enrolling again
--             replaces it, which is what "I lost my phone and still have my
--             recovery codes" has to do.
--   recovery  `<identity>:<n>`, so ten codes are ten rows that cannot collide.
COMMENT ON COLUMN authenticator.secret IS
    'password: Argon2id PHC. totp: sealed shared secret, base64. recovery: SHA-256 of one single-use code, hex.';

-- Finding somebody's second factor is a per-identity question, and
-- `authenticator_identity_idx` already answers it. No new index.
