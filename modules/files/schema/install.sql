-- What is attached to what.
--
-- Schema-relative, like every module's install: every name here is unqualified,
-- so the same file installs into `proj_files` during provisioning and into a
-- staging schema during `rebuild_swap`.
--
-- **Derived entirely from the log** (L2). The bytes are not here and never
-- were — they are in whichever engine the tenant configured — and everything
-- below is what the events said about them. A rebuild reproduces this table
-- exactly and touches no file.

CREATE TABLE IF NOT EXISTS file (
    -- The caller's own id, which is also the idempotency key for the upload.
    id            TEXT PRIMARY KEY,

    -- What it is called, as whoever uploaded it named it. For a person to
    -- recognise and for `Content-Disposition` to suggest; **never** part of the
    -- key, because a key is generated and a name is typed.
    name          TEXT NOT NULL,

    -- **The owner, as an opaque pair.** No foreign key and no join: the thing
    -- on the other end lives in another projection group and L3 forbids
    -- reaching into it. What this answers is "what is attached to invoice
    -- INV-1", which needs no join at all.
    owner_kind    TEXT NOT NULL,
    owner_id      TEXT NOT NULL,

    -- Where the bytes are. **An engine and a key, never a URL** — a URL is
    -- where a file is today and a key is what it is, and a tenant who moves
    -- from disk to object storage has not changed any of their documents.
    engine        TEXT NOT NULL,
    storage_key   TEXT NOT NULL,
    -- SHA-256, hex. Checked on every read; a mismatch is a failure.
    checksum      TEXT NOT NULL,
    size          BIGINT NOT NULL CHECK (size >= 0),
    -- As the uploader declared it. Not sniffed — see `erp_storage::Stored`.
    media_type    TEXT NOT NULL,

    stored_at     TIMESTAMPTZ NOT NULL,
    -- Detached, not deleted. A document that was on an invoice is part of what
    -- happened, and the row saying it was removed is the record of that.
    removed_at    TIMESTAMPTZ,
    removed_why   TEXT,

    recorded_at   TIMESTAMPTZ NOT NULL,
    position      BIGINT NOT NULL
);

-- "What is attached to this", which is the only listing anybody asks for.
CREATE INDEX IF NOT EXISTS file_by_owner_idx
    ON file (owner_kind, owner_id, stored_at DESC) WHERE removed_at IS NULL;
