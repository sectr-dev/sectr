import assert from "node:assert/strict";
import { test } from "node:test";

import { failedTurnEnds } from "../src/lib/turns.ts";

/** A journal from event types; USER and RESUME are INVOCATION_STARTED with
 *  a user_message or approval_decision reason. */
const journal = (types: string[]) =>
  types.map((t, seq) => {
    const kind = { USER: "user_message", RESUME: "approval_decision" }[t];
    return kind
      ? { type: "INVOCATION_STARTED", seq, payload: { reason: { kind } } }
      : { type: t, seq, payload: {} };
  });

test("a turn's end is failed when the turn holds an error", () => {
  // 0 user, 1 end: done. 2 user, 3 ERROR, 4 end: failed.
  assert.deepEqual([...failedTurnEnds(journal(["USER", "NO_MORE_ACTIONS", "USER", "ERROR", "NO_MORE_ACTIONS"]))], [4]);
  // A session error after an approval resume fails the whole turn.
  assert.deepEqual(
    [...failedTurnEnds(journal(["USER", "TOOL_APPROVAL_REQUIRED", "RESUME", "SESSION_ERROR", "NO_MORE_ACTIONS"]))],
    [4],
  );
  // The next turn starts clean.
  assert.deepEqual(
    [...failedTurnEnds(journal(["USER", "SESSION_ERROR", "NO_MORE_ACTIONS", "USER", "NO_MORE_ACTIONS"]))],
    [2],
  );
  // An error with no sentinel yet has no end to mark.
  assert.deepEqual([...failedTurnEnds(journal(["USER", "SESSION_ERROR"]))], []);
});
