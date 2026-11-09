-- Endpoint deployments: the deployment runs on the customer's own endpoint
-- (the sidecar in serve mode on their serverless or containers), which the
-- server wakes over HTTP for each turn. NULL = a managed deployment, built
-- and run by the server's platform. Set once at creation, never changed: a
-- new URL is a new version.

ALTER TABLE deployments ADD COLUMN endpoint_url text NULL;
