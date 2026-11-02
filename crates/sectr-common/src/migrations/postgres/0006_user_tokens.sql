-- User-scope browser tokens (sks_) + session ownership.
--
-- api_keys gains the third key class ('user'): a browser token minted by
-- the app's backend (POST /tokens), bound to one external_user_id and a
-- hard expiry (expired == revoked at lookup). The class/app_id table
-- CHECK is rebuilt accordingly (constraint replacement, not a table
-- rebuild — Postgres drops/adds table constraints cheaply).
--
-- sessions gains an owner: external_user_id, set from the creating user
-- token and immutable thereafter. NULL = backend/platform-created (no
-- owner; never visible to user tokens).
--
-- Existing rows are unaffected: all new columns are nullable.

ALTER TABLE api_keys ADD COLUMN external_user_id text;
ALTER TABLE api_keys ADD COLUMN expires_at timestamptz;
-- The invocation key that minted this token: revoking that key revokes its
-- tokens (cascaded in revoke_api_key); deleting it deletes them.
ALTER TABLE api_keys ADD COLUMN minted_by_key_id uuid REFERENCES api_keys(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD COLUMN external_user_id text;

ALTER TABLE api_keys DROP CONSTRAINT api_keys_kind_check;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_kind_check
    CHECK (kind IN ('platform', 'invocation', 'user'));

ALTER TABLE api_keys DROP CONSTRAINT api_keys_check;
ALTER TABLE api_keys ADD CONSTRAINT api_keys_check
    CHECK ((kind = 'platform' AND app_id IS NULL)
           OR (kind = 'invocation' AND app_id IS NOT NULL)
           OR (kind = 'user' AND app_id IS NOT NULL AND external_user_id IS NOT NULL AND expires_at IS NOT NULL AND minted_by_key_id IS NOT NULL));

-- Serves the periodic purge of expired user tokens.
CREATE INDEX idx_api_keys_user_expiry ON api_keys(expires_at) WHERE kind = 'user';
