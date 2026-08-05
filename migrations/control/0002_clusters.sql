-- Clusters as data, so a new one can be brought online without a deploy.
--
-- The soak test settled what "capacity" means here. Open connections are bounded
-- by *concurrently-active tenants × per-tenant pool size*, not by the lane budget
-- and not by request rate — so the number that decides whether a cluster can take
-- another tenant is how many of its tenants are active at once, not how many it
-- holds. `max_active_tenants` is therefore the primary capacity limit, and
-- `max_databases` is the secondary, storage-shaped one.
--
-- Credentials are deliberately absent. The row names an environment variable
-- (`dsn_env`) holding the DSN; the process reads it at startup. Putting a
-- password in a table means it reaches every backup, every replica, and every
-- support engineer with read access to the control plane.

CREATE TABLE cluster (
    name           TEXT PRIMARY KEY
                   CHECK (name ~ '^[a-z][a-z0-9_-]{0,62}$'),

    -- Names of the environment variables holding the connection strings.
    dsn_env        TEXT NOT NULL CHECK (dsn_env ~ '^[A-Z][A-Z0-9_]{0,62}$'),
    replica_dsn_env TEXT CHECK (replica_dsn_env ~ '^[A-Z][A-Z0-9_]{0,62}$'),

    status         TEXT NOT NULL DEFAULT 'available'
                   CHECK (status IN ('available', 'draining', 'full', 'offline')),

    -- Capacity. See the note above on why the first one is the one that matters.
    max_active_tenants INT NOT NULL CHECK (max_active_tenants > 0),
    max_databases      INT NOT NULL CHECK (max_databases > 0),

    -- Placement preference among otherwise-equal clusters. Higher wins, so a
    -- new cluster can be filled ahead of older ones without editing limits.
    weight         INT NOT NULL DEFAULT 100,

    -- Free-form, for an operator: region, hardware, who to page.
    notes          TEXT,

    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Tenants already carry a cluster name. Making it a real foreign key means a
-- tenant cannot be placed on a cluster that does not exist, and a cluster
-- holding tenants cannot be deleted out from under them.
ALTER TABLE tenant
    ADD CONSTRAINT tenant_cluster_exists
    FOREIGN KEY (cluster) REFERENCES cluster (name)
    ON DELETE RESTRICT;

CREATE INDEX tenant_by_cluster_idx ON tenant (cluster) WHERE status <> 'deleted';

-- What a cluster is currently carrying.
--
-- `live_tenants` is what the placement policy reads. Activity is measured by the
-- caller (it is a runtime property, not a stored one), so this view answers the
-- storage-shaped question and the policy combines it with observed activity.
CREATE VIEW cluster_load AS
SELECT c.name,
       c.status,
       c.max_active_tenants,
       c.max_databases,
       c.weight,
       count(t.id) FILTER (WHERE t.status <> 'deleted')  AS live_tenants,
       count(t.id) FILTER (WHERE t.status = 'active')    AS active_tenants
  FROM cluster c
  LEFT JOIN tenant t ON t.cluster = c.name
 GROUP BY c.name, c.status, c.max_active_tenants, c.max_databases, c.weight;
