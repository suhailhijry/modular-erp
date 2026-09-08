-- A thread against a subject, and every line in it.
--
-- Schema-relative, like every module's install: every name here is unqualified,
-- so the same file installs into `proj_conversations` during provisioning and
-- into a staging schema during `rebuild_swap`.
--
-- **Derived entirely from the log** (L2). What was said, what was noted and what
-- came back are events; this is what a screen reads.
--
-- # Why no column says what a thread is about
--
-- Because the id already does. A thread's id is derived from its subject, so
-- the route that knows it is looking at booking `BK-1` computes the same id the
-- writer did — and a column repeating it would be a second answer to a question
-- that already has one, kept in step by nothing.

-- One line in one thread: a note, something said, or something heard.
CREATE TABLE IF NOT EXISTS conversation_message (
    -- The thread aggregate, derived from what the thread is about.
    thread       TEXT NOT NULL,

    -- **The log position**, which is also the order. Two people writing at once
    -- get the order the log gave them, and a rebuild reproduces it exactly —
    -- a per-thread counter would have to be derived and could disagree.
    position     BIGINT NOT NULL,

    --   note   internal, and it never leaves
    --   said   sent to the customer, on `channel`, to `address`
    --   heard  they answered
    kind         TEXT NOT NULL CHECK (kind IN ('note', 'said', 'heard')),
    body         TEXT NOT NULL,

    -- Set on `said` and `heard`. Null on a note, which goes nowhere.
    channel      TEXT,
    address      TEXT,

    -- **Who wrote it.** The identity from the event's metadata for a note or
    -- something said; null for something heard, whose sender is `address`.
    who          TEXT,

    -- The writer's instant, never `now()` (architecture L2).
    said_at      TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (thread, position)
);

-- One thread, as a list of them shows it.
--
-- **Not derivable by grouping the messages**: the tray needs the address a
-- thread is with and where it was sent once somebody said. Both are facts about
-- the thread rather than about any one line.
CREATE TABLE IF NOT EXISTS conversation_thread (
    thread       TEXT PRIMARY KEY,

    -- Set only on a tray thread: the number nobody could be matched to.
    address      TEXT,
    -- Which thread its messages were moved onto, once somebody said what they
    -- were about. The row stays, so a number assigned last week is still
    -- findable.
    assigned_to  TEXT,

    messages     INTEGER NOT NULL DEFAULT 0,
    last_at      TIMESTAMPTZ NOT NULL,
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL
);

-- The tray: threads with a number and nobody attached, newest first. Partial,
-- because it is the exception and should stay small — a business whose tray is
-- large has a customer list missing everybody's number.
CREATE INDEX IF NOT EXISTS conversation_thread_unmatched_idx
    ON conversation_thread (last_at DESC)
    WHERE address IS NOT NULL AND assigned_to IS NULL;
