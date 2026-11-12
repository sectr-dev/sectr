import assert from "node:assert/strict";
import { test } from "node:test";

import { lastTurnFailed } from "../src/lib/session-status.ts";

const user = { type: "INVOCATION_STARTED", payload: { reason: { kind: "user_message", input: "hi" } } };
const resume = { type: "INVOCATION_STARTED", payload: { reason: { kind: "approval_decision" } } };
const end = { type: "NO_MORE_ACTIONS", payload: {} };
const sessionError = { type: "SESSION_ERROR", payload: { message: "endpoint wake failed: …" } };
const runnerError = { type: "ERROR", payload: { message: "status_code: 401" } };
const approval = { type: "TOOL_APPROVAL_REQUIRED", payload: {} };

test("the last turn decides whether the session is in error", () => {
  assert.equal(lastTurnFailed([]), false);
  assert.equal(lastTurnFailed([user, end]), false);
  // Failed either way, with the sentinel after the error.
  assert.equal(lastTurnFailed([user, sessionError, end]), true);
  assert.equal(lastTurnFailed([user, runnerError, end]), true);
  assert.equal(lastTurnFailed([user, approval, resume, sessionError, end]), true);
  // A new turn recovers the session, from its start.
  assert.equal(lastTurnFailed([user, sessionError, end, user]), false);
  assert.equal(lastTurnFailed([user, runnerError, end, user, end]), false);
});
