-- Which tenant an audit entry concerns, as a column.
--
-- # Why a column and not a query
--
-- The trail is read by tenant: its owner reads what happened to their company,
-- and support reads one company's history. Before this, "the entries about
-- tenant T" was an OR across the subject, a `tenant` key some writers put in
-- `detail`, and a join to `api_key` — and `api_key.revoked` and `api_key.rotated`
-- carried no tenant anywhere in the row, so no query could find them. Every
-- writer now says which tenant, or that there is none, through `record()`.
--
-- # No foreign key
--
-- The trail outlives the tenant: a failed signup's row and an expired demo's
-- are deleted, and their entries (`tenant.abandoned`, `tenant.demo_reaped`)
-- are the only record they existed. `ON DELETE SET NULL` would also be an
-- UPDATE, which the trigger refuses — the bug `0007_erasure.sql` fixed once.
ALTER TABLE audit_entry ADD COLUMN IF NOT EXISTS tenant_id UUID;

-- The tenant an entry concerns, from the three places it used to be: a subject
-- that is the tenant; a `tenant` key in `detail` (`membership.*`,
-- `api_key.issued`), where a platform membership's `"tenant": null` reads as SQL
-- NULL and stays out; and the key's tenant for `api_key.revoked` and
-- `api_key.rotated`, which named only the key. NULL when none says — a person, a
-- cluster, a signup, the platform's own outbox. A value that is not a UUID
-- raises, which aborts the backfill or refuses the insert rather than leave an
-- entry out of its tenant's view.
--
-- One function because two things use it, and they must agree: the backfill
-- below, and the trigger after it.
CREATE OR REPLACE FUNCTION audit_entry_tenant(subject_type TEXT, subject_id TEXT, detail JSONB)
RETURNS UUID AS $$
BEGIN
    IF subject_type = 'tenant' THEN
        RETURN subject_id::uuid;
    END IF;
    IF detail ->> 'tenant' IS NOT NULL THEN
        RETURN (detail ->> 'tenant')::uuid;
    END IF;
    IF subject_type = 'api_key' THEN
        RETURN (SELECT k.tenant_id FROM api_key k WHERE k.id::text = subject_id);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql STABLE;

-- The rows written before this. The function `0007` installed does not know the
-- new column, so an UPDATE that changes only it passes; it is re-pinned below,
-- after this.
UPDATE audit_entry SET tenant_id = audit_entry_tenant(subject_type, subject_id, detail)
 WHERE tenant_id IS NULL AND audit_entry_tenant(subject_type, subject_id, detail) IS NOT NULL;

-- # The rows written after this by the build before it
--
-- A deploy overlaps: until the last old pod drains, it inserts entries in the
-- shape it knows, with no `tenant_id`, and the trigger below forbids filling one
-- in afterwards. So the database fills it at the insert, by the backfill's
-- rules, and only where the writer left it NULL — the current build's `record()`
-- states it, and a tenant it states is never replaced. The current build's
-- `None` writers (staff, dead letters, identities, clusters, signups) name no
-- tenant in a way these rules find, so they stay out of every tenant's trail.
--
-- In this migration and not a later one, so the trigger exists from the instant
-- the column does. INSERT only: the append-only trigger is UPDATE and DELETE,
-- so the two never fire on the same statement.
CREATE OR REPLACE FUNCTION audit_entry_fills_tenant() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.tenant_id IS NULL THEN
        NEW.tenant_id := audit_entry_tenant(NEW.subject_type, NEW.subject_id, NEW.detail);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER audit_entry_tenant_on_insert
    BEFORE INSERT ON audit_entry
    FOR EACH ROW EXECUTE FUNCTION audit_entry_fills_tenant();

CREATE INDEX IF NOT EXISTS audit_by_tenant_idx ON audit_entry (tenant_id, id DESC)
    WHERE tenant_id IS NOT NULL;

-- A person's own view asks for entries whose subject, actor **or** on-behalf-of
-- is them. The first two have indexes since `0001`; without this one, the third
-- makes that OR a scan of the whole trail.
CREATE INDEX IF NOT EXISTS audit_by_on_behalf_idx ON audit_entry (on_behalf_of_identity_id)
    WHERE on_behalf_of_identity_id IS NOT NULL;

-- `0007`'s one permitted UPDATE — an actor nulled, nothing else changed — now
-- names the new column too. Without this line the column is a hole: an UPDATE
-- that moved an entry to another tenant, or out of every tenant's view, would
-- pass, because the whitelist would not know to compare it.
CREATE OR REPLACE FUNCTION audit_entry_is_append_only() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.id           =              OLD.id
       AND NEW.at           =              OLD.at
       AND NEW.action       =              OLD.action
       AND NEW.subject_type =              OLD.subject_type
       AND NEW.subject_id   =              OLD.subject_id
       AND NEW.detail       IS NOT DISTINCT FROM OLD.detail
       AND NEW.tenant_id    IS NOT DISTINCT FROM OLD.tenant_id
       AND (NEW.actor_identity_id IS NOT DISTINCT FROM OLD.actor_identity_id
            OR NEW.actor_identity_id IS NULL)
       AND (NEW.on_behalf_of_identity_id IS NOT DISTINCT FROM OLD.on_behalf_of_identity_id
            OR NEW.on_behalf_of_identity_id IS NULL)
    THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION 'audit_entry is append-only (attempted %)', TG_OP;
END;
$$ LANGUAGE plpgsql;
