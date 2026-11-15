-- One row: this server's random id for anonymous usage pings (docs:
-- reference/telemetry). The row is created on the first ping, so a server
-- with pings off has none. In the database, not a file: recreating the
-- server's container keeps the id, and servers sharing a database share it.

CREATE TABLE server_instance (
    singleton  boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    install_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
