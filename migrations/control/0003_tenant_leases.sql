-- Which worker is responsible for which tenant's background work.
--
-- ===========================================================================
-- The problem this solves
-- ===========================================================================
--
-- Projections and the outbox need someone to drive them. With one worker that
-- is trivial; with a fleet it is a coordination problem, and the naive answers
-- are both wrong:
--
--   * Every worker services every tenant. Two workers processing one group is
--     already refused by the checkpoint lock (L4), so it is *safe* — but each
--     worker opens a connection to each tenant to find that out. Connections
--     scale as workers × tenants, and the measured sizing rule is
--     `connections ≈ active_tenants × per_tenant_pool`, so this makes every
--     tenant permanently active. It is the one thing that must not happen.
--
--   * Static assignment by hash. Rebalancing on a deploy means either a window
--     where nobody owns a shard, or one where two do.
--
-- A lease is the standard answer and costs one table. A worker claims tenants
-- it does not already hold, renews while it works, and a worker that dies stops
-- renewing — so its tenants become claimable when the lease lapses, with no
-- membership protocol and no leader.
--
-- ===========================================================================
-- Why `next_visit_at` matters more than the lease
-- ===========================================================================
--
-- A leased tenant is not a *busy* tenant. Most tenants are idle most of the
-- time, and visiting an idle tenant costs a connection to learn nothing.
--
-- So each tenant carries when it is next worth visiting, and the worker pushes
-- that out as it finds nothing to do — hundreds of milliseconds while a tenant
-- is active, tens of seconds once it goes quiet. An idle tenant then costs one
-- query per interval and holds no connection between them, because per-tenant
-- pools have `min = 0` and a short idle timeout.
--
-- This is the column the eventual push path replaces: when the API can tell a
-- worker directly that a tenant just wrote something, it does so by moving
-- `next_visit_at` to now. Polling becomes the floor rather than the mechanism,
-- and nothing else has to change.

ALTER TABLE tenant
    -- Which worker holds this tenant. Free-form: a hostname, a pod name, an
    -- ordinal. Only ever compared for equality and shown in diagnostics.
    ADD COLUMN worker_lease_owner TEXT
        CHECK (worker_lease_owner IS NULL OR length(worker_lease_owner) BETWEEN 1 AND 128),

    -- When the lease lapses. Expiry rather than release is what makes a crashed
    -- worker recoverable without anyone noticing it crashed.
    ADD COLUMN worker_lease_until  TIMESTAMPTZ,

    -- When this tenant is next worth a look. Moved forward by the worker as it
    -- finds nothing to do, and moved back to `now()` by anything that knows
    -- there is work.
    ADD COLUMN next_visit_at       TIMESTAMPTZ NOT NULL DEFAULT now();

-- The claim query: active tenants, unleased or lapsed, due for a visit.
--
-- Partial on status, so suspended and deleted tenants are not in the index at
-- all rather than being filtered out of it.
CREATE INDEX tenant_claimable_idx
    ON tenant (next_visit_at, worker_lease_until)
    WHERE status = 'active';

-- Renewing and releasing, both keyed by owner.
CREATE INDEX tenant_lease_owner_idx ON tenant (worker_lease_owner)
    WHERE worker_lease_owner IS NOT NULL;
