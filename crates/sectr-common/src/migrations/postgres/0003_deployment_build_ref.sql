-- Deployments carry a builder-specific reference for the CURRENT build
-- attempt (an opaque id the builder re-checks on startup to detect /
-- resume / fail builds left in flight by a server crash). Provider-neutral
-- by contract: each ImageBuilder stores whatever identifier it polls.
-- NULL once the build resolves (active/failed/retired) and for deployments
-- created before async builds existed.

ALTER TABLE deployments ADD COLUMN build_ref text NULL;
