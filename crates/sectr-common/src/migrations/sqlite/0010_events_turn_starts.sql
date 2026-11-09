-- The wakes summary (`GET /deployments/{id}/wakes`) reads a deployment's
-- recent turn starts and session errors. A partial index keeps that to the
-- few rows per turn it needs. 

CREATE INDEX idx_events_turn_starts ON events (deployment_id, created_at DESC)
    WHERE type IN ('INVOCATION_STARTED', 'SESSION_ERROR');
