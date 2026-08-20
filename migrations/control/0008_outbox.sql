-- The control plane's outbox.
--
-- ===========================================================================
-- Why the control plane needs one of its own
-- ===========================================================================
--
-- The outbox was built in the tenant database, because that is where commands
-- and their events are. But the things that most need to reach the outside
-- world do not happen in a tenant database at all: an **invitation** is a
-- control-plane row, and so is a signup, and so is a password reset.
--
-- Writing an invitation here and its email into the tenant's outbox would be two
-- databases, therefore two transactions, therefore a window where the invitation
-- exists and the email was never promised — and nothing would ever notice,
-- because there is no row saying it was owed. That window is the exact thing
-- D9 exists to close, so closing it in one plane and leaving it open in the
-- other is not a design, it is an oversight with a comment on it.
--
-- ===========================================================================
-- Why it is byte-for-byte the tenant's table
-- ===========================================================================
--
-- `erp_eventlog`'s `Dispatcher` and `enqueue` are compile-time-checked against a
-- table named `outbox` with these columns. Reusing them here — claim under
-- `SKIP LOCKED`, lease, backoff, dead letters, the at-least-once idempotency
-- key, and every crash test already written against them — costs exactly one
-- thing: this file must not drift from `migrations/tenant/0003_outbox.sql`.
--
-- `the_two_outboxes_are_the_same_table` in `crates/erp-control/tests/outbox.rs`
-- is what stops it drifting, by comparing the two schemas column by column.
-- Without that, a column added to one would fail against the other at runtime,
-- in whichever plane was touched second.
--
-- `IF NOT EXISTS` because `just prepare` loads both migration chains into one
-- type-check database, where these are the same table and only the first one
-- run creates it. The recipe runs the tenant chain first so that the *tenant*
-- definition is the one sqlx validates against, and this file's job there is to
-- be a no-op.
--
-- `caused_by` is kept and always NULL here: there is no event log in the control
-- plane to point at. Eight bytes of nothing, in exchange for every line of the
-- dispatcher.

CREATE TABLE IF NOT EXISTS outbox (
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
CREATE INDEX IF NOT EXISTS outbox_due_idx ON outbox (next_attempt_at, id)
    WHERE delivered_at IS NULL AND dead_at IS NULL;

-- Dead letters, for the health check. Also partial: normally empty.
CREATE INDEX IF NOT EXISTS outbox_dead_idx ON outbox (dead_at)
    WHERE dead_at IS NOT NULL;
