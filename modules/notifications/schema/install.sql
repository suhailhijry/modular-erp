-- The bell, and what each person wants in it.
--
-- Schema-relative, like every module's install: every name here is unqualified,
-- so the same file installs into `proj_notifications` during provisioning and
-- into a staging schema during `rebuild_swap`.
--
-- **Derived entirely from the log** (L2), read state included. "Sara has seen
-- this and Ahmed has not" is a projection of `notifications.notification.read`,
-- never a flag somebody set — which is the whole reason a rebuild can be run
-- without asking anybody to mark their inbox again.

-- One person's copy of one notification.
--
-- **A row per recipient, not a set of readers on one row.** The question this
-- table exists to answer is "what is unread, for me" — one index, one `WHERE`,
-- no join — and a shared row with an array of readers answers it only by
-- scanning everybody's.
CREATE TABLE IF NOT EXISTS inbox (
    -- The notification aggregate. Derived from the kind and the subject, so the
    -- same thing announced twice is the same id and the second one writes
    -- nothing.
    notification TEXT NOT NULL,

    -- **The login this belongs to**, from `hr.employee.identity`. Every read of
    -- this table is `WHERE recipient = $1`: somebody else's notification is not
    -- hidden by the handler, it is not selected.
    recipient    TEXT NOT NULL,

    kind         TEXT NOT NULL,

    -- What it is about, as an opaque pair. No foreign key and no join: the
    -- reservation or invoice on the other end lives in another projection group
    -- and L3 forbids reaching into it. A client that wants the booking asks for
    -- the booking.
    topic        TEXT NOT NULL,
    subject_id   TEXT NOT NULL,

    -- `{"en": {"title": …, "body": …}, "ar": {…}}`.
    --
    -- **Rendered when it was announced, in both languages.** In both, so the
    -- bell answers `Accept-Language` like every other response; at announce
    -- time, because the bindings it names — the customer's name, when the
    -- booking starts — have to be read from the read models with a connection,
    -- and rendering per row on every list would be that read multiplied by the
    -- page size.
    wording      JSONB NOT NULL,

    announced_at TIMESTAMPTZ NOT NULL,
    -- Null until this person has seen it. Set by their own `read`, or by their
    -- `read_all`.
    read_at      TIMESTAMPTZ,

    -- The event's own timestamp, never `now()` (architecture L2).
    recorded_at  TIMESTAMPTZ NOT NULL,
    position     BIGINT NOT NULL,

    PRIMARY KEY (notification, recipient)
);

-- The bell itself: this person's, newest first.
CREATE INDEX IF NOT EXISTS inbox_by_recipient_idx
    ON inbox (recipient, announced_at DESC, notification);

-- The count on the badge. Partial, because the interesting set is small and
-- shrinks every time somebody looks at it.
CREATE INDEX IF NOT EXISTS inbox_unread_idx
    ON inbox (recipient) WHERE read_at IS NULL;

-- What an announcer asks before it announces: "have I already said this?" The
-- aggregate is the authority — it refuses a repeat outright — and this is what
-- keeps a scan from opening a transaction per row to find that out.
CREATE INDEX IF NOT EXISTS inbox_by_subject_idx ON inbox (kind, subject_id);

-- What one person wants told to them, for one kind.
--
-- **The channels are an array rather than a row per channel** because a row is
-- a statement and its absence is the default: "nothing at all for this kind"
-- has to be distinguishable from "never said", and an empty set of rows cannot
-- say which it is.
CREATE TABLE IF NOT EXISTS preference (
    identity    TEXT NOT NULL,
    kind        TEXT NOT NULL,
    channels    TEXT[] NOT NULL,

    recorded_at TIMESTAMPTZ NOT NULL,
    position    BIGINT NOT NULL,

    PRIMARY KEY (identity, kind)
);
