-- **What was said, to whom, and about what.**
--
-- Write-side state, beside the meter and the device tokens, and here for the
-- same reason both of those are: a send is an **effect promise, not an event**,
-- so nothing about it is derivable from the log — and a rebuild must not
-- destroy it. `messaging` keeps no projections and this does not change that.
--
-- It exists because a reply arrives as a number and a body and nothing else.
-- Correlating one to the booking it answers is impossible without a record of
-- what went to that number, and `messaging::send` kept none: it resolved,
-- rendered, metered, promised an effect, and forgot.
CREATE TABLE IF NOT EXISTS message_sent (
    -- **The outbox key**, primary. A promise the outbox deduplicated is
    -- recorded once here too, which is what stops a five-minute reminder job
    -- writing a row per tick.
    key           TEXT PRIMARY KEY,

    channel       TEXT NOT NULL,

    -- The number or address it went to, exactly as it was resolved. Exactly,
    -- because that is what a gateway hands back on a reply and matching it any
    -- more cleverly than character-for-character puts one person's answer in
    -- another person's conversation.
    addressed_to  TEXT NOT NULL,

    -- What it was about, when the sender knew. **Null is ordinary**: a one-time
    -- code, a signup email and an invitation are about nothing in this sense,
    -- and a reply to one correlates to no subject.
    topic         TEXT,
    subject_id    TEXT,

    -- The sender's instant, never `now()` (architecture L2).
    sent_at       TIMESTAMPTZ NOT NULL
);

-- The correlation query, and the only one: what was last said to this address,
-- as of an instant.
CREATE INDEX IF NOT EXISTS message_sent_by_address_idx
    ON message_sent (addressed_to, sent_at DESC);
