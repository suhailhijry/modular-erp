-- Proving a phone number belongs to whoever is booking with it.
--
-- # Why this is not the control plane's `one_time_code`
--
-- Because that one **signs somebody in**. Verifying it mints a session and, for
-- a number nobody has used before, an identity — which is right for a member
-- logging in and wrong for a stranger booking a haircut, in two ways.
--
-- A customer is a `crm` record and not an account, so a booking form that
-- created identities would fill the fleet's identity table with people who have
-- no business there. And worse: a public form that can request a *sign-in* code
-- for any number is a way to have a staff member's phone buzz with a real code
-- that a caller can then talk them into reading out. Codes here cannot sign
-- anybody into anything, because nothing reads this table but the booking that
-- claims one.
--
-- # Why it is in the migration chain and not a projection
--
-- A code is not derived from the event log and must not be: it is a secret with
-- a lifetime, and a rebuild that replayed one would hand out a code somebody
-- already used. It is also not something to keep — see the sweep below.
CREATE TABLE booking_verification (
    id           UUID PRIMARY KEY,

    -- E.164, normalised by the caller: `+966500000000`. Compared exactly, so a
    -- number typed two ways is two rows and neither verifies the other.
    handle       TEXT NOT NULL CHECK (handle ~ '^\+[1-9][0-9]{7,14}$'),
    -- SHA-256 of the code, which stops a casual read of the table and nothing
    -- more. Six digits do not survive an offline attack whatever is done to
    -- them, which is why the defences that matter are the lifetime, the single
    -- use and the attempt limit below.
    code_hash    BYTEA NOT NULL CHECK (length(code_hash) = 32),

    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL,
    -- Single use, set in the same statement that accepts it, so two bookings
    -- racing with one code resolve to one.
    used_at      TIMESTAMPTZ,
    -- Wrong guesses. Twenty bits against unlimited guesses is minutes; against
    -- a handful it is one in two hundred thousand.
    attempts     INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0)
);

-- **The cooldown reads this**: the newest code for a number, to see how long
-- ago it was asked for. Every one of these costs the business a message.
CREATE INDEX booking_verification_by_handle
    ON booking_verification (handle, created_at DESC);

-- What the sweep deletes. A code that has expired is not evidence of anything.
CREATE INDEX booking_verification_expired ON booking_verification (expires_at);
