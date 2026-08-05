-- The tenant event log.
--
-- One of these exists per tenant database, so nothing here carries a tenant id:
-- the database *is* the tenant. That also simplifies architecture law L1 — the
-- serialization it calls for needs no per-tenant key, because there is nothing
-- else in this database to serialize against.
--
-- ===========================================================================
-- L1: positions are gapless and commit-ordered
-- ===========================================================================
--
-- The obvious implementation is wrong in a way that is invisible under light
-- load. `BIGINT GENERATED ALWAYS AS IDENTITY` assigns at INSERT time, not at
-- COMMIT time, so:
--
--   T1 inserts, takes position 100
--   T2 inserts, takes position 101
--   T2 commits
--   -- a tailer reading `position > 99` now sees 101 and advances past it --
--   T1 commits
--
-- Event 100 is never delivered. Rare when writes are sparse, routine under
-- contention, and it corrupts replay silently: the live projection and a replay
-- observe different sets of events.
--
-- The fix is the counter row below. `UPDATE ... RETURNING` takes a row lock, so
-- a second appender cannot obtain a position until the first has committed —
-- which makes position order equal commit order. And because the counter is
-- ordinary transactional data rather than a sequence, a rollback *returns* the
-- number instead of burning it, so positions are gapless as well as ordered.
--
-- A plain sequence plus an advisory lock would also give commit ordering, but
-- would still leave holes wherever a transaction rolled back — and then the
-- contiguity check could only ever be a warning, not an integrity assertion.
--
-- The cost is real and bounded: appends within one tenant serialize. The lock is
-- held from the append until the caller's transaction ends, so **append last**.
-- If a single tenant ever outgrows this, the escape hatch is snapshot-visibility
-- tracking (`pg_snapshot_xmin`) rather than a bigger lock — but that trades this
-- file's one simple invariant for reasoning about transaction visibility at
-- every read site, so it is not a trade to make early.

CREATE TABLE event_log_position (
    -- Single row, enforced by the primary key and the check together.
    id            BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    next_position BIGINT NOT NULL DEFAULT 1 CHECK (next_position >= 1)
);

INSERT INTO event_log_position (id) VALUES (true);

CREATE TABLE event (
    -- Gapless, commit-ordered. Assigned from the counter above, never a sequence.
    position       BIGINT PRIMARY KEY,

    -- Which aggregate this belongs to.
    stream_domain  TEXT NOT NULL CHECK (stream_domain ~ '^[A-Za-z0-9_.-]{1,64}$'),
    stream_id      TEXT NOT NULL CHECK (length(stream_id) BETWEEN 1 AND 128),
    -- The aggregate's version after this event. A DIFFERENT quantity from
    -- `position`, which is why they are different types in Rust: conflating
    -- them is the defect that motivated the newtypes in `spa-types`.
    sequence       BIGINT NOT NULL CHECK (sequence >= 1),

    event_name     TEXT NOT NULL CHECK (event_name ~ '^[A-Za-z0-9_.-]{1,96}$'),
    -- Drives the upcaster chain. Stored bytes never change; only the
    -- interpretation moves forward.
    schema_version SMALLINT NOT NULL CHECK (schema_version >= 1),

    payload        JSONB NOT NULL,
    metadata       JSONB NOT NULL DEFAULT '{}',

    recorded_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Optimistic concurrency. Two writers who both loaded an aggregate at
    -- version N will both try to write N+1; the database refuses the second.
    CONSTRAINT event_stream_sequence_is_unique
        UNIQUE (stream_domain, stream_id, sequence)
);

-- Loading one aggregate: its events in order.
CREATE INDEX event_by_stream_idx ON event (stream_domain, stream_id, sequence);

-- Tailing the log. The primary key already serves `position > $1 ORDER BY
-- position`, so no separate index is needed for it.

-- Answering "has anything happened since?" cheaply, for the scheduler deciding
-- which tenants have pending work.
CREATE INDEX event_recorded_at_idx ON event (recorded_at);

-- ---------------------------------------------------------------------------
-- Append-only enforcement.
--
-- A log that can be edited is not a log. This is the same posture as the
-- control plane's audit table: enforced by the database, not by convention,
-- because the code that would violate it is exactly the code written at 3am.
-- ---------------------------------------------------------------------------
CREATE FUNCTION event_is_append_only() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'the event log is append-only (attempted %)', TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER event_no_update_or_delete
    BEFORE UPDATE OR DELETE ON event
    FOR EACH ROW EXECUTE FUNCTION event_is_append_only();
