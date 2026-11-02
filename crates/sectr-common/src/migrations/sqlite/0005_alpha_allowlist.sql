-- Alpha allowlist — Postgres 0005_alpha_allowlist.sql, mechanical
-- SQLite translation (text stays text; timestamptz->TEXT; DEFAULT now()
-- omitted; see the canonical file's header mapping). Access is via
-- inherent PgRegistry methods only — SQLite has no alpha endpoints.

CREATE TABLE alpha_allowlist (
    email          TEXT PRIMARY KEY,
    added_at       TEXT NOT NULL,
    added_by       TEXT NOT NULL,
    provisioned_at TEXT
);
