CREATE TABLE snapshots (
    id BIGSERIAL PRIMARY KEY,
    aggregate_domain TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    version BIGINT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (aggregate_domain, version)
);

CREATE INDEX idx_snapshots_aggregate ON snapshots (aggregate_domain, aggregate_id, version);
CREATE INDEX idx_snapshots_created_at ON snapshots (created_at);
