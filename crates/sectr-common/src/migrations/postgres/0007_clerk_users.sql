-- Clerk signup. A user row carries the Clerk user id (`sub` of
-- the Clerk session JWT) — the account's identity key (no linking by
-- email). NULL only on pre-Clerk rows. Partial unique index: many NULLs,
-- at most one row per Clerk id.

ALTER TABLE users ADD COLUMN clerk_user_id text;
CREATE UNIQUE INDEX idx_users_clerk ON users(clerk_user_id) WHERE clerk_user_id IS NOT NULL;
