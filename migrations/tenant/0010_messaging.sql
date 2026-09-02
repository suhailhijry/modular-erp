-- What has been sent, and where a push notification would go.
--
-- # Why these are core tables rather than a module's read models
--
-- The same argument `0005_numbering.sql`, `0007_occupancy.sql` and
-- `0009_short_links.sql` make: **a read model can be rebuilt; money already
-- spent cannot be un-spent, and a device token a customer registered is not
-- derivable from anything in the log.**
--
-- `message_meter` is write-side state consulted inside the transaction that
-- adds to it — the same category as an occupancy claim. A budget enforced
-- against a projection would be a budget enforced against a number that is a
-- second out of date, which for a per-segment bill is money.
--
-- `push_token` is worse than that: a token arrives from a device, is not in
-- anybody's event log, and rebuilding a projection would destroy it. It belongs
-- where `rebuild_schema` cannot reach.

-- What was sent this month, per channel.
--
-- One row per (period, channel), created on the first message of the month.
CREATE TABLE IF NOT EXISTS message_meter (
    -- `YYYY-MM`, so a budget is monthly and a total sorts as text. Taken from
    -- the caller's instant, never a clock reading — a reminder for a booking on
    -- the 1st that was enqueued on the 31st is metered where it was sent.
    period      TEXT NOT NULL CHECK (period ~ '^[0-9]{4}-[0-9]{2}$'),
    channel     TEXT NOT NULL CHECK (channel IN ('email', 'sms', 'push', 'whatsapp')),

    messages    INTEGER NOT NULL DEFAULT 0 CHECK (messages >= 0),
    -- **What is actually billed.** An SMS is billed per 160-character segment,
    -- or per 70 in Arabic — so a message that silently becomes three costs
    -- three times, and counting messages would understate the bill by a factor
    -- nobody would predict.
    --
    -- Equal to `messages` on every channel billed per message.
    segments    INTEGER NOT NULL DEFAULT 0 CHECK (segments >= 0),

    first_at    TIMESTAMPTZ NOT NULL,
    last_at     TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (period, channel)
);

-- Where a push notification goes.
--
-- A device, not a person: one customer has a phone and a tablet, and a message
-- goes to both. The `recipient` is whoever the device belongs to, in whatever
-- id space the module that registered it uses — this table has no opinion.
CREATE TABLE IF NOT EXISTS push_token (
    -- The token the platform issued. Opaque, long, and the natural key: the
    -- same device re-registering must update rather than duplicate.
    token         TEXT PRIMARY KEY CHECK (length(token) BETWEEN 1 AND 512),

    recipient     TEXT NOT NULL CHECK (length(recipient) BETWEEN 1 AND 128),
    platform      TEXT NOT NULL CHECK (platform IN ('apns', 'fcm', 'web')),

    registered_at TIMESTAMPTZ NOT NULL,
    -- **Push tokens expire**, and the platform is the only thing that knows.
    -- It says so by rejecting a send, which is what sets this — so cleaning
    -- them up is scheduled work over this column rather than a guess about age.
    retired_at    TIMESTAMPTZ,
    retired_why   TEXT
);

CREATE INDEX IF NOT EXISTS push_token_recipient_idx
    ON push_token (recipient) WHERE retired_at IS NULL;

-- "What is there to clean up", which is the sweep's whole query.
CREATE INDEX IF NOT EXISTS push_token_retired_idx
    ON push_token (retired_at) WHERE retired_at IS NOT NULL;
