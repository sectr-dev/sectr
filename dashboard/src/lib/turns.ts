/** The least of a journal event the turn helpers read. */
export interface EventLike {
  type: string;
  payload: unknown;
}

/** A journal event with its seq. */
export type SeqEventLike = EventLike & { seq: number };

/** Whether `e` starts a turn: a user message's `INVOCATION_STARTED`. An
 *  approval resume continues the turn it paused. */
export function startsATurn(e: EventLike): boolean {
  if (e.type !== "INVOCATION_STARTED") return false;
  const reason = (e.payload as { reason?: { kind?: string } } | null)?.reason;
  return reason?.kind === "user_message";
}

/** Whether `e` fails its turn: an `ERROR` (the agent raised) or a
 *  `SESSION_ERROR` (the platform ended it). Either ends the turn, and the
 *  sentinel follows. */
export function isTurnError(e: EventLike): boolean {
  return e.type === "ERROR" || e.type === "SESSION_ERROR";
}

/**
 * The seqs of the `NO_MORE_ACTIONS` events that end a failed turn: one
 * holding an `ERROR` (the agent raised) or a `SESSION_ERROR` (the platform
 * ended it). Errors end the turn, so the sentinel follows them; the views
 * show those ends as failed rather than done.
 */
export function failedTurnEnds(events: readonly SeqEventLike[]): Set<number> {
  const failed = new Set<number>();
  let errored = false;
  for (const e of events) {
    if (startsATurn(e)) errored = false;
    else if (isTurnError(e)) errored = true;
    else if (e.type === "NO_MORE_ACTIONS") {
      if (errored) failed.add(e.seq);
      errored = false;
    }
  }
  return failed;
}
