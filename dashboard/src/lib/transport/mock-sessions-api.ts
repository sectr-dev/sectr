/*
  MOCK implementation of `SessionsApi` (see ./types.ts) — fixtures from
  `mock-data.ts` plus a client-side runtime store, behaving like the real
  gateway: async reads, a journal you subscribe to (full replay from seq 0,
  then live), turns that stream in over time, and server-derived statuses.

  Where the real transport speaks HTTP+SSE, this one speaks timers: turns
  journal instantly (INVOCATION_STARTED), then the scripted runner output
  arrives event-by-event (~1s apart) through the same subscription path the
  inspector uses live. Events land in the store whether or not anyone is
  subscribed, so navigation races never lose events.

  Fixture events are UI-shaped (SectrEvent, string deltas) and converted to
  contract shape here — mock-data.ts is a fixture SOURCE, never rendered
  directly. Runtime (playground) sessions are stored contract-shaped in
  sessionStorage so a shared/reloaded URL still resolves.
*/

import type {
  ApprovalDecision,
  DevStatus,
  JournalEvent,
  ListSessionsParams,
  NewSessionRequest,
  PendingApproval,
  Session,
  SessionStreamOptions,
  SessionsApi,
} from "./types";
import {
  mockEventsBySession,
  mockSessions,
  getSessionContinuation,
  getChatReply,
} from "../mock-data";
import { MOCK_EMPTY } from "../mock-scenario";
import type { InvocationStartedPayload, SectrEvent } from "../types";

// ---------------------------------------------------------------------------
// Fixture conversion (UI-shaped fixtures → contract envelopes)
// ---------------------------------------------------------------------------

/** MESSAGE_DELTA.delta is a string in fixtures, {type, text} on the wire. */
function toContractEvent(e: SectrEvent): JournalEvent {
  if (e.type === "MESSAGE_DELTA") {
    const p = e.payload as { message_id: string; delta: string };
    return {
      ...e,
      payload: { message_id: p.message_id, delta: { type: "text", text: p.delta } },
    } as unknown as JournalEvent;
  }
  return e as unknown as JournalEvent;
}

/** The fixture rows carry the UI's app_name; the contract row doesn't. */
function toContractSession(s: Session & { app_name?: string }): Session {
  const rest: Session = { ...s };
  delete (rest as { app_name?: string }).app_name;
  return rest;
}

// ---------------------------------------------------------------------------
// The store: fixture journals (converted lazily, then mutable) + runtime
// sessions (sessionStorage, contract-shaped) + live subscribers.
// ---------------------------------------------------------------------------

const eventStore = new Map<string, JournalEvent[]>();
const subscribers = new Map<string, Set<(e: JournalEvent) => void>>();
/** Fixture rows are module constants — runtime status changes land here. */
const statusOverrides = new Map<string, Session["status"]>();

const RUNTIME_KEY = "sectr.runtimeSessions";
interface RuntimeStore {
  sessions: Session[];
  events: Record<string, JournalEvent[]>;
}

/** Outside the browser (SSR, node smoke) the runtime store lives in memory. */
const runtimeMemory: RuntimeStore = { sessions: [], events: {} };

function readRuntime(): RuntimeStore {
  if (typeof window === "undefined") return runtimeMemory;
  try {
    return JSON.parse(sessionStorage.getItem(RUNTIME_KEY) ?? "") as RuntimeStore;
  } catch {
    return { sessions: [], events: {} };
  }
}

function writeRuntime(store: RuntimeStore) {
  if (typeof window === "undefined") {
    runtimeMemory.sessions = store.sessions;
    runtimeMemory.events = store.events;
    return;
  }
  sessionStorage.setItem(RUNTIME_KEY, JSON.stringify(store));
}

function eventsFor(sessionId: string): JournalEvent[] {
  let events = eventStore.get(sessionId);
  if (!events) {
    const runtime = readRuntime().events[sessionId] ?? [];
    events = (mockEventsBySession[sessionId] ?? runtime).map(toContractEvent);
    eventStore.set(sessionId, events);
  }
  return events;
}

function allSessions(): Session[] {
  // Empty demo account: no apps, so no sessions — runtime ones included
  // (they belong to fixture apps).
  if (MOCK_EMPTY) return [];
  return [
    ...mockSessions.map(toContractSession),
    ...readRuntime().sessions,
  ];
}

function statusFor(id: string, fallback: Session["status"]): Session["status"] {
  return statusOverrides.get(id) ?? fallback;
}

