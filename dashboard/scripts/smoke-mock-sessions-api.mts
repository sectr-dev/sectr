/*
  Manual smoke of the MOCK sessions transport (fixtures + scripted
  streaming) — the browser-side twin of smoke-sessions-api.mts.

    cd dashboard && npx tsx scripts/smoke-mock-sessions-api.mts

  Exits 0 on green. No gateway needed: everything is fixtures + timers.
*/
import { mockSessionsApi as api } from "../src/lib/transport/mock-sessions-api";

const fail = (m: string): never => {
  console.error(`✗ ${m}`);
  process.exit(1);
};
const expect = (cond: boolean, m: string) => {
  if (!cond) fail(m);
  console.log(`✓ ${m}`);
};

const timeout = <T,>(p: Promise<T>, ms: number, what: string) =>
  Promise.race([p, new Promise<never>((_, rej) => setTimeout(() => rej(new Error(`${what} timed out`)), ms))]);

/** Subscribe and resolve when `until` fires (or the session errors). */
function awaitEvent(sessionId: string, until: (e: { type: string; seq: number }) => boolean) {
  return new Promise<{ type: string; seq: number; payload: unknown }[]>((resolve, reject) => {
    const seen: { type: string; seq: number; payload: unknown }[] = [];
    const close = api.openSessionStream(sessionId, {
      onEvent: (e) => {
        seen.push({ type: e.type, seq: e.seq, payload: e.payload });
        if (until(e)) {
          close();
          resolve(seen);
        }
      },
      onError: (err) => reject(err),
    });
  });
}

// --- 1. createSession: SESSION_CREATED + INVOCATION_STARTED journal
// instantly; the scripted runner streams in over time.
const created = await api.createSession({
  app: "app-support",
  route: "/customer-support",
  input: "hello there",
});
expect(typeof created.session_id === "string", "createSession → session_id");
const early = await api.getSession(created.session_id);
expect(early?.status === "active", "new session is active");

const done1 = awaitEvent(created.session_id, (e) => e.type === "NO_MORE_ACTIONS");
const events1 = await timeout(done1, 20_000, "scripted reply");
expect(
  events1.some((e) => e.type === "MESSAGE_DELTA" && (e.payload as { delta: { type: string; text: string } }).delta.type === "text"),
  "deltas arrive in contract shape ({type, text})",
);
expect(events1[0]!.type === "SESSION_CREATED" && events1[1]!.type === "INVOCATION_STARTED", "replay starts at seq 0 (SESSION_CREATED)");

// Follow-up input → another turn on the same tail.
const before = events1.length;
const done2 = awaitEvent(created.session_id, (e) => e.seq >= before + 6);
await api.sendSessionInput(created.session_id, "and again");
const events2 = await timeout(done2, 20_000, "follow-up");
expect(events2.some((e) => e.type === "NO_MORE_ACTIONS"), "follow-up turn streams to NO_MORE_ACTIONS");

// Mock: no /dev/status probe (the badge's mock state comes from MOCK_MODE).
const devStatus = await api.devStatus();
expect(devStatus === null, "mock devStatus → null (no fake probe)");

// --- 2. fixture session A: pending approval in the inbox; deciding
// journals the decision and streams the continuation.
const SESS_A = "7c62c9a1-165b-4386-aebd-6647b35e472f";
const approvals = await api.listApprovals();
expect(approvals.some((a) => a.session.id === SESS_A), "fixture approval is in the inbox");
expect(approvals[0]!.event.type === "TOOL_APPROVAL_REQUIRED", "PendingApproval.event is the approval envelope");

const done3 = awaitEvent(SESS_A, (e) => e.type === "NO_MORE_ACTIONS");
await api.decideApproval(SESS_A, { approved: true, approver: "smoke" });
const events3 = await timeout(done3, 20_000, "continuation");
expect(
  events3.some((e) => e.type === "TOOL_APPROVAL_GRANTED") && events3.some((e) => e.type === "TOOL_RESULT"),
  "continuation journals decision + tool result",
);
const after = await api.listApprovals();
expect(!after.some((a) => a.session.id === SESS_A), "decided session leaves the inbox");
const sessA = await api.getSession(SESS_A);
expect(sessA?.status === "active", "status returns to active after the decision");

console.log("\nmock smoke green");
process.exit(0);
