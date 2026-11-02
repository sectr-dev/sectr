/*
  Manual smoke of src/lib/sessions-api.ts against a LIVE dev gateway.

  Start first:
    SECTR_FAKE=1 cargo run -p sectr-cli -- dev --path examples/openai-agents --no-open
  Then:
    cd dashboard && npx tsx scripts/smoke-sessions-api.mts
  Exits 0 on green. (Vitest setup lands with the UI-wiring slice; until
  then this script is the data layer's executable check.)
*/
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createSessionsApi, ApiError } from "../src/lib/transport/sessions-api";

const state = JSON.parse(
  readFileSync(join(import.meta.dirname, "../../examples/openai-agents/.agent/dev.json"), "utf8"),
);
const api = createSessionsApi({ baseUrl: state.gateway_url });

const fail = (m: string): never => {
  console.error(`✗ ${m}`);
  process.exit(1);
};
const expect = (cond: boolean, m: string) => {
  if (!cond) fail(m);
  console.log(`✓ ${m}`);
};

// Dev-only probe: a local gateway answers /dev/status (a prod server
// wouldn't have the route — this is the backend badge's local-vs-prod test).
const status = await api.devStatus();
expect(status !== null && typeof status.project === "string", "devStatus → local gateway identity");
expect(
  status!.gateway_port === Number(new URL(state.gateway_url).port),
  "devStatus reports the actual gateway port",
);

// Create + first turn: fake agent lookups the order, then requests approval
// for a refund → the stream must suspend on TOOL_APPROVAL_REQUIRED.
const created = await api.createSession({
  app: "support-agent",
  route: "/chat",
  input: "please refund order 42",
});
expect(typeof created.session_id === "string" && created.session_id.length === 36, "createSession → 201 {session_id}");

const events: { seq: number; type: string }[] = [];
const suspended = new Promise<void>((resolve, reject) => {
  const close = api.openSessionStream(created.session_id, {
    onEvent: (e) => {
      events.push({ seq: e.seq, type: e.type });
      if (e.type === "TOOL_APPROVAL_REQUIRED") {
        close();
        resolve();
      }
      if (e.type === "SESSION_ERROR") reject(new Error("session errored"));
    },
    onError: (err) => reject(err),
  });
});
const timeout = <T,>(p: Promise<T>, ms: number, what: string) =>
  Promise.race([p, new Promise<never>((_, rej) => setTimeout(() => rej(new Error(`${what} timed out`)), ms))]);
await timeout(suspended, 15_000, "stream");
expect(events.some((e) => e.type === "TOOL_APPROVAL_REQUIRED"), "stream replays journal and suspends on TOOL_APPROVAL_REQUIRED");

// 409: input while awaiting approval must be rejected.
let input409 = false;
try {
  await api.sendSessionInput(created.session_id, "hello?");
} catch (e) {
  input409 = e instanceof ApiError && e.status === 409;
}
expect(input409, "input while awaiting approval → ApiError 409");

// Approvals inbox sees it.
const approvals = await api.listApprovals();
expect(approvals.some((a) => a.session.id === created.session_id), "listApprovals → includes the pending session");
expect(approvals[0]!.event.type === "TOOL_APPROVAL_REQUIRED", "PendingApproval.event is the TOOL_APPROVAL_REQUIRED envelope");

// Session summary shows the derived status.
const mid = await api.getSession(created.session_id);
expect(mid?.status === "awaiting_approval", "getSession → status awaiting_approval (server-derived)");

// Decide → resume → turn completes.
await api.decideApproval(created.session_id, { approved: true, approver: "smoke" });
const done = new Promise<void>((resolve, reject) => {
  const close = api.openSessionStream(created.session_id, {
    onEvent: (e) => {
      if (e.type === "NO_MORE_ACTIONS") {
        close();
        resolve();
      }
      if (e.type === "SESSION_ERROR") reject(new Error("session errored"));
    },
    onError: (err) => reject(err),
  });
});
await timeout(done, 15_000, "resume");
expect(true, "decideApproval → resumed turn reaches NO_MORE_ACTIONS");

// Turn-based: after NO_MORE_ACTIONS the session is ACTIVE again (open for
// follow-ups) — 'ended' is for terminally closed sessions, not turn ends.
const after = await api.getSession(created.session_id);
expect(after?.status === "active", "after a completed turn → status active (open for follow-ups)");

const list = await api.listSessions({ app_id: "support-agent" });
expect(list.some((s) => s.id === created.session_id), "listSessions filters by app_id");

// 404 mapping: unknown session → undefined, not a throw.
const ghost = await api.getSession("00000000-0000-0000-0000-000000000000");
expect(ghost === undefined, "getSession(unknown) → undefined (404 mapped)");

console.log("\nsmoke green");
