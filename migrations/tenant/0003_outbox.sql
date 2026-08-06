-- The outbox: effects a command promised to perform.
--
-- ===========================================================================
-- D9: effects are values, never inline I/O
-- ===========================================================================
--
-- A command handler that sends an email inline has two failure modes with no
-- good answer. Send before commit and a rolled-back transaction has already
-- mailed the customer. Send after commit and a crash in between loses the send
-- with nothing recording that it was owed.
--
-- Writing the *intention* as a row in the same transaction as the events removes
-- both. After commit, either the events and the promise are both durable or
-- neither is. A dispatcher picks the promise up afterwards and retries until it
-- is kept — which turns "did the email go out?" from an unanswerable question
-- into a row with a delivery timestamp on it.
--
-- ===========================================================================
-- Why effects are written by commands, not derived by projections
-- ===========================================================================
--
-- A projection could derive effects from the event stream, and it would get
-- exactly-once for free from L4. It is still wrong: projections are rebuildable
-- by design, and a rebuild would re-derive every effect and re-send years of
-- email. Making the outbox a command-time artifact means a rebuild sends
-- nothing, which is what makes rebuilds safe to run at all.
--
-- It also matches L5. An effect records a *resolved decision* — this address,
-- this template, this amount — taken with the configuration in force at the
-- time. Re-deriving it later would resolve against today's configuration.

CREATE TABLE outbox (
    -- An ordinary identity column, unlike `event.position`.
    --
    -- The L1 argument does not apply here: nothing tails the outbox by id. Rows
    -- are *claimed* by lease, so a gap left by a rolled-back transaction is
    -- invisible to every reader. Paying for gaplessness would mean serializing
    -- every command behind one more row lock for a property no one reads.
    id              BIGINT PRIMARY KEY GENERATED ALWAYS AS IDENTITY,

    -- What makes delivery safe to retry.
    --
    -- Derived from the causing log position when the caller does not pin one, so
    -- it is unique without coordination. A caller may pin it instead to dedup
    -- across command executions — the same intention enqueued twice becomes one
    -- row, because the insert is `ON CONFLICT DO NOTHING`.
    idempotency_key TEXT NOT NULL UNIQUE
                    CHECK (length(idempotency_key) BETWEEN 1 AND 200),

    -- Which handler delivers this. Same character set as `event.event_name`.
    kind            TEXT NOT NULL CHECK (kind ~ '^[A-Za-z0-9_.-]{1,64}$'),
    payload         JSONB NOT NULL,

    -- The log position of the command that promised this, for tracing an effect
    -- back to its cause. Deliberately not a foreign key: the event log is
    -- append-only and this column is diagnostic, so a constraint would add write
    -- cost to buy a guarantee the trigger already provides.
    caused_by       BIGINT CHECK (caused_by >= 1),

    enqueued_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ---------------------------------------------------------------------
    -- Delivery state
    -- ---------------------------------------------------------------------

    -- Incremented when a row is *claimed*, not when delivery fails.
    --
    -- That is deliberate. An effect whose handler crashes the worker would
    -- otherwise be re-claimed forever by every replacement process, and each
    -- crash would look like a fresh start. Counting the claim means a poisonous
    -- effect dead-letters instead of taking the fleet down with it.
    attempts        INT NOT NULL DEFAULT 0 CHECK (attempts >= 0),

    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Held while a dispatcher is delivering. Expiry, not release, is what makes
    -- this crash-safe: a dispatcher that dies mid-delivery leaves the lease
    -- behind, and the row becomes claimable again when it lapses.
    leased_until    TIMESTAMPTZ,

    delivered_at    TIMESTAMPTZ,

    -- Set when the effect is given up on: attempts exhausted, or a failure the
    -- handler reported as permanent. Never deleted — a dead letter is evidence,
    -- and `unresolved dead letters = 0` is a per-tenant health assertion
    -- (architecture §7).
    dead_at         TIMESTAMPTZ,
    last_error      TEXT,

    CONSTRAINT outbox_is_not_both_delivered_and_dead
        CHECK (delivered_at IS NULL OR dead_at IS NULL)
);

-- The claim query: due, undelivered, unleased, oldest first.
--
-- Partial, so it holds only rows that are still owed. A tenant with ten million
-- delivered effects and four pending ones has a four-row index.
CREATE INDEX outbox_due_idx ON outbox (next_attempt_at, id)
    WHERE delivered_at IS NULL AND dead_at IS NULL;

-- Dead letters, for the health check. Also partial: normally empty.
CREATE INDEX outbox_dead_idx ON outbox (dead_at)
    WHERE dead_at IS NOT NULL;
