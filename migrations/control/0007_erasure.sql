-- Make erasing a person possible without losing the audit trail.
--
-- # The bug this fixes
--
-- `audit_entry` is append-only, enforced by a trigger that refuses UPDATE and
-- DELETE. Its actor columns are `REFERENCES identity (id) ON DELETE SET NULL` —
-- and `SET NULL` is an UPDATE, so the trigger refuses it. The result:
--
--     DELETE FROM identity WHERE id = …
--     ERROR:  audit_entry is append-only (attempted UPDATE)
--
-- **An identity that has ever acted could not be deleted at all.** The
-- `ON DELETE SET NULL` clause was unreachable from the day it was written, and
-- nothing noticed because nothing had ever tried.
--
-- # Why that matters here
--
-- Saudi Arabia's Personal Data Protection Law gives a data subject the right to
-- have their personal data destroyed. An account that cannot be deleted is a
-- request that cannot be honoured, and "our schema will not let us" is not one
-- of the lawful grounds for refusing.
--
-- # What changes, and what does not
--
-- The trigger now permits **exactly one** shape of UPDATE: one that nulls an
-- actor column and changes nothing else. Every other update, and every delete,
-- is refused as before.
--
-- So the trail keeps what it is for — what was done, to what, and when — and
-- loses only the link to a person who has asked to be forgotten. An entry whose
-- actor is null is one this schema has always allowed: it is what a
-- system-initiated action looks like.
--
-- # Why not simply drop the trigger
--
-- Because then an audit trail is a table anybody can rewrite, which is the
-- thing it exists not to be. The narrow permission is the point: it is easier
-- to reason about "actors may be nulled" than about "trust the application".
CREATE OR REPLACE FUNCTION audit_entry_is_append_only() RETURNS TRIGGER AS $$
BEGIN
    -- Erasure: the entry is untouched except that an actor became nobody.
    --
    -- Each actor column may stay as it is or go to NULL, and never to a
    -- different identity — one person's actions cannot be attributed to
    -- another. Deleting one of two people named on an entry nulls their column
    -- and leaves the other's, which is why they are tested separately.
    IF TG_OP = 'UPDATE'
       AND NEW.id           =              OLD.id
       AND NEW.at           =              OLD.at
       AND NEW.action       =              OLD.action
       AND NEW.subject_type =              OLD.subject_type
       AND NEW.subject_id   =              OLD.subject_id
       AND NEW.detail       IS NOT DISTINCT FROM OLD.detail
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
