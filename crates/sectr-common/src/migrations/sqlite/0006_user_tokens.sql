-- User-scope browser tokens (sks_) + session ownership.
--
-- api_keys gains the third key class ('user'): a browser token minted by
-- the app's backend (POST /tokens), bound to one external_user_id and a
-- hard expiry (expired == revoked at lookup). SQLite cannot alter CHECK
-- constraints in place, so the table is rebuilt (rename old → create
-- canonical → copy → drop; no other table references api_keys, so the
-- rebuild is safe).
--
-- sessions gains an owner: external_user_id, set from the creating user
-- token and immutable thereafter. NULL = backend/platform-created (no
-- owner; never visible to user tokens).
--
-- Existing rows are unaffected: all new columns are nullable, and the
-- copied rows carry NULL for both (only 'user'-kind rows ever use them).

ALTER TABLE sessions ADD COLUMN external_user_id TEXT;

ALTER TABLE api_keys RENAME TO api_keys_old_rebuild;

CREATE TABLE api_keys (
    id               TEXT PRIMARY KEY,
    org_id           TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    app_id           TEXT REFERENCES apps(id) ON DELETE CASCADE,  -- NULL for platform keys
    kind             TEXT NOT NULL CHECK (kind IN ('platform', 'invocation', 'user')),
    name             TEXT NOT NULL,
    key_hash         TEXT NOT NULL UNIQUE,
    external_user_id TEXT,  -- user tokens only: the bound end-user identity
    expires_at       TEXT,  -- user tokens only: hard expiry (expired == revoked)
    minted_by_key_id TEXT REFERENCES api_keys(id) ON DELETE CASCADE,  -- user tokens only: the minting invocation key (its revocation revokes this token)
    created_at       TEXT NOT NULL,
    last_used_at     TEXT,
    revoked_at       TEXT,
    CHECK ((kind = 'platform' AND app_id IS NULL) OR (kind = 'invocation' AND app_id IS NOT NULL) OR (kind = 'user' AND app_id IS NOT NULL AND external_user_id IS NOT NULL AND expires_at IS NOT NULL AND minted_by_key_id IS NOT NULL))
);

INSERT INTO api_keys (id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, last_used_at, revoked_at)
    SELECT id, org_id, app_id, kind, name, key_hash, NULL, NULL, created_at, last_used_at, revoked_at FROM api_keys_old_rebuild;

DROP TABLE api_keys_old_rebuild;

-- Serves the periodic purge of expired user tokens.
CREATE INDEX idx_api_keys_user_expiry ON api_keys(expires_at) WHERE kind = 'user';
