-- Which branches a member belongs to.
--
-- `X-Branch` was a header the caller wrote: checked for shape only, and
-- branch-scoped claims, inventory shelves and every posting trusted it. Nothing
-- recorded which branches a person belonged to, so nothing could refuse one
-- they did not. Decided 2026-09-14 by the product owner: a list per
-- membership, beside the per-module roles and for the same reason — it is
-- access, not an org-chart fact (`hr`'s employee has a "where they work",
-- which is a different thing).
--
-- No rows means every branch, which is what every membership had before this
-- table existed. A row names a branch the way `membership_module_role` names a
-- module: as the tenant's own identifier, not a foreign key — the branch lives
-- in the tenant's database, and a self-hosted deployment runs this table beside
-- its tenant on its own control plane.
CREATE TABLE membership_branch (
    membership_id UUID NOT NULL REFERENCES membership (id) ON DELETE CASCADE,
    branch        TEXT NOT NULL CHECK (length(branch) BETWEEN 1 AND 128),
    set_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (membership_id, branch)
);

COMMENT ON TABLE membership_branch IS
    'The branches a member may act in and read. No rows: every branch.';
