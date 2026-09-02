-- Signing in with a phone number.
--
-- # Why this market needs it
--
-- A phone number is the identity here and an email address often is not. A
-- login that insists on one excludes people who have a phone, a bank account
-- and a business, and no inbox they read.
--
-- # What is stored, and what that is honestly worth
--
-- The code is six digits — twenty bits — so a SHA-256 of it in a stolen dump is
-- reversible by anybody with a laptop and a million guesses. **The hash is not
-- what protects it.** What protects it is that the code lives five minutes, is
-- single use, and dies after a handful of wrong attempts; the digest only
-- stops a casual `SELECT *` from being a login.
--
-- Saying so here rather than implying otherwise with an expensive hash: an
-- Argon2 over a six-digit code would cost every verification and buy the same
-- nothing.
--
-- # Two limits, because they fail differently
--
-- **Requesting** a code is limited by a cooldown per number — the failure is
-- somebody using this system to send texts, which costs money and annoys the
-- person whose number it is. **Verifying** one is limited by attempts on the
-- code itself — the failure is guessing, and a million guesses against twenty
-- bits is minutes.
--
-- One limiter would have to be the stricter of the two everywhere, which makes
-- the ordinary case worse to defend against the rarer one.

ALTER TABLE authenticator DROP CONSTRAINT authenticator_kind_check;
ALTER TABLE authenticator ADD CONSTRAINT authenticator_kind_check
    CHECK (kind IN ('password', 'api_key', 'phone'));

CREATE TABLE one_time_code (
    id           UUID PRIMARY KEY,

    -- E.164, normalised by the caller: `+966500000000`. Compared exactly.
    handle       TEXT NOT NULL CHECK (handle ~ '^\+[1-9][0-9]{7,14}$'),
    -- SHA-256 of the code. See above for what that is and is not worth.
    code_hash    BYTEA NOT NULL CHECK (length(code_hash) = 32),

    -- Who it is for, when the number is already known. **Null for a number
    -- nobody has signed in with**, and the identity is created when the code is
    -- verified — because creating one on *request* would let anybody fill the
    -- table with accounts by typing numbers.
    identity_id  UUID REFERENCES identity(id) ON DELETE CASCADE,

    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    -- Single use. Set the moment it is accepted, in the same statement that
    -- accepts it, so two requests racing with the same code resolve to one.
    used_at      TIMESTAMPTZ,
    -- Wrong guesses. The code is dead once this reaches the limit, which is the
    -- second of the two limiters above.
    attempts     INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0)
);

-- "The newest live code for this number", which is both the verify lookup and
-- the cooldown check.
CREATE INDEX one_time_code_handle_idx ON one_time_code (handle, created_at DESC);
-- Sweeping what has expired.
CREATE INDEX one_time_code_expiry_idx ON one_time_code (expires_at);
