-- SQLite variant of schema/data-model.sql — mechanical translation per the
-- mapping documented in the canonical file's header (uuid->TEXT,
-- timestamptz->TEXT RFC 3339, jsonb->TEXT, DEFAULT now() omitted,
-- gen_random_uuid() omitted). sectr-common embeds and executes this via
-- `migrate_sqlite()`; a parity test in journal/sqlite.rs asserts table /
-- column / events-CHECK sync with the canonical DDL.
--
-- PRAGMAs are NOT set here: `foreign_keys` is connection-scoped (set via
-- SqliteConnectOptions in the runner) and `journal_mode = WAL` likewise.

-- ----------------------------------------------------------------------------
-- Tenancy & identity
-- ----------------------------------------------------------------------------

CREATE TABLE orgs (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL
);

CREATE TABLE users (
    id          TEXT PRIMARY KEY,
    org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    email       TEXT NOT NULL,
    name        TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE (org_id, email)
);

-- ----------------------------------------------------------------------------
-- Apps, keys, deployments, secrets (registry)
-- ----------------------------------------------------------------------------

CREATE TABLE apps (
    id          TEXT PRIMARY KEY,
    org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    UNIQUE (org_id, slug)
);

-- kind = 'platform'  : control-plane key (CLI + dashboard), org-scoped
-- kind = 'invocation': per-app client key for POST /v1/sessions + SSE
-- Only key_hash is stored; keys are never readable back.
CREATE TABLE api_keys (
    id            TEXT PRIMARY KEY,
    org_id        TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    app_id        TEXT REFERENCES apps(id) ON DELETE CASCADE,  -- NULL for platform keys
    kind          TEXT NOT NULL CHECK (kind IN ('platform', 'invocation')),
    name          TEXT NOT NULL,
    key_hash      TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL,
    last_used_at  TEXT,
    revoked_at    TEXT,
    CHECK ((kind = 'platform' AND app_id IS NULL) OR (kind = 'invocation' AND app_id IS NOT NULL))
);

CREATE TABLE deployments (
    id            TEXT PRIMARY KEY,
    app_id        TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    version       INTEGER NOT NULL,
    status        TEXT NOT NULL DEFAULT 'building'
                  CHECK (status IN ('building', 'active', 'failed', 'retired')),
    manifest      TEXT NOT NULL,       -- authoritative manifest from SECTR_MANIFEST import
    artifact_uri  TEXT,                -- tarball location (set once built)
    created_at    TEXT NOT NULL,
    UNIQUE (app_id, version)
);

-- Write-only by contract: no read API ever exposes values.
CREATE TABLE secrets (
    id               TEXT PRIMARY KEY,
    app_id           TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    key              TEXT NOT NULL,
    value_encrypted  TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT,
    UNIQUE (app_id, key)
);

-- ----------------------------------------------------------------------------
-- Sessions & journal
-- ----------------------------------------------------------------------------

-- status transitions: active -> awaiting_approval <-> active -> ended
CREATE TABLE sessions (
    id             TEXT PRIMARY KEY,
    app_id         TEXT NOT NULL REFERENCES apps(id),
    deployment_id  TEXT NOT NULL REFERENCES deployments(id),  -- sessions pin to the deployment they started on
    route          TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'active'
                   CHECK (status IN ('active', 'awaiting_approval', 'ended')),
    created_at     TEXT NOT NULL,
    updated_at     TEXT,
    ended_at       TEXT
);

CREATE INDEX idx_sessions_app ON sessions(app_id, created_at DESC);

CREATE TABLE events (
    id             TEXT PRIMARY KEY,      -- UUIDv7, journal-writer-generated
    session_id     TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    deployment_id  TEXT NOT NULL,
    seq            INTEGER NOT NULL CHECK (seq >= 0),
    type           TEXT NOT NULL CHECK (type IN (
                       -- runner-emitted (validated + journaled by the sidecar)
                       'MESSAGE_START',
                       'MESSAGE_DELTA',
                       'MESSAGE_END',
                       'TOOL_CALL',
                       'TOOL_APPROVAL_REQUIRED',
                       'ERROR',
                       'NO_MORE_ACTIONS',
                       'TRANSCRIPT_COMPACTED',
                       -- platform-emitted
                       'SESSION_CREATED',
                       'INVOCATION_STARTED',
                       'TOOL_RESULT',
                       'TOOL_APPROVAL_GRANTED',
                       'TOOL_APPROVAL_DENIED',
                       'SESSION_ERROR'
                   )),
    payload        TEXT NOT NULL CHECK (json_type(payload) = 'object'),
    extra          TEXT NOT NULL CHECK (json_type(extra) = 'object'),
    created_at     TEXT NOT NULL,
    UNIQUE (session_id, seq)
);

-- Client SSE catch-up is `WHERE session_id = $1 AND seq > $last ORDER BY seq`;
-- served by the UNIQUE index above. The SQLite dev journal polls; Postgres
-- LISTEN/NOTIFY wakes the tail (PgJournal).

-- ----------------------------------------------------------------------------
-- Leases (runner lifecycle)
-- ----------------------------------------------------------------------------

-- At most one live lease per session; released_at IS NULL marks it live.
-- Claim/heartbeat policy lives in sectr-worker; this table only records it.
CREATE TABLE leases (
    id            TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    runner_id     TEXT NOT NULL,
    invocation_id TEXT,
    claimed_at    TEXT NOT NULL,
    heartbeat_at  TEXT NOT NULL,
    expires_at    TEXT NOT NULL,
    released_at   TEXT
);

CREATE UNIQUE INDEX idx_leases_session_live ON leases(session_id) WHERE released_at IS NULL;
CREATE INDEX idx_leases_expiry ON leases(expires_at) WHERE released_at IS NULL;
