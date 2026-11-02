-- Postgres variant of schema/data-model.sql — the canonical file IS
-- Postgres-canonical, so this is its verbatim table/column/index set (the
-- parity test in migrations/mod.rs asserts table / column / events-CHECK
-- sync with the canonical DDL). sectr-common embeds and executes this via
-- `migrate_postgres()`.
--
-- BEGIN/COMMIT are NOT set here: the migration runner wraps each version in
-- one transaction (transactional DDL). No gen_random_uuid() defaults — ids
-- are client-generated UUIDv7.

-- ----------------------------------------------------------------------------
-- Tenancy & identity
-- ----------------------------------------------------------------------------

CREATE TABLE orgs (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    slug        text NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id          uuid PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    email       text NOT NULL,
    name        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, email)
);

-- ----------------------------------------------------------------------------
-- Apps, keys, deployments, secrets (registry)
-- ----------------------------------------------------------------------------

CREATE TABLE apps (
    id          uuid PRIMARY KEY,
    org_id      uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    name        text NOT NULL,
    slug        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, slug)
);

-- kind = 'platform'  : control-plane key (CLI + dashboard), org-scoped
-- kind = 'invocation': per-app client key for POST /v1/sessions + SSE
-- Only key_hash is stored; keys are never readable back.
CREATE TABLE api_keys (
    id            uuid PRIMARY KEY,
    org_id        uuid NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    app_id        uuid REFERENCES apps(id) ON DELETE CASCADE,  -- NULL for platform keys
    kind          text NOT NULL CHECK (kind IN ('platform', 'invocation')),
    name          text NOT NULL,
    key_hash      text NOT NULL UNIQUE,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz,
    revoked_at    timestamptz,
    CHECK ((kind = 'platform' AND app_id IS NULL) OR (kind = 'invocation' AND app_id IS NOT NULL))
);

CREATE TABLE deployments (
    id            uuid PRIMARY KEY,
    app_id        uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    version       bigint NOT NULL,
    status        text NOT NULL DEFAULT 'building'
                  CHECK (status IN ('building', 'active', 'failed', 'retired')),
    manifest      jsonb NOT NULL,       -- authoritative manifest from SECTR_MANIFEST import
    artifact_uri  text,                 -- tarball location (set once built)
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (app_id, version)
);

-- Write-only by contract: no read API ever exposes values.
CREATE TABLE secrets (
    id               uuid PRIMARY KEY,
    app_id           uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    key              text NOT NULL,
    value_encrypted  text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz,
    UNIQUE (app_id, key)
);

-- ----------------------------------------------------------------------------
-- Sessions & journal
-- ----------------------------------------------------------------------------

-- status transitions: active -> awaiting_approval <-> active -> ended
CREATE TABLE sessions (
    id             uuid PRIMARY KEY,
    app_id         uuid NOT NULL REFERENCES apps(id),
    deployment_id  uuid NOT NULL REFERENCES deployments(id),  -- sessions pin to the deployment they started on
    route          text NOT NULL,
    status         text NOT NULL DEFAULT 'active'
                   CHECK (status IN ('active', 'awaiting_approval', 'ended')),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz,
    ended_at       timestamptz
);

CREATE INDEX idx_sessions_app ON sessions(app_id, created_at DESC);

CREATE TABLE events (
    id             uuid PRIMARY KEY,      -- UUIDv7, journal-writer-generated
    session_id     uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    deployment_id  uuid NOT NULL,
    seq            bigint NOT NULL CHECK (seq >= 0),
    type           text NOT NULL CHECK (type IN (
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
    payload        jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    extra          jsonb NOT NULL,       -- unknown envelope fields, preserved verbatim (forward compat)
    created_at     timestamptz NOT NULL,
    UNIQUE (session_id, seq)
);

-- Client SSE catch-up is `WHERE session_id = $1 AND seq > $last ORDER BY seq`;
-- served by the UNIQUE index above. The tail wakes on LISTEN/NOTIFY
-- (channel `sectr_events`, payload = session uuid — see PgJournal); the
-- SQLite dev journal polls.

-- ----------------------------------------------------------------------------
-- Leases (runner lifecycle)
-- ----------------------------------------------------------------------------

-- At most one live lease per session; released_at IS NULL marks it live.
-- Claim/heartbeat policy lives in the server-role layer; this table only
-- records it.
CREATE TABLE leases (
    id            uuid PRIMARY KEY,
    session_id    uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    runner_id     text NOT NULL,
    invocation_id uuid,
    claimed_at    timestamptz NOT NULL DEFAULT now(),
    heartbeat_at  timestamptz NOT NULL DEFAULT now(),
    expires_at    timestamptz NOT NULL,
    released_at   timestamptz
);

CREATE UNIQUE INDEX idx_leases_session_live ON leases(session_id) WHERE released_at IS NULL;
CREATE INDEX idx_leases_expiry ON leases(expires_at) WHERE released_at IS NULL;
