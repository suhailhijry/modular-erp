-- Which sealing key a second factor's secret is sealed under.
--
-- `module_secret.sealed_with` has recorded this for tenant secrets since
-- tenant 0006; the TOTP secrets `0015` added to `authenticator` never did. So a
-- change of `SEALING_KEY` left every enrolled authenticator app unreadable, and
-- nothing could find which rows still needed moving to a new key.
--
-- The column is the authority `SealingKey::unseal` reads: a row is opened with
-- the key it names and no other, and a name the deployment does not hold is
-- refused. `migrator reseal` moves rows onto the current key and fills it in.
--
-- NULL means sealed before this column existed, and is read by trying every
-- key the deployment holds. Only `totp` and `totp_pending` rows are sealed, so
-- the column stays NULL on every other kind. No constraint says so: nothing
-- writes it for them, and a CHECK would be the kind of addition the
-- expand-only rule makes a draining pod trip over.
ALTER TABLE authenticator ADD COLUMN sealed_with TEXT;

COMMENT ON COLUMN authenticator.sealed_with IS
    'totp and totp_pending: the id of the sealing key the secret is sealed under (SEALING_KEY''s <id>). NULL: sealed before this was recorded; migrator reseal fills it in.';