const TITLE_MAX_CHARS = 80;

/** The server's title derivation (sectr-common `derive_session_title`):
 *  the first user message's text, whitespace collapsed, truncated. */
function titleFor(events: JournalEvent[]): string | undefined {
  const started = events.find(
    (e) =>
      e.type === "INVOCATION_STARTED" &&
      (e.payload as { reason: { kind: string } }).reason.kind === "user_message",
  );
  if (!started) return undefined;
  const reason = (started.payload as InvocationStartedPayload).reason;
  const text = reason.kind === "user_message" ? reason.input : undefined;
  const words = text?.split(/\s+/).filter(Boolean).join(" ");
  if (!words) return undefined;
  const chars = [...words];
  return chars.length <= TITLE_MAX_CHARS
    ? words
    : `${chars.slice(0, TITLE_MAX_CHARS).join("").trimEnd()}…`;
}

/** A row as the server serves it: status, title and counts derived from
 *  the journal. */
function withDerived(s: Session): Session {
  const events = eventsFor(s.id);
  const title = titleFor(events);
  return {
    ...s,
    status: statusFor(s.id, s.status),
    last_event_at: events[events.length - 1]?.created_at ?? s.last_event_at,
    event_count: events.length,
    ...(title !== undefined && { title }),
  };
}

function persistRuntime(sessionId: string) {
  const runtime = readRuntime();
  if (!runtime.events[sessionId]) return; // fixture session — not persisted
  runtime.events[sessionId] = eventsFor(sessionId);
  const session = runtime.sessions.find((s) => s.id === sessionId);
  if (session) session.status = statusOverrides.get(sessionId) ?? session.status;
  writeRuntime(runtime);
}

/** Append an event to the journal and notify subscribers. */
function journal(sessionId: string, event: JournalEvent) {
  const events = eventsFor(sessionId);
  events.push(event);
  // Server-derived statuses (contract rule: derived from the journal).
  if (event.type === "TOOL_APPROVAL_REQUIRED") {
    statusOverrides.set(sessionId, "awaiting_approval");
  } else if (event.type === "TOOL_APPROVAL_GRANTED" || event.type === "TOOL_APPROVAL_DENIED") {
    statusOverrides.set(sessionId, "active");
  }
  persistRuntime(sessionId);
  subscribers.get(sessionId)?.forEach((cb) => cb(event));
}

/**
 * Stream a scripted tail into the journal (the mock's runner): re-stamped
 * seq/ids, fresh timestamps, ~1s apart — exactly how the inspector used to
 * fake it locally, now behind the same subscription path as the real
 * gateway's SSE tail.
 */
function streamIn(sessionId: string, scripted: SectrEvent[]) {
  if (scripted.length === 0) return;
  const events = eventsFor(sessionId);
  let seq = events[events.length - 1]?.seq ?? -1;
  let i = 0;
  const tick = () => {
    if (i >= scripted.length) return;
    journal(sessionId, {
      ...toContractEvent(scripted[i]),
      id: crypto.randomUUID(),
      seq: ++seq,
      created_at: new Date().toISOString(),
    });
    i += 1;
    if (i < scripted.length) timers.push(setTimeout(tick, 850 + (i % 3) * 300));
  };
  timers.push(setTimeout(tick, 500));
}

const timers: ReturnType<typeof setTimeout>[] = [];

// ---------------------------------------------------------------------------
// SessionsApi
// ---------------------------------------------------------------------------

