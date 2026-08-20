-- Control plane: the core database.
--
-- Per architecture decision D2 these are normalized tables with an audit trail,
-- not an event stream. They are read on every request, are highly relational,
-- and must be queryable for cross-tenant reporting — all three of which an
-- event-sourced representation would make harder for no gain. Provisioning
-- workflows, which genuinely need resumable state, are event-sourced separately.
--
-- Note on enumerations: these are TEXT + CHECK rather than Postgres ENUM types.
-- `ALTER TYPE ... ADD VALUE` cannot run inside a transaction and values can
-- never be removed, which makes enums hostile to the expand/contract migration
-- discipline in architecture §4.11. A CHECK constraint is altered like anything
-- else.

-- ---------------------------------------------------------------------------
-- Identities: things that can authenticate.
--
-- Not people, not roles, not party records. A person with a login has one
-- identity; an employee record without a login has none. See ARCHITECTURE §1.9.
-- ---------------------------------------------------------------------------
CREATE TABLE identity (
    id               UUID PRIMARY KEY,
    status           TEXT NOT NULL DEFAULT 'active'
                     CHECK (status IN ('active', 'suspended')),
    suspended_reason TEXT,
    suspended_at     TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A suspended identity must say why and when; an active one must not carry
    -- stale suspension data. Keeping this in the schema means no code path can
    -- produce a half-suspended row.
    CONSTRAINT identity_suspension_is_complete CHECK (
        (status = 'active'    AND suspended_reason IS NULL AND suspended_at IS NULL) OR
        (status = 'suspended' AND suspended_reason IS NOT NULL AND suspended_at IS NOT NULL)
    )
);

-- ---------------------------------------------------------------------------
-- Tenants: one row per customer, one database each.
-- ---------------------------------------------------------------------------
CREATE TABLE tenant (
    id            UUID PRIMARY KEY,
    -- 2–50 characters. Two is deliberate: "hp", "3m" and "bp" are real company
    -- names, and a three-character floor would reject them. One character is
    -- still refused, since single letters are worth reserving.
    slug          TEXT NOT NULL UNIQUE
                  CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,48}[a-z0-9]$'),
    display_name  TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),

    status        TEXT NOT NULL DEFAULT 'provisioning'
                  CHECK (status IN ('provisioning', 'active', 'suspended', 'deleted')),

    -- Physical location. Recorded from day one even though there is one cluster
    -- today: promoting a tenant to dedicated hardware must be a row change, not
    -- a schema change (ARCHITECTURE §2).
    cluster       TEXT NOT NULL DEFAULT 'primary',
    database_name TEXT NOT NULL,

    -- Demo tenants are ordinary tenants with an expiry. A demo that converts
    -- becomes real by clearing this column.
    demo_expires_at TIMESTAMPTZ,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    activated_at  TIMESTAMPTZ,
    deleted_at    TIMESTAMPTZ,

    -- Two tenants pointing at one database would be a cross-tenant data leak of
    -- the worst kind. The database refuses.
    CONSTRAINT tenant_database_is_exclusive UNIQUE (cluster, database_name),

    -- Matches erp-testkit's identifier rules and Postgres's 63-byte limit, so a
    -- name that reaches CREATE DATABASE is always valid.
    CONSTRAINT tenant_database_name_is_a_safe_identifier
        CHECK (database_name ~ '^[a-z][a-z0-9_]{0,62}$')
);

CREATE INDEX tenant_status_idx ON tenant (status) WHERE status <> 'deleted';
CREATE INDEX tenant_demo_expiry_idx ON tenant (demo_expires_at)
    WHERE demo_expires_at IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Memberships: the right to ENTER a scope.
--
-- Not fine-grained permission — that lives in the tenant's own database, next
-- to the data it governs (ARCHITECTURE §2). This table answers only "may this
-- identity open this tenant at all", which is the question on the hot path.
-- ---------------------------------------------------------------------------
CREATE TABLE membership (
    id          UUID PRIMARY KEY,
    identity_id UUID NOT NULL REFERENCES identity (id) ON DELETE CASCADE,

    scope_kind  TEXT NOT NULL CHECK (scope_kind IN ('platform', 'tenant')),
    tenant_id   UUID REFERENCES tenant (id) ON DELETE CASCADE,

    role        TEXT NOT NULL CHECK (length(role) BETWEEN 1 AND 64),

    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ,

    CONSTRAINT membership_scope_matches_tenant CHECK (
        (scope_kind = 'platform' AND tenant_id IS NULL) OR
        (scope_kind = 'tenant'   AND tenant_id IS NOT NULL)
    ),

    -- NULLS NOT DISTINCT so an identity gets at most one platform membership,
    -- rather than one per insert (Postgres 15+).
    CONSTRAINT membership_is_unique_per_scope
        UNIQUE NULLS NOT DISTINCT (identity_id, tenant_id)
);

CREATE INDEX membership_by_identity_idx ON membership (identity_id)
    WHERE revoked_at IS NULL;
CREATE INDEX membership_by_tenant_idx ON membership (tenant_id)
    WHERE revoked_at IS NULL AND tenant_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Entitlements: which modules a tenant has, and therefore pays for.
--
-- The source of truth lives here, next to billing, so "did they pay for this"
-- and "is this on" cannot disagree (ARCHITECTURE §5.4).
-- ---------------------------------------------------------------------------
CREATE TABLE entitlement (
    tenant_id   UUID NOT NULL REFERENCES tenant (id) ON DELETE CASCADE,
    module_id   TEXT NOT NULL CHECK (module_id ~ '^[a-z][a-z0-9_]{0,47}$'),

    enabled_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at TIMESTAMPTZ,

    PRIMARY KEY (tenant_id, module_id)
);

-- Disabling a module never drops its tables, so this index is what "which
-- modules are live" reads, not the absence of a row.
CREATE INDEX entitlement_live_idx ON entitlement (tenant_id)
    WHERE disabled_at IS NULL;

-- ---------------------------------------------------------------------------
-- Audit: append-only.
--
-- D2 trades an event log for normalized tables, so this is where the "who
-- changed what" that an event log would have given for free is recovered.
-- Append-only is enforced by the database rather than by convention.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_entry (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Null for system-initiated actions (provisioning workers, reapers).
    actor_identity_id UUID REFERENCES identity (id) ON DELETE SET NULL,
    -- Set when platform staff acted on a tenant's behalf. Both parties are
    -- recorded, so an impersonated action is never indistinguishable from one
    -- the tenant took themselves (ARCHITECTURE §1.9).
    on_behalf_of_identity_id UUID REFERENCES identity (id) ON DELETE SET NULL,

    action       TEXT NOT NULL CHECK (length(action) BETWEEN 1 AND 128),
    subject_type TEXT NOT NULL CHECK (length(subject_type) BETWEEN 1 AND 64),
    subject_id   TEXT NOT NULL,
    detail       JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX audit_by_subject_idx ON audit_entry (subject_type, subject_id, at DESC);
CREATE INDEX audit_by_actor_idx ON audit_entry (actor_identity_id, at DESC)
    WHERE actor_identity_id IS NOT NULL;

CREATE FUNCTION audit_entry_is_append_only() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'audit_entry is append-only (attempted %)', TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_entry_no_update
    BEFORE UPDATE OR DELETE ON audit_entry
    FOR EACH ROW EXECUTE FUNCTION audit_entry_is_append_only();
