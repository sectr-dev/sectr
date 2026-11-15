-- One row: this server's random id for anonymous usage pings (docs:
-- reference/telemetry). The row is created on the first ping, so a server
-- with pings off has none.
-- (SQLite parity only: `sectr dev` runs no server, so nothing writes it.)

CREATE TABLE server_instance (
    singleton  INTEGER PRIMARY KEY DEFAULT 1 CHECK (singleton = 1),
    install_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
