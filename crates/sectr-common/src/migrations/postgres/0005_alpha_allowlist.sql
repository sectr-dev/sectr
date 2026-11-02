-- Alpha allowlist — admin-curated emails that may self-serve
-- provision their org + first platform key via POST /alpha/provision
-- (dashboard login page). One row per email:
--
-- - `provisioned_at NULL` → the email may provision (or, after a
--   successful provision, re-provision = roll; `allowlist-add` re-approves
--   a roll by clearing/updating the row).
-- - `provisioned_at` set → consumed: one-time claim, knowledge of the
--   email alone buys nothing after the first provision.
--
-- Lives beside the registry tables (same DB, same migration set — see
-- migrations/mod.rs); access is via inherent PgRegistry methods, NOT the
-- Registry trait (alpha is prod-Postgres-only).

CREATE TABLE alpha_allowlist (
    email          text PRIMARY KEY,
    added_at       timestamptz NOT NULL DEFAULT now(),
    added_by       text NOT NULL,
    provisioned_at timestamptz
);