export const mockSessionsApi: SessionsApi = {
  async createSession(req: NewSessionRequest): Promise<{ session_id: string }> {
    const sessionId = crypto.randomUUID();
    const now = new Date().toISOString();
    const session: Session = {
      id: sessionId,
      app_id: req.app,
      route: req.route,
      origin: "playground",
      deployment_id: "mock-deployment",
      status: "active",
      created_at: now,
      last_event_at: now,
      event_count: 0,
    };
    const runtime = readRuntime();
    runtime.sessions.push(session);
    runtime.events[sessionId] = [];
    writeRuntime(runtime);
    eventStore.set(sessionId, runtime.events[sessionId]);

    // The turn journals instantly, then the runner streams (like the real
    // 202: scheduled, not done — output arrives on the tail).
    let seq = -1;
    const mk = (type: JournalEvent["type"], payload: unknown): JournalEvent =>
      ({
        id: crypto.randomUUID(),
        session_id: sessionId,
        deployment_id: session.deployment_id,
        seq: ++seq,
        type,
        payload,
        created_at: new Date().toISOString(),
      }) as JournalEvent;
    journal(sessionId, mk("SESSION_CREATED", { app_id: req.app, route: req.route }));
    journal(
      sessionId,
      mk("INVOCATION_STARTED", {
        invocation_id: crypto.randomUUID(),
        route: req.route,
        deployment_id: session.deployment_id,
        reason: { kind: "user_message", input: req.input },
      }),
    );
    streamIn(sessionId, getChatReply(sessionId, session.deployment_id, 1));
    return { session_id: sessionId };
  },

  async listSessions(params: ListSessionsParams = {}): Promise<Session[]> {
    const rows = allSessions().map(withDerived);
    return params.app_id ? rows.filter((s) => s.app_id === params.app_id) : rows;
  },

  async getSession(sessionId: string): Promise<Session | undefined> {
    const s = allSessions().find((x) => x.id === sessionId);
    return s && withDerived(s);
  },

  async sendSessionInput(sessionId: string, input: string): Promise<void> {
    const events = eventsFor(sessionId);
    const last = events[events.length - 1];
    journal(sessionId, {
      id: crypto.randomUUID(),
      session_id: sessionId,
      deployment_id: last?.deployment_id ?? "mock-deployment",
      seq: (last?.seq ?? -1) + 1,
      type: "INVOCATION_STARTED",
      payload: {
        invocation_id: crypto.randomUUID(),
        route:
          allSessions().find((x) => x.id === sessionId)?.route ?? "unknown",
        deployment_id: last?.deployment_id ?? "mock-deployment",
        reason: { kind: "user_message", input },
      },
      created_at: new Date().toISOString(),
    } as JournalEvent);
    streamIn(sessionId, getChatReply(sessionId, "mock-deployment", 0));
  },

  async decideApproval(sessionId: string, decision: ApprovalDecision): Promise<void> {
    const events = eventsFor(sessionId);
    const pending = [...events]
      .reverse()
      .find((e) => e.type === "TOOL_APPROVAL_REQUIRED");
    if (!pending) return; // mirrors the real 409 (no pending approval)
    const approved = decision.approved;
    journal(sessionId, {
      id: crypto.randomUUID(),
      session_id: sessionId,
      deployment_id: pending.deployment_id,
      seq: (events[events.length - 1]?.seq ?? 0) + 1,
      type: approved ? "TOOL_APPROVAL_GRANTED" : "TOOL_APPROVAL_DENIED",
      payload: { tool_call_id: (pending.payload as { tool_call_id: string }).tool_call_id, approver: decision.approver },
      created_at: new Date().toISOString(),
    } as JournalEvent);
    streamIn(sessionId, getSessionContinuation(sessionId, approved));
  },

  async listApprovals(): Promise<PendingApproval[]> {
    const approvals: PendingApproval[] = [];
    for (const session of allSessions()) {
      const events = eventsFor(session.id);
      const pending = [...events].reverse().find((e) => e.type === "TOOL_APPROVAL_REQUIRED");
      if (!pending) continue;
      const decided = [...events]
        .reverse()
        .find(
          (e) =>
            e.seq > pending.seq &&
            (e.type === "TOOL_APPROVAL_GRANTED" || e.type === "TOOL_APPROVAL_DENIED"),
        );
      if (decided) continue;
      approvals.push({
        session: withDerived(session),
        event: pending,
      });
    }
    return approvals;
  },

  async prodHealth(): Promise<boolean> {
    return false; // no backend to probe; the mock badge comes from MOCK_MODE
  },

  async devStatus(): Promise<DevStatus | null> {
    return null; // fixtures — the badge's mock state comes from MOCK_MODE
  },

  openSessionStream(sessionId: string, opts: SessionStreamOptions): () => void {
    const events = eventsFor(sessionId);
    // Replay (seq 0 unless resuming), then hold for live events.
    let startSeq = -1;
    if (opts.lastEventId) {
      startSeq = events.find((e) => e.id === opts.lastEventId)?.seq ?? -1;
    }
    for (const e of events) {
      if (e.seq > startSeq) opts.onEvent(e);
    }
    const set = subscribers.get(sessionId) ?? new Set();
    set.add(opts.onEvent);
    subscribers.set(sessionId, set);
    return () => {
      set.delete(opts.onEvent);
    };
  },
};
