-- What somebody else's system told us, and that we already heard it.
--
-- # Why this is write-side state and not a projection
--
-- It is the **dedupe record**, consulted inside the transaction that accepts a
-- callback. A webhook is a command with the provider's id as its idempotency
-- key, and the thing that makes "arriving twice does nothing twice" true is
-- this table's primary key — checked and written together.
--
-- A projection is a second behind by design. A dedupe check against a number
-- that is a second out of date is not a dedupe check, and for a payment
-- confirmation it is money. Same category as `occupancy_claim`, and it lives in
-- the migration chain for the same reason: `rebuild_swap` must never come near
-- it.
--
-- # Why the payload is kept
--
-- Because "a provider says it sent us something we never processed" is the
-- question after an outage, and it is unanswerable from a row that records only
-- that an id was seen. It is also what a replay would need if the handler that
-- was meant to process it was not deployed yet.

CREATE TABLE IF NOT EXISTS webhook_event (
    -- Whose callback. A slug the tenant configured a secret under.
    provider     TEXT NOT NULL CHECK (provider ~ '^[a-z][a-z0-9_]{0,39}$'),
    -- **The provider's own id for the event.** Not ours: theirs is what is
    -- stable across their retries, which is the whole mechanism.
    event_id     TEXT NOT NULL CHECK (length(event_id) BETWEEN 1 AND 200),

    -- What they called it, when they said. Null when the payload does not carry
    -- one, which is a provider we can still deduplicate.
    kind         TEXT,
    payload      JSONB NOT NULL,

    received_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- **The count of deliveries, not a boolean.** A provider that sent the same
    -- event nine times is a provider whose retries are not being acknowledged,
    -- and that is worth being able to see.
    deliveries   INTEGER NOT NULL DEFAULT 1 CHECK (deliveries > 0),

    PRIMARY KEY (provider, event_id)
);

-- "What arrived recently", which is the listing and the outage question.
CREATE INDEX IF NOT EXISTS webhook_event_recent_idx
    ON webhook_event (provider, received_at DESC);
