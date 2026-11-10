/*
  Fixture SOURCE for the mock transports (`transport/mock-sessions-api.ts`,
  `transport/mock-control-plane.ts`) — modeled on
  schema/fixtures/valid/event/*.json so the UI stays contract-accurate.
  Never imported by components: they go through api.ts. UI-shaped (string
  deltas); the mock sessions transport converts to contract envelopes.
*/

import type { App, Deployment, EventType, SectrEvent, Session, WakeSummary } from "./types";

// ---------------------------------------------------------------------------
// id + time helpers
// ---------------------------------------------------------------------------

let uuidCounter = 0;
function uuid(): string {
  // uuidv7-shaped, monotonic within this module — good enough for mocks.
  uuidCounter += 1;
  const ts = Date.now();
  const hex = ts.toString(16).padStart(12, "0");
  const rnd = (uuidCounter * 2654435761) % 0xffffffff;
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-7${(rnd & 0xfff)
    .toString(16)
    .padStart(3, "0")}-8${(rnd & 0xfff)
    .toString(16)
    .padStart(3, "0")}-${rnd.toString(16).padStart(12, "0")}`.slice(0, 36);
}

/** minutes ago + offset ms */
function ago(minutes: number, offsetMs = 0): string {
  return new Date(Date.now() - minutes * 60_000 - offsetMs).toISOString();
}

// ---------------------------------------------------------------------------
// Event builders (payload shapes mirror schema/event.schema.json $defs)
// ---------------------------------------------------------------------------

type EventSpec =
  | { t: "SESSION_CREATED"; app_id: string; route: string }
  | {
      t: "INVOCATION_STARTED";
      invocation_id: string;
      route: string;
      deployment_id: string;
      reason: Record<string, unknown>;
    }
  | { t: "MESSAGE_START"; message_id: string; role?: string }
  | { t: "MESSAGE_DELTA"; message_id: string; delta: string }
  | { t: "MESSAGE_END"; message_id: string }
  | { t: "TOOL_CALL"; tool_call_id: string; name: string; arguments: Record<string, unknown> }
  | { t: "TOOL_RESULT"; tool_call_id: string; result: unknown }
  | {
      t: "TOOL_APPROVAL_REQUIRED";
      tool_call_id: string;
      name: string;
      arguments: Record<string, unknown>;
      reason?: string;
    }
  | { t: "TOOL_APPROVAL_GRANTED"; tool_call_id: string; approver?: string }
  | { t: "TOOL_APPROVAL_DENIED"; tool_call_id: string; approver?: string }
  | { t: "TRANSCRIPT_COMPACTED"; replaces_up_to_seq: number; messages: Record<string, unknown>[] }
  | { t: "ERROR"; message: string; code?: string; transient?: boolean }
  | { t: "SESSION_ERROR"; message: string }
  | { t: "NO_MORE_ACTIONS" };

function buildEvents(
  sessionId: string,
  deploymentId: string,
  specs: EventSpec[],
  startMinutesAgo: number,
  /** Stretch the events over this many minutes (default: seconds apart). Real
   *  sessions idle, pause and resume, so list views show Started and Last
   *  event differing. */
  spanMinutes?: number,
): SectrEvent[] {
  const gaps = specs.map((_, i) => 700 + (((i + 1) * 977) % 2300)); // deterministic jitter, 0.7–3s apart
  const total = gaps.reduce((a, b) => a + b, 0);
  const scale = spanMinutes ? (spanMinutes * 60_000) / total : 1;
  let seq = 0;
  let ms = 0;
  return specs.map((spec, i) => {
    seq += 1;
    ms += gaps[i] * scale;
    const payload: Record<string, unknown> = { ...spec };
    delete payload.t;
    return {
      id: uuid(),
      session_id: sessionId,
      deployment_id: deploymentId,
      seq,
      type: spec.t as EventType,
      payload,
      created_at: ago(startMinutesAgo, -ms),
    } as SectrEvent;
  });
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

const SUPPORT_DEPLOYMENT = "dep-support-0007";

// — Session A: awaiting approval (the flagship inspector session) —
const SESS_A_ID = "7c62c9a1-165b-4386-aebd-6647b35e472f";
const sessAEvents = buildEvents(SESS_A_ID, SUPPORT_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-support", route: "/customer-support" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-a1",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "Where is my order A-1234? It was supposed to arrive Tuesday." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "Let me look up the status of order A-1234 for you." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "TOOL_CALL", tool_call_id: "call_01", name: "lookup_order", arguments: { order_id: "A-1234" } },
  {
    t: "TOOL_APPROVAL_REQUIRED",
    tool_call_id: "call_01",
    name: "lookup_order",
    arguments: { order_id: "A-1234" },
    reason: "Order lookup requires manager approval (PII access)",
  },
], 14, 9);

// Scripted continuations for Session A after an approval decision (played live
// by the inspector simulation).
export const sessAEventsPart2 = buildEvents(SESS_A_ID, SUPPORT_DEPLOYMENT, [
  { t: "TOOL_APPROVAL_GRANTED", tool_call_id: "call_01", approver: "you@company.com" },
  {
    t: "TOOL_RESULT",
    tool_call_id: "call_01",
    result: {
      order_id: "A-1234",
      status: "in_transit",
      carrier: "UPS",
      tracking: "1Z999AA10123456784",
      eta: "Thursday by 8pm",
      items: ["Wireless keyboard — qty 1"],
    },
  },
  { t: "MESSAGE_START", message_id: "msg_02" },
  { t: "MESSAGE_DELTA", message_id: "msg_02", delta: "Good news — order A-1234 is in transit with UPS (tracking 1Z999AA10123456784) and is now expected Thursday by 8pm, two days later than the original estimate. I'm sorry about the delay; your account has been credited $5 for the inconvenience. Anything else?" },
  { t: "MESSAGE_END", message_id: "msg_02" },
  { t: "NO_MORE_ACTIONS" },
], 13);

export const sessAEventsPart3 = buildEvents(SESS_A_ID, SUPPORT_DEPLOYMENT, [
  { t: "TOOL_APPROVAL_DENIED", tool_call_id: "call_01", approver: "you@company.com" },
  { t: "MESSAGE_START", message_id: "msg_02" },
  { t: "MESSAGE_DELTA", message_id: "msg_02", delta: "I wasn't able to look up that order right now — order-level details need a manager's approval and the request was declined. You can also track order A-1234 directly from your account's Orders page, or I can escalate this to a human teammate." },
  { t: "MESSAGE_END", message_id: "msg_02" },
  { t: "NO_MORE_ACTIONS" },
], 13);

// — Session B: active, live-streaming in the inspector —
export const SESS_B_ID = "95d6d7ac-d4e1-4f31-86ea-233a57996bed";
const sessBEventsPart1 = buildEvents(SESS_B_ID, SUPPORT_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-support", route: "/customer-support" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-b1",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "Hi — I was charged twice for my subscription this month." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "I'm sorry about the duplicate charge — let me check your billing history right away." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "TOOL_CALL", tool_call_id: "call_01", name: "search_knowledge_base", arguments: { query: "duplicate charge refund policy" } },
  {
    t: "TOOL_RESULT",
    tool_call_id: "call_01",
    result: { answer: "Duplicate charges are eligible for full refund within 30 days. Approval required above $50." },
  },
  { t: "MESSAGE_START", message_id: "msg_02" },
  { t: "MESSAGE_DELTA", message_id: "msg_02", delta: "You're right — account 8841 was charged twice on the 12th ($29 both times). I can refund the duplicate now." },
  { t: "MESSAGE_END", message_id: "msg_02" },
  {
    t: "TRANSCRIPT_COMPACTED",
    replaces_up_to_seq: 8,
    messages: [
      { role: "user", content: "Hi — I was charged twice for my subscription this month." },
      { role: "assistant", content: "Account 8841 was charged twice on the 12th ($29 each). Duplicate-charge refund is possible within 30 days." },
    ],
  },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-b2",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "Yes please, refund the duplicate." },
  },
  { t: "MESSAGE_START", message_id: "msg_03" },
  { t: "MESSAGE_DELTA", message_id: "msg_03", delta: "Sure — issuing the refund for $29.00 to your original payment method." },
  { t: "MESSAGE_END", message_id: "msg_03" },
  { t: "TOOL_CALL", tool_call_id: "call_02", name: "issue_refund", arguments: { account_id: "8841", amount_usd: 29.0, charge_id: "ch_2Xk9" } },
  {
    t: "TOOL_APPROVAL_REQUIRED",
    tool_call_id: "call_02",
    name: "issue_refund",
    arguments: { account_id: "8841", amount_usd: 29.0, charge_id: "ch_2Xk9" },
    reason: "Refunds require human approval",
  },
], 3);

// Scripted continuation after approve — played by the inspector's live simulation.
export const sessBEventsPart2 = buildEvents(SESS_B_ID, SUPPORT_DEPLOYMENT, [
  { t: "TOOL_APPROVAL_GRANTED", tool_call_id: "call_02", approver: "you@company.com" },
  {
    t: "TOOL_RESULT",
    tool_call_id: "call_02",
    result: { refunded: true, amount_usd: 29.0, ETA: "3–5 business days" },
  },
  { t: "MESSAGE_START", message_id: "msg_04" },
  { t: "MESSAGE_DELTA", message_id: "msg_04", delta: "Done — $29.00 is on its way back to your card, arriving in 3–5 business days. Anything else I can help with?" },
  { t: "MESSAGE_END", message_id: "msg_04" },
  { t: "NO_MORE_ACTIONS" },
], 2);

// Scripted continuation after deny.
export const sessBEventsPart3 = buildEvents(SESS_B_ID, SUPPORT_DEPLOYMENT, [
  { t: "TOOL_APPROVAL_DENIED", tool_call_id: "call_02", approver: "you@company.com" },
  { t: "MESSAGE_START", message_id: "msg_04" },
  { t: "MESSAGE_DELTA", message_id: "msg_04", delta: "Understood — I won't issue the refund. I've escalated this charge to our billing team for manual review; you'll hear back within one business day." },
  { t: "MESSAGE_END", message_id: "msg_04" },
  { t: "NO_MORE_ACTIONS" },
], 2);

// — Session C: ended, happy path with approval granted —
const sessCEvents = buildEvents("dffb03f5-2e6f-4fea-9fb3-d0b897b7656d", SUPPORT_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-support", route: "/customer-support" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-c1",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "Can you upgrade my plan to Team?" },
  },
  // Tool-call-only response: an empty START/END pair, as the adapters journal.
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "TOOL_CALL", tool_call_id: "call_01", name: "change_plan", arguments: { account_id: "5520", plan: "team" } },
  { t: "TOOL_APPROVAL_REQUIRED", tool_call_id: "call_01", name: "change_plan", arguments: { account_id: "5520", plan: "team" }, reason: "Plan changes require approval" },
  { t: "TOOL_APPROVAL_GRANTED", tool_call_id: "call_01", approver: "ops@company.com" },
  { t: "TOOL_RESULT", tool_call_id: "call_01", result: { plan: "team", effective: "immediately" } },
  { t: "MESSAGE_START", message_id: "msg_02" },
  { t: "MESSAGE_DELTA", message_id: "msg_02", delta: "All set — your workspace is on the Team plan as of now. You'll see the new seats on the next invoice." },
  { t: "MESSAGE_END", message_id: "msg_02" },
  { t: "NO_MORE_ACTIONS" },
], 96, 14);

// — Session D: ended, approval denied → agent adapts —
const sessDEvents = buildEvents("91a8fa09-1e82-4e85-bdd7-a9e62b36c5e9", SUPPORT_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-support", route: "/customer-support" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-d1",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "Delete my account and all data, GDPR request." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "I can submit that erasure request for account 9032. Note this is irreversible." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "TOOL_CALL", tool_call_id: "call_01", name: "delete_account", arguments: { account_id: "9032" } },
  { t: "TOOL_APPROVAL_REQUIRED", tool_call_id: "call_01", name: "delete_account", arguments: { account_id: "9032" }, reason: "Irreversible account deletion — always escalate" },
  { t: "TOOL_APPROVAL_DENIED", tool_call_id: "call_01", approver: "legal@company.com" },
  { t: "MESSAGE_START", message_id: "msg_02" },
  { t: "MESSAGE_DELTA", message_id: "msg_02", delta: "The deletion request was held by our compliance team (they may need to verify identity first). I've emailed you the verification steps to complete the request." },
  { t: "MESSAGE_END", message_id: "msg_02" },
  { t: "NO_MORE_ACTIONS" },
], 190, 125);

// — Session E: error —
const sessEEvents = buildEvents("db814059-43a3-4399-a9ca-084766f2361c", SUPPORT_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-support", route: "/customer-support" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-e1",
    route: "/customer-support",
    deployment_id: SUPPORT_DEPLOYMENT,
    reason: { kind: "user_message", input: "What's your SLA for enterprise plans?" },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "Our enterprise SLA includes a 99.9% uptime commitment and a" },
  { t: "ERROR", message: "upstream request timeout after 30s", code: "model_timeout", transient: true },
], 410, 5);

// — research-agent session: awaiting approval —
const sessRId = "6771f464-27f9-4a07-b6fa-334f8e0f4658";
const sessREvents = buildEvents(sessRId, "dep-research-0001", [
  { t: "SESSION_CREATED", app_id: "app-research", route: "/research" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-r1",
    route: "/research",
    deployment_id: "dep-research-0001",
    reason: { kind: "user_message", input: "Competitive landscape for AI agent hosting platforms — pricing, positioning, funding." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "I'll research the landscape across three angles: pricing pages, positioning, and recent funding rounds." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "TOOL_CALL", tool_call_id: "call_01", name: "web_search", arguments: { query: "AI agent hosting platform pricing 2026" } },
  { t: "TOOL_RESULT", tool_call_id: "call_01", result: { results: ["…9 results collected"] } },
  { t: "TOOL_CALL", tool_call_id: "call_02", name: "fetch_url", arguments: { url: "https://competitor.example.com/pricing" } },
  { t: "TOOL_APPROVAL_REQUIRED", tool_call_id: "call_02", name: "fetch_url", arguments: { url: "https://competitor.example.com/pricing" }, reason: "Domain not on the allowlist" },
], 55, 6);

// — code-reviewer sessions —
const sessV1Events = buildEvents("b5b6fcc0-f3e2-4fb3-a532-bbe3bda75fae", "dep-review-0001", [
  { t: "SESSION_CREATED", app_id: "app-review", route: "/review" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-v1",
    route: "/review",
    deployment_id: "dep-review-0001",
    reason: { kind: "user_message", input: "Review PR #482 — focus on the auth changes." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "Reviewing PR #482 (14 files, +412/−96). Auth changes look mostly sound; found 2 issues worth blocking." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "NO_MORE_ACTIONS" },
], 26);
const sessV2Events = buildEvents("d20a9332-3b00-4df8-8a8b-e7621391b5d2", "dep-review-0001", [
  { t: "SESSION_CREATED", app_id: "app-review", route: "/review" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-v2",
    route: "/review",
    deployment_id: "dep-review-0001",
    reason: { kind: "user_message", input: "Review PR #483." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "PR #483 is a clean refactor — no blocking issues. Left 3 nit comments." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "NO_MORE_ACTIONS" },
], 75);

// — triage-agent: runs on the customer's own Lambda (an endpoint
// deployment). One session's turn never started: the function URL refused
// the wake.
const TRIAGE_DEPLOYMENT = "dep-triage-0002";
const TRIAGE_URL = "https://k7xq2b4pd3.lambda-url.us-east-1.on.aws/";
const sessT1Id = "3f0c8e52-9d1a-4b7e-8c35-6a2f1e9d4b70";
const sessT1Events = buildEvents(sessT1Id, TRIAGE_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-triage", route: "/triage" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-t1",
    route: "/triage",
    deployment_id: TRIAGE_DEPLOYMENT,
    reason: { kind: "user_message", input: "New ticket: checkout page returns 502 for EU customers." },
  },
  {
    t: "SESSION_ERROR",
    message: "endpoint wake failed: k7xq2b4pd3.lambda-url.us-east-1.on.aws answered HTTP 403: {\"Message\":\"Forbidden. For troubleshooting Function URL authorization issues, see: https://docs.aws.amazon.com/lambda/latest/dg/urls-auth.html\"}",
  },
], 18);
const sessT2Id = "a41d7b39-2c85-4f60-9e1b-7d3c5a8f2e16";
const sessT2Events = buildEvents(sessT2Id, TRIAGE_DEPLOYMENT, [
  { t: "SESSION_CREATED", app_id: "app-triage", route: "/triage" },
  {
    t: "INVOCATION_STARTED",
    invocation_id: "inv-t2",
    route: "/triage",
    deployment_id: TRIAGE_DEPLOYMENT,
    reason: { kind: "user_message", input: "New ticket: password reset emails arrive twice." },
  },
  { t: "MESSAGE_START", message_id: "msg_01" },
  { t: "MESSAGE_DELTA", message_id: "msg_01", delta: "Likely a retry on the mail queue: the reset job enqueues before the transaction commits. Labeled `auth`, priority P3." },
  { t: "MESSAGE_END", message_id: "msg_01" },
  { t: "NO_MORE_ACTIONS" },
], 95);

/** The mock wakes summary per deployment (GET /deployments/{id}/wakes). */
export const mockWakes: Record<string, WakeSummary> = {
  [TRIAGE_DEPLOYMENT]: {
    // One turn in each of its two sessions; the first failed to wake.
    turns: 2,
    failed: 1,
    latest_failure: {
      message: (sessT1Events[2].payload as { message: string }).message,
      session_id: sessT1Id,
      created_at: sessT1Events[2].created_at,
    },
  },
};

// ---------------------------------------------------------------------------
// Assembled domain objects
// ---------------------------------------------------------------------------

function deployment(
  id: string,
  appId: string,
  version: number,
  status: Deployment["status"],
  minutesAgo: number,
  commit_message?: string,
  active = false,
  error?: string,
): Deployment {
  return {
    id,
    app_id: appId,
    version,
    status,
    created_at: ago(minutesAgo),
    commit_message,
    error,
    active,
  };
}

export const mockApps: App[] = [
  {
    id: "app-support",
    name: "support-copilot",
    slug: "support-copilot",
    framework: "openai-agents",
    description: "Customer support agent with human-gated refunds and account changes.",
    url: "support-copilot.sectr.app",
    routes: [
      { path: "/customer-support", description: "Main support conversation" },
      { path: "/refund-policy", description: "Refund policy Q&A (read-only tools)" },
    ],
    sessions_count: 5,
    latest_deployment: deployment("dep-support-0007", "app-support", 7, "active", 2880, "feat: compaction for long billing threads", true),
    deployments: [
      deployment("dep-support-0007", "app-support", 7, "active", 2880, "feat: compaction for long billing threads", true),
      deployment("dep-support-0006", "app-support", 6, "active", 5760, "fix: clamp refund ceiling to $50"),
      deployment("dep-support-0005", "app-support", 5, "failed", 8640, "fix: tool schema for issue_refund", false, "build failed: tool `issue_refund` has an invalid JSON schema\n  at agents/refunds.py:42\n  'amount' must declare a type"),
      deployment("dep-support-0004", "app-support", 4, "retired", 14400, "feat: approval gating on plan changes"),
    ],
  },
  {
    id: "app-research",
    name: "research-agent",
    slug: "research-agent",
    framework: "langgraph",
    description: "Deep-research agent: multi-step web research with cited reports.",
    url: "research-agent.sectr.app",
    routes: [{ path: "/research", description: "Research brief generator" }],
    sessions_count: 1,
    latest_deployment: deployment("dep-research-0002", "app-research", 2, "building", 22, "wip: langgraph compaction capture"),
    deployments: [
      deployment("dep-research-0002", "app-research", 2, "building", 22, "wip: langgraph compaction capture"),
      deployment("dep-research-0001", "app-research", 1, "active", 4320, "init", true),
    ],
  },
  {
    id: "app-review",
    name: "code-reviewer",
    slug: "code-reviewer",
    framework: "claude-code",
    description: "PR review agent powered by Claude Code with repo access.",
    url: "code-reviewer.sectr.app",
    routes: [{ path: "/review", description: "Review a pull request" }],
    sessions_count: 2,
    latest_deployment: deployment("dep-review-0001", "app-review", 3, "active", 10080, "chore: bump base image", true),
    deployments: [
      deployment("dep-review-0001", "app-review", 3, "active", 10080, "chore: bump base image", true),
      deployment("dep-review-0000", "app-review", 2, "active", 20160, "feat: stricter auth-review checklist"),
    ],
  },
  {
    // Freshly deployed, nobody has talked to it yet: the empty states.
    id: "app-onboarding",
    name: "onboarding-bot",
    slug: "onboarding-bot",
    framework: "pydantic-ai",
    description: "Walks new teammates through account setup.",
    url: "onboarding-bot.sectr.app",
    routes: [{ path: "/onboard", description: "Guided account setup" }],
    sessions_count: 0,
    latest_deployment: deployment("dep-onboarding-0001", "app-onboarding", 1, "active", 35, undefined, true),
    deployments: [deployment("dep-onboarding-0001", "app-onboarding", 1, "active", 35, undefined, true)],
  },
  {
    // Runs on the customer's own Lambda: an endpoint deployment.
    id: "app-triage",
    name: "triage-agent",
    slug: "triage-agent",
    framework: "openai-agents",
    description: "Labels and routes new support tickets. Runs on our own AWS Lambda.",
    url: null,
    routes: [{ path: "/triage", description: "Triage a new ticket" }],
    sessions_count: 2,
    latest_deployment: endpointDeployment(TRIAGE_DEPLOYMENT, 2, "active", 240, TRIAGE_URL, true),
    deployments: [
      endpointDeployment(TRIAGE_DEPLOYMENT, 2, "active", 240, TRIAGE_URL, true),
      endpointDeployment("dep-triage-0001", 1, "retired", 4320, TRIAGE_URL),
    ],
  },
];

function endpointDeployment(
  id: string,
  version: number,
  status: Deployment["status"],
  minutesAgo: number,
  endpointUrl: string,
  active = false,
): Deployment {
  return { ...deployment(id, "app-triage", version, status, minutesAgo, undefined, active), endpoint_url: endpointUrl };
}

function session(
  id: string,
  app: App,
  route: string,
  status: Session["status"],
  events: SectrEvent[],
  origin: Session["origin"] = "api",
): Session {
  return {
    id,
    app_id: app.id,
    app_name: app.name,
    route,
    status,
    origin,
    deployment_id: events[0]?.deployment_id ?? app.latest_deployment.id,
    created_at: events[0]?.created_at ?? ago(60),
    last_event_at: events[events.length - 1]?.created_at ?? ago(60),
    event_count: events.length,
  };
}

export const mockSessions: Session[] = [
  session(SESS_A_ID, mockApps[0], "/customer-support", "awaiting_approval", sessAEvents),
  session(SESS_B_ID, mockApps[0], "/customer-support", "active", sessBEventsPart1, "playground"),
  session("dffb03f5-2e6f-4fea-9fb3-d0b897b7656d", mockApps[0], "/customer-support", "ended", sessCEvents),
  session("91a8fa09-1e82-4e85-bdd7-a9e62b36c5e9", mockApps[0], "/customer-support", "ended", sessDEvents),
  session("db814059-43a3-4399-a9ca-084766f2361c", mockApps[0], "/customer-support", "error", sessEEvents),
  session(sessRId, mockApps[1], "/research", "awaiting_approval", sessREvents),
  session("b5b6fcc0-f3e2-4fb3-a532-bbe3bda75fae", mockApps[2], "/review", "ended", sessV1Events),
  session("d20a9332-3b00-4df8-8a8b-e7621391b5d2", mockApps[2], "/review", "ended", sessV2Events),
  session(sessT1Id, mockApps[4], "/triage", "error", sessT1Events),
  session(sessT2Id, mockApps[4], "/triage", "ended", sessT2Events),
];

export const mockEventsBySession: Record<string, SectrEvent[]> = {
  [SESS_A_ID]: sessAEvents,
  [SESS_B_ID]: sessBEventsPart1,
  "dffb03f5-2e6f-4fea-9fb3-d0b897b7656d": sessCEvents,
  "91a8fa09-1e82-4e85-bdd7-a9e62b36c5e9": sessDEvents,
  "db814059-43a3-4399-a9ca-084766f2361c": sessEEvents,
  [sessRId]: sessREvents,
  "b5b6fcc0-f3e2-4fb3-a532-bbe3bda75fae": sessV1Events,
  "d20a9332-3b00-4df8-8a8b-e7621391b5d2": sessV2Events,
  [sessT1Id]: sessT1Events,
  [sessT2Id]: sessT2Events,
};

export function getSessionContinuation(
  id: string,
  approved: boolean,
): SectrEvent[] {
  if (id === SESS_A_ID) return approved ? sessAEventsPart2 : sessAEventsPart3;
  if (id === SESS_B_ID) return approved ? sessBEventsPart2 : sessBEventsPart3;
  return [];
}

/**
 * Scripted assistant reply for the playground chat (mock of a journal turn:
 * assistant message + NO_MORE_ACTIONS). `startSeq` continues the session's
 * seq numbering — the journal writer owns seq in production.
 */
export function getChatReply(sessionId: string, deploymentId: string, startSeq: number): SectrEvent[] {
  const messageId = crypto.randomUUID();
  const text =
    "Got it — I've pulled up the account and checked the current state. " +
    "Based on what I'm seeing, the duplicate charge from the 12th is still " +
    "pending reversal on the processor side. I can issue the refund for " +
    "$29.00 to the original payment method now, or you can ask me to " +
    "escalate to a human specialist — your call.";
  const deltas = [text.slice(0, 96), text.slice(96, 200), text.slice(200)];
  let seq = startSeq;
  const base = {
    session_id: sessionId,
    deployment_id: deploymentId,
    created_at: new Date().toISOString(),
  };
  const mk = (type: EventType, payload: Record<string, unknown>): SectrEvent =>
    ({ id: crypto.randomUUID(), seq: ++seq, type, payload, ...base }) as SectrEvent;
  return [
    mk("MESSAGE_START", { role: "assistant", message_id: messageId }),
    ...deltas.map((delta) => mk("MESSAGE_DELTA", { message_id: messageId, delta })),
    mk("MESSAGE_END", { message_id: messageId }),
    mk("NO_MORE_ACTIONS", {}),
  ];
}
