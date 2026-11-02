-- Deployments carry the failure reason of a `failed` build: a truncated
-- tail of the build/failure output (or the static stale-sweep message for
-- rows orphaned by a server restart). Surfaced through the public API's
-- Deployment DTO so the CLI/dashboard can show WHY a deploy failed without
-- server-log access. NULL while building and for successful builds.

ALTER TABLE deployments ADD COLUMN error text NULL;
