-- Browser login for `sectr auth login` (device authorization flow).
-- One row per login REQUEST; only the hash of the device code is stored. The
-- minted key is an ordinary `api_keys` row (no expiry) created when the CLI
-- polls after approval; deleting a request row never touches that key.

CREATE TABLE cli_logins (
    id               uuid PRIMARY KEY,
    device_code_hash text NOT NULL UNIQUE,
    user_code        text NOT NULL UNIQUE,
    client_name      text NOT NULL,
    status           text NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'approved', 'denied', 'consumed')),
    org_id           uuid REFERENCES orgs(id) ON DELETE CASCADE,
    approved_by      uuid REFERENCES api_keys(id) ON DELETE SET NULL,
    key_id           uuid REFERENCES api_keys(id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    expires_at       timestamptz NOT NULL
);
CREATE INDEX idx_cli_logins_expiry ON cli_logins(expires_at);
