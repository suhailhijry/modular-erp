-- Getting back in after forgetting the password.
--
-- ===========================================================================
-- What this is, and what it deliberately is not
-- ===========================================================================
--
-- It is `pending_signup` with the password taken out. A signup has to carry a
-- hash because there is no account to check one against when the link comes
-- back; a reset has one, so the new password is collected **at redemption** and
-- nothing sensitive sits here in between. An operator reading a dump of this
-- table learns exactly one thing: who asked.
--
-- It is **not** a way to claim a handle. `0010_signups.sql` argues that writing
-- to `authenticator` is what takes an address, which is why a signup's password
-- waits for the mailbox. This flow never inserts there. It runs one
-- `UPDATE ... WHERE identity_id = $1 AND kind = 'password'`, so a reset can
-- rewrite a secret and can never create a login, move one, or point one at
-- somebody else. There is no state of this table from which an account exists
-- that did not before.
--
-- ===========================================================================
-- Why there is no "one live link per address"
-- ===========================================================================
--
-- `pending_signup` and `invitation` both hold one. Copying it here would have
-- been a **permanent denial of password recovery**, and cheap: an attacker
-- requests a reset for the victim once a minute, each request cancelling the
-- link the last one minted. The victim's mail arrives — greylisting alone
-- routinely costs fifteen minutes — and the link in it is already dead. They
-- can never complete a reset, from one address, inside every rate limit.
--
-- The law it would have been copied from does not transfer. The reason
-- `invitation_one_outstanding_per_handle` exists is written at
-- `0005_invitations.sql`: *revoking an invitation actually revokes access
-- rather than one of several ways in*. An invitation is revocable access. A
-- reset link is neither revoked by anyone nor access — it is a one-hour,
-- single-use permission to choose a password, and several of them outstanding
-- is several chances for the person who asked for them.
--
-- What bounds the rows instead: a cooldown per address, an hour of life, and
-- the sweep. Sixty per address per hour, worst case, all of them the victim's
-- own.

CREATE TABLE password_reset (
    id           UUID PRIMARY KEY,

    -- Never the token, only its SHA-256 — the reasoning `session`, `invitation`
    -- and `pending_signup` all give. 256 bits of entropy needs no slow hash;
    -- the point is that a leaked dump holds no working links.
    token_hash   BYTEA NOT NULL UNIQUE CHECK (length(token_hash) = 32),

    -- Who asked. Lowercased and trimmed by the caller, compared exactly.
    handle       TEXT NOT NULL CHECK (length(handle) BETWEEN 3 AND 320),

    -- **Pinned when the link is minted, not resolved when it is opened.**
    -- Resolving at redemption would let a link minted for an account that has
    -- since been erased reset whatever account next takes that address.
    identity_id  UUID NOT NULL REFERENCES identity (id) ON DELETE CASCADE,

    -- Wrong second-factor codes spent against this link. The token is 256 bits
    -- and unguessable; the six digits it gates are twenty, and twenty bits is
    -- where guessing actually goes. Same limiter and same argument as
    -- `one_time_code.attempts`.
    attempts     INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),

    expires_at   TIMESTAMPTZ NOT NULL,
    -- Single use, set in the statement that claims it, so two people opening
    -- the same link resolve to one.
    used_at      TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- What the cooldown reads: the most recent request for an address.
CREATE INDEX password_reset_handle_idx ON password_reset (handle, created_at DESC);

-- For the sweep, which does not care about the lifecycle columns: an hour later
-- a spent link and an unopened one are both rubbish.
CREATE INDEX password_reset_expiry_idx ON password_reset (expires_at);

COMMENT ON TABLE password_reset IS
    'One-hour, single-use permission to choose a new password. Rows exist only '
    'for addresses that have an account: an address with none is answered '
    'identically and nothing is written, because writing and mailing for any '
    'address anybody types is an open mail relay on the fleet''s sending domain.';
