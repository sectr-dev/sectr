import { type SeqEventLike, startsATurn } from "./turns.ts";

/**
 * Where a tool call stands:
 * - `running`: no result yet, and its turn hasn't ended (or is waiting on
 *   an approval);
 * - `done`: its `TOOL_RESULT` is journaled (the result itself may be null);
 * - `interrupted`: its turn ended before it finished. The turn was killed
 *   or failed mid-call, or, for a granted call, its resume failed to start,
 *   so it may never have run;
 * - `not_run`: its turn ended and it never ran, because its approval was
 *   denied or never decided.
 *
 * The journal never invents a result for a call its turn left open, so the
 * views say why there's none.
 */
export type ToolCallStatus = "running" | "done" | "interrupted" | "not_run";

function field(e: SeqEventLike, name: string): unknown {
  return (e.payload as Record<string, unknown> | null)?.[name];
}

/**
 * Each `TOOL_CALL`'s status, by the call event's seq. A turn ends at its
 * `NO_MORE_ACTIONS`, at an `ERROR` or `SESSION_ERROR` (errors end the turn;
 * the sentinel follows), or when the next user message starts one (which
 * abandons a call still waiting on its approval). An approval pause doesn't
 * end the turn: the call stays open across the resume.
 */
export function toolCallStatuses(events: readonly SeqEventLike[]): Map<number, ToolCallStatus> {
  const statuses = new Map<number, ToolCallStatus>();
  // The turn's calls still waiting for a result, by tool_call_id (a later
  // call with the same id replaces an earlier one).
  let open = new Map<string, { seq: number; approval?: "awaiting" | "granted" | "denied" }>();
  const endTurn = () => {
    for (const call of open.values()) {
      const ran = call.approval === undefined || call.approval === "granted";
      statuses.set(call.seq, ran ? "interrupted" : "not_run");
    }
    open = new Map();
  };

  for (const e of events) {
    const id = String(field(e, "tool_call_id"));
    if (startsATurn(e)) {
      endTurn();
      continue;
    }
    switch (e.type) {
      case "TOOL_CALL":
        open.set(id, { seq: e.seq });
        statuses.set(e.seq, "running");
        break;
      case "TOOL_RESULT": {
        const call = open.get(id);
        if (call) {
          statuses.set(call.seq, "done");
          open.delete(id);
        }
        break;
      }
      case "TOOL_APPROVAL_REQUIRED":
      case "TOOL_APPROVAL_GRANTED":
      case "TOOL_APPROVAL_DENIED": {
        const call = open.get(id);
        if (call) {
          call.approval =
            e.type === "TOOL_APPROVAL_REQUIRED"
              ? "awaiting"
              : e.type === "TOOL_APPROVAL_GRANTED"
                ? "granted"
                : "denied";
        }
        break;
      }
      case "ERROR":
      case "SESSION_ERROR":
      case "NO_MORE_ACTIONS":
        endTurn();
        break;
    }
  }
  return statuses;
}
