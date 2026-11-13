/** The least of a journal event this module reads. */
interface EventLike {
  type: string;
  payload: unknown;
}

function startsATurn(e: EventLike): boolean {
  if (e.type !== "INVOCATION_STARTED") return false;
  const reason = (e.payload as { reason?: { kind?: string } } | null)?.reason;
  return reason?.kind === "user_message";
}

/**
 * Whether the session's last turn failed: it holds an `ERROR` or a
 * `SESSION_ERROR`. The server derives the session's `error` status the same
 * way (`derive_session_status`), so the live view can follow the tail
 * without refetching the session. A new turn starts the session afresh.
 */
export function lastTurnFailed(events: readonly EventLike[]): boolean {
  let start = 0;
  for (let i = events.length - 1; i >= 0; i--) {
    if (startsATurn(events[i])) {
      start = i;
      break;
    }
  }
  return events.slice(start).some((e) => e.type === "ERROR" || e.type === "SESSION_ERROR");
}
