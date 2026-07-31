CREATE TABLE projector_checkpoints (
    projector TEXT NOT NULL PRIMARY KEY,
    global_position BIGINT NOT NULL
);

CREATE TABLE processed_events (
    projector TEXT NOT NULL,
    global_position BIGINT NOT NULL,
    processed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (projector, global_position)
);

CREATE TABLE IF NOT EXISTS retry_attempts (
    projector TEXT NOT NULL,
    global_position BIGINT NOT NULL,
    attempt_count INT NOT NULL DEFAULT 0,
    last_error TEXT,
    first_attempted_at TIMESTAMPTZ NOT NULL DEFAULT now(), -- set once on insert, NEVER updated
    last_attempted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (projector, global_position)
);

CREATE TABLE IF NOT EXISTS projector_dead_letters (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    projector TEXT NOT NULL,
    global_position BIGINT NOT NULL,
    aggregate_domain TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    event_name TEXT NOT NULL,
    payload JSONB NOT NULL,
    error TEXT NOT NULL,
    attempt_count INT NOT NULL,
    first_failed_at TIMESTAMPTZ NOT NULL,
    quarantined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ,
    resolution_notes TEXT
);

CREATE INDEX IF NOT EXISTS idx_dead_letters_unresolved ON projector_dead_letters (projector) WHERE resolved_at IS NULL;
