-- Projection checkpoints.
--
-- ===========================================================================
-- L4: a checkpoint advances in the same transaction as the effects it records
-- ===========================================================================
--
-- The tempting shape is: apply a batch, then record how far you got. It is wrong
-- in both orderings. Record first and a crash loses the events. Apply first and
-- a crash reapplies them — fine for an idempotent projection, silently wrong for
-- one that increments a total.
--
-- Committing both together removes the window entirely: after a crash, the
-- checkpoint names exactly the events whose effects survived, so recovery
-- replays precisely what was lost. There is no dedup table and no second
-- position space, because there is nothing to reconcile.
--
-- ===========================================================================
-- The lease
-- ===========================================================================
--
-- `SELECT ... FOR UPDATE` on this row is also what stops two workers processing
-- one group at once. It costs nothing extra — the row has to be read anyway —
-- and it means the mutual exclusion cannot drift out of step with the
-- checkpoint, the way a separate lock table would.

CREATE TABLE projection_checkpoint (
    -- Matches `ProjectionGroup::NAME`.
    group_name TEXT PRIMARY KEY CHECK (group_name ~ '^[a-z][a-z0-9_]{0,62}$'),

    -- Last position whose effects are committed. Zero means nothing yet.
    position   BIGINT NOT NULL DEFAULT 0 CHECK (position >= 0),

    -- For lag monitoring (architecture §7). Never read by the runner.
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- L3: a group owns a Postgres schema, and may not read outside it.
--
-- "Reproducible across related tables" is what forces grouping. If two tables
-- must agree, they cannot be independently checkpointed projections that replay
-- at different rates — mid-replay they would disagree, and any query touching
-- both would see a state that never existed.
--
-- Within a group: one checkpoint, one transaction. Across groups: no reads. That
-- second half is enforced rather than reviewed — the runner sets `search_path`
-- to the group's own schema for the duration of its transaction, so a query
-- naming another group's table fails with "relation does not exist" the first
-- time it runs. Reaching across then requires a schema-qualified name, which is
-- greppable and bannable.
--
-- Schemas are created by `ensure_group_schema` when a module is enabled, not
-- here: which groups exist depends on which modules a tenant has.
-- ---------------------------------------------------------------------------
