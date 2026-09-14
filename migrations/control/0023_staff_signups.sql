-- Staff set up a tenant for a company that has paid.
--
-- Signup is closed on a production deployment (decided 2026-09-14: a tenant
-- gets what it asked for, after it pays, and there is no trial — the public
-- demo is the trial). So a pending signup can now be filed by platform staff
-- for an owner who has not typed a password: `created_by` names the staff
-- member, and the owner chooses a password — or proves an existing account's —
-- at the confirmation link, exactly as an invitation is accepted.
--
-- The one-owner rule is **widened**: a row may carry neither an identity nor a
-- hash only when staff filed it. Every row the previous build can write names
-- exactly one, as before, and satisfies the new rule.
ALTER TABLE pending_signup
    ADD COLUMN IF NOT EXISTS created_by UUID REFERENCES identity (id) ON DELETE SET NULL;

ALTER TABLE pending_signup
    DROP CONSTRAINT IF EXISTS pending_signup_names_one_owner;
ALTER TABLE pending_signup
    ADD CONSTRAINT pending_signup_names_one_owner CHECK (
        ((identity_id IS NULL) <> (password_hash IS NULL))
        OR (created_by IS NOT NULL AND password_hash IS NULL)
    );

COMMENT ON COLUMN pending_signup.created_by IS
    'The staff member who filed this signup for a paying customer; NULL for a self-signup. The owner sets a password at the link.';
