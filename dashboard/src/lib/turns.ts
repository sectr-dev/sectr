/** The least of a journal event this module reads. */
interface EventLike {
  type: string;
  seq: number;
  payload: unknown;
}

function startsATurn(e: EventLike): boolean {
  if (e.type !== "INVOCATION_STARTED") return false;
  const reason = (e.payload as { reason?: { kind?: string } } | null)?.reason;
  return reason?.kind === "user_message";
}

/**
 * The seqs of the `NO_MORE_ACTIONS` events that end a failed turn: one
 * holding an `ERROR` (the agent raised) or a `SESSION_ERROR` (the platform
 * ended it). Errors end the turn, so the sentinel follows them; the views
 * show those ends as failed rather than done.
 */
export function failedTurnEnds(events: readonly EventLike[]): Set<number> {
  const failed = new Set<number>();
  let errored = false;
  for (const e of events) {
    if (startsATurn(e)) errored = false;
    else if (e.type === "ERROR" || e.type === "SESSION_ERROR") errored = true;
    else if (e.type === "NO_MORE_ACTIONS") {
      if (errored) failed.add(e.seq);
      errored = false;
    }
  }
  return failed;
}
