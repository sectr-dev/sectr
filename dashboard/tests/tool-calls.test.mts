import assert from "node:assert/strict";
import { test } from "node:test";

import { toolCallStatuses } from "../src/lib/tool-calls.ts";

/**
 * A journal from short tokens, one event per token, seq = index:
 * USER and RESUME are INVOCATION_STARTED with a user_message or
 * approval_decision reason; CALL:a, RESULT:a, ASK:a, GRANT:a and DENY:a are
 * TOOL_CALL, TOOL_RESULT and the approval events for tool_call_id `a`; any
 * other token is an event of that type.
 */
const journal = (tokens: string[]) =>
  tokens.map((token, seq) => {
    const [t, id] = token.split(":");
    const reason = { USER: "user_message", RESUME: "approval_decision" }[t];
    if (reason) return { type: "INVOCATION_STARTED", seq, payload: { reason: { kind: reason } } };
    const type =
      {
        CALL: "TOOL_CALL",
        RESULT: "TOOL_RESULT",
        ASK: "TOOL_APPROVAL_REQUIRED",
        GRANT: "TOOL_APPROVAL_GRANTED",
        DENY: "TOOL_APPROVAL_DENIED",
      }[t] ?? t;
    return { type, seq, payload: id === undefined ? {} : { tool_call_id: id } };
  });

/** The statuses as [seq, status] pairs, in seq order. */
const statuses = (tokens: string[]) =>
  [...toolCallStatuses(journal(tokens))].sort(([a], [b]) => a - b);

test("a call is running until its result, then done", () => {
  assert.deepEqual(statuses(["USER", "CALL:a"]), [[1, "running"]]);
  assert.deepEqual(statuses(["USER", "CALL:a", "RESULT:a", "NO_MORE_ACTIONS"]), [[1, "done"]]);
  // Two calls in one turn, answered out of order.
  assert.deepEqual(statuses(["USER", "CALL:a", "CALL:b", "RESULT:b", "RESULT:a"]), [
    [1, "done"],
    [2, "done"],
  ]);
});

test("a call still open when its turn ends is interrupted", () => {
  // Each ends the turn. A lease reclaim journals SESSION_ERROR, then the
  // sentinel.
  for (const end of ["NO_MORE_ACTIONS", "SESSION_ERROR", "ERROR"]) {
    assert.deepEqual(statuses(["USER", "CALL:a", end]), [[1, "interrupted"]], end);
  }
  // Only the open one: the answered call stays done.
  assert.deepEqual(statuses(["USER", "CALL:a", "RESULT:a", "CALL:b", "SESSION_ERROR", "NO_MORE_ACTIONS"]), [
    [1, "done"],
    [3, "interrupted"],
  ]);
  // A turn with no end (an old journal) is ended by the next user message.
  assert.deepEqual(statuses(["USER", "CALL:a", "USER", "CALL:b"]), [
    [1, "interrupted"],
    [3, "running"],
  ]);
});

test("an approval pause doesn't end the turn", () => {
  // Waiting on the decision, then resumed and answered.
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a"]), [[1, "running"]]);
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a", "GRANT:a", "RESUME", "RESULT:a", "NO_MORE_ACTIONS"]), [
    [1, "done"],
  ]);
  // Granted, then the resume was killed mid-call, or failed to start: the
  // journal can't tell which, so it's interrupted either way.
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a", "GRANT:a", "RESUME", "SESSION_ERROR", "NO_MORE_ACTIONS"]), [
    [1, "interrupted"],
  ]);
});

test("a call whose approval was denied or never decided didn't run", () => {
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a", "DENY:a", "RESUME", "NO_MORE_ACTIONS"]), [[1, "not_run"]]);
  // A runner may still journal a result for a denied call (what it told the
  // model): that's done.
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a", "DENY:a", "RESUME", "RESULT:a", "NO_MORE_ACTIONS"]), [
    [1, "done"],
  ]);
  // A later user message abandons a call still waiting on its decision.
  assert.deepEqual(statuses(["USER", "CALL:a", "ASK:a", "USER"]), [[1, "not_run"]]);
});

test("a reused tool_call_id in a later turn is its own call", () => {
  assert.deepEqual(statuses(["USER", "CALL:a", "NO_MORE_ACTIONS", "USER", "CALL:a", "RESULT:a"]), [
    [1, "interrupted"],
    [4, "done"],
  ]);
});
