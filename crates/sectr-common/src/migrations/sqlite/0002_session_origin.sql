-- Sessions carry their origin (which principal class created them) —
-- 'api' (end-user agent via an invocation key) or 'playground' (dashboard
-- via a platform principal). Serves the sessions-api SessionSummary.origin.
-- Existing rows: dev sessions are playground-originated.

ALTER TABLE sessions ADD COLUMN origin TEXT NOT NULL DEFAULT 'playground' CHECK (origin IN ('api', 'playground'));
