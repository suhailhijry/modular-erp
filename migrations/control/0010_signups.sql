-- Signups that have not proved their email address yet.
--
-- ===========================================================================
-- What this table exists to stop
-- ===========================================================================
--
-- `POST /v1/signups` is unauthenticated by definition — the point is to arrive
-- without an account. Until this table existed, every call that got past
-- validation ran `CREATE DATABASE` and a full migration chain, so a shell loop
-- from the open internet cost the attacker one HTTP request and cost the
-- operator one database. At the fleet size this is built for that is a disk,
-- not an inconvenience.
--
-- It also claimed the login handle. Signing up as `ceo@bigcorp.example` created
-- an authenticator under that address with a password of the attacker's
-- choosing, and the real owner could then never sign up: they would have to
-- prove a password they never set.
--
-- So nothing is created until the address answers. A request writes one row
-- here and one outbox effect, and *that is all* — no identity, no
-- authenticator, no tenant, no database. Confirming the link is what builds
-- them.
--
-- ===========================================================================
-- Why the password is here and not in `authenticator`
-- ===========================================================================
--
-- Because writing it to `authenticator` is exactly the handle claim above. The
-- hash sits here until the address is proved, and moves across on confirmation.
--
-- Same posture as `authenticator.secret`: an Argon2id PHC string, so the
-- parameters travel with it and a leaked dump is no more useful than a leaked
-- dump of the real table.
--
-- `identity_id` is the other half. An address that **already** has an account
-- proved itself with its password at request time, so there is no second
-- password to store and nothing to create later — the row names the identity
-- instead. Exactly one of the two is set, and the constraint says so, because
-- "both" would mean a confirmation with two different answers about who this is.

CREATE TABLE pending_signup (
    id            UUID PRIMARY KEY,

    -- The token is never stored, only its SHA-256 — the same reasoning as
    -- `session` and `invitation`. A leaked dump must not contain working links,
    -- and 256 bits of entropy needs no slow hash.
    token_hash    BYTEA NOT NULL UNIQUE CHECK (length(token_hash) = 32),

    -- Who has to answer. Confirmation binds to *this* address and no other, so
    -- a forwarded link cannot become somebody else's account.
    handle        TEXT NOT NULL CHECK (length(handle) BETWEEN 3 AND 320),

    -- What will be built, if they answer. The same rules `tenant` enforces, so
    -- a slug that cannot become a tenant cannot be requested either — found at
    -- the form rather than after the round trip through a mailbox.
    slug          TEXT NOT NULL
                  CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,48}[a-z0-9]$'),
    company       TEXT NOT NULL CHECK (length(company) BETWEEN 1 AND 200),
    modules       TEXT[] NOT NULL,

    -- Exactly one. See the header.
    identity_id   UUID REFERENCES identity (id) ON DELETE CASCADE,
    password_hash TEXT,

    expires_at    TIMESTAMPTZ NOT NULL,
    confirmed_at  TIMESTAMPTZ,
    cancelled_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT pending_signup_names_one_owner CHECK (
        (identity_id IS NULL) <> (password_hash IS NULL)
    )
);

-- At most one live request per address.
--
-- Requesting again cancels the previous one in the same transaction, so this is
-- what stops an address accumulating live links: revoking one of several ways
-- in is not revoking, which is the same argument as
-- `invitation_one_outstanding_per_handle` one table over.
--
-- Deliberately **not** `AND expires_at > now()`: a partial index cannot be
-- built on a non-immutable function. An expired-but-unswept row still holds the
-- slot, and re-requesting cancels it, so the slot is never stuck.
CREATE UNIQUE INDEX pending_signup_one_live_per_handle
    ON pending_signup (handle)
    WHERE confirmed_at IS NULL AND cancelled_at IS NULL;

-- For the reaper's sweep.
CREATE INDEX pending_signup_expiry_idx ON pending_signup (expires_at)
    WHERE confirmed_at IS NULL AND cancelled_at IS NULL;

-- ===========================================================================
-- What is deliberately not here: the slug is not reserved
-- ===========================================================================
--
-- A unique index on `slug` would hold the name for whoever asked first, which
-- reads like the kinder behaviour and is not. It would make squatting free:
-- one throwaway address per name, held for the whole expiry window, against a
-- table nobody is watching.
--
-- So the slug is checked when it is requested — which catches every ordinary
-- collision, because two people picking `acme` within a day is rare — and
-- checked again when it is confirmed, where the loser gets `slug_taken` and can
-- pick another. First to *confirm* wins, and confirming costs a mailbox.
