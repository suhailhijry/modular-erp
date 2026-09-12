-- **Which read model built a group's tables.**
--
-- `install.sql` is `IF NOT EXISTS` throughout, so enabling or re-enabling a
-- module never reshapes tables that already exist, and nothing recorded which
-- shape they were. A build that changed a read model could not tell a tenant
-- on the old shape from one on the new, and neither could the deploy step.
--
-- Matches `ProjectionGroup::VERSION`. Stamped by whatever builds the tables
-- (`ensure_group_schema`, provisioning, `rebuild_swap`'s swap), and read by the
-- projection runner, which refuses a group older than its build, by the
-- request path, which answers that module's routes 503, and by the migrator,
-- which rebuilds every group not at this build's version.
--
-- **0 means "built before this was recorded"**, and is below every real
-- version, so it counts as behind. Deliberately not 1: nothing knows whether an
-- existing tenant's tables match today's install script, and stamping them
-- current would be the silent fallback L6 forbids. The cost is one rebuild of
-- every group on every tenant at the first deploy, through the swap, with no
-- outage.
--
-- Expand-only: a column with a default. A pod from before this migration never
-- names it, so its inserts get 0 — the honest value for tables it built.
ALTER TABLE projection_checkpoint
    ADD COLUMN read_model_version SMALLINT NOT NULL DEFAULT 0
        CHECK (read_model_version >= 0);
