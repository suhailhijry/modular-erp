CREATE TABLE events (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    aggregate_domain TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    event_name TEXT NOT NULL,
    payload JSONB NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE(aggregate_domain, aggregate_id, sequence)
);

CREATE INDEX idx_events_aggregate ON events (aggregate_domain, aggregate_id, sequence);
CREATE INDEX idx_events_created_at ON events (created_at);
CREATE INDEX idx_events_name_time ON events (event_name, created_at);
