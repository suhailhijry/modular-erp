-- Invitations: giving someone access without knowing their password.
--
-- Before this, adding a colleague meant the owner choosing a password and
-- handing it over — a password the owner then knows forever. An invitation
-- inverts that: the recipient sets their own, and the owner never sees it.

CREATE TABLE invitation (
    id          UUID PRIMARY KEY,
    tenant_id   UUID NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,

    -- The token itself is never stored, only its SHA-256 — the same reasoning
    -- as `session`. A leaked database dump must not contain working invitation
    -- links, and 256 bits of entropy needs no slow hash.
    token_hash  BYTEA NOT NULL UNIQUE CHECK (length(token_hash) = 32),

    -- Who it is for. Acceptance always binds to *this* address, never to
    -- whoever happens to be holding the link, so a forwarded invitation cannot
    -- quietly become somebody else's account.
    handle      TEXT NOT NULL CHECK (length(handle) BETWEEN 3 AND 320),
    role        TEXT NOT NULL CHECK (length(role) BETWEEN 1 AND 64),

    -- Nullable: an invitation outlives the person who sent it, and losing the
    -- attribution is better than losing the invitation.
    invited_by  UUID REFERENCES identity (id) ON DELETE SET NULL,

    expires_at  TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    accepted_by UUID REFERENCES identity (id) ON DELETE SET NULL,
    revoked_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Attribution implies acceptance, but not the reverse.
    --
    -- Accepting claims the row *before* the identity exists — that claim is what
    -- stops two people opening the same link from both getting in, and it is one
    -- statement precisely so it cannot race. `accepted_by` is filled in a moment
    -- later. Requiring both together would forbid the state the claim creates.
    CONSTRAINT invitation_attribution_implies_acceptance CHECK (
        accepted_by IS NULL OR accepted_at IS NOT NULL
    )
);

-- At most one outstanding invitation per address per tenant. Re-inviting
-- revokes the previous one rather than leaving two live links, so revoking an
-- invitation actually revokes access rather than one of several ways in.
CREATE UNIQUE INDEX invitation_one_outstanding_per_handle
    ON invitation (tenant_id, handle)
    WHERE accepted_at IS NULL AND revoked_at IS NULL;

CREATE INDEX invitation_by_tenant_idx ON invitation (tenant_id)
    WHERE accepted_at IS NULL AND revoked_at IS NULL;
