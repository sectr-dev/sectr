/*
  Domain types for the dashboard shell (Phase 1.5 — mock data only).

  Event types mirror `schema/event.schema.json` (payload shapes verified
  against $defs). In Phase 7 these get generated from `schema/public-api.yaml`
  via openapi-typescript.
  Control-plane domain types (App, Deployment, Session) mirror the planned
  read APIs.
*/

// ---------------------------------------------------------------------------
// Journal events (schema/event.schema.json)
// ---------------------------------------------------------------------------

export type EventType =
  | "SESSION_CREATED"
  | "INVOCATION_STARTED"
  | "MESSAGE_START"
  | "MESSAGE_DELTA"
  | "MESSAGE_END"
  | "TOOL_CALL"
  | "TOOL_RESULT"
  | "TOOL_APPROVAL_REQUIRED"
  | "TOOL_APPROVAL_GRANTED"
  | "TOOL_APPROVAL_DENIED"
  | "TRANSCRIPT_COMPACTED"
  | "ERROR"
  | "SESSION_ERROR"
  | "NO_MORE_ACTIONS";

export interface EventEnvelope<P = Record<string, unknown>> {
  id: string; // uuidv7
  session_id: string;
  deployment_id: string;
  seq: number;
  type: EventType;
  payload: P;
  created_at: string; // RFC 3339
}

// Payload shapes (subset we render). `arguments`/`input`/`result` are opaque
// JSON per the contract.
export interface MessageStartPayload {
  role: string;
  message_id: string;
}
export interface MessageDeltaPayload {
  message_id: string;
  delta: string;
}
export interface MessageEndPayload {
  message_id: string;
}
export interface ToolCallPayload {
  tool_call_id: string;
  name: string;
  arguments: Record<string, unknown>;
}
export interface ToolResultPayload {
  tool_call_id: string;
  result: unknown;
}
export interface ToolApprovalRequiredPayload {
  tool_call_id: string;
  name: string;
  arguments: Record<string, unknown>;
  reason?: string;
}
export interface ToolApprovalDecisionPayload {
  tool_call_id: string;
  approver?: string;
}
export interface ErrorPayload {
  message: string;
  code?: string;
  transient?: boolean;
}
export interface SessionCreatedPayload {
  app_id: string;
  route: string;
}
export interface InvocationStartedPayload {
  invocation_id: string;
  route: string;
  deployment_id: string;
  reason:
    | { kind: "user_message"; input: string }
    | { kind: "approval_decision"; decision: { approved: boolean; approver?: string } };
}
export interface TranscriptCompactedPayload {
  replaces_up_to_seq: number;
  messages: Record<string, unknown>[];
}

export type SectrEvent =
  | EventEnvelope<MessageStartPayload>
  | EventEnvelope<MessageDeltaPayload>
  | EventEnvelope<MessageEndPayload>
  | EventEnvelope<ToolCallPayload>
  | EventEnvelope<ToolResultPayload>
  | EventEnvelope<ToolApprovalRequiredPayload>
  | EventEnvelope<ToolApprovalDecisionPayload>
  | EventEnvelope<ErrorPayload>
  | EventEnvelope<SessionCreatedPayload>
  | EventEnvelope<InvocationStartedPayload>
  | EventEnvelope<TranscriptCompactedPayload>
  | EventEnvelope<Record<string, never>>;

// ---------------------------------------------------------------------------
// Control-plane domain (planned public API shapes)
// ---------------------------------------------------------------------------

export type DeploymentStatus = "building" | "active" | "failed" | "retired";

export interface Deployment {
  id: string;
  app_id: string;
  version: number;
  status: DeploymentStatus;
  /** null on the dev gateway — a dev deployment is a running process, not an artifact with a birthdate */
  created_at: string | null;
  commit_message?: string;
  /** Build error, present on failed deployments. */
  error?: string;
  active: boolean;
}

export type SessionStatus = "active" | "awaiting_approval" | "ended" | "error";

export interface Session {
  id: string;
  app_id: string;
  app_name: string;
  route: string;
  /**
   * Where the session came from. Chat input on the console is restricted to
   * playground-originated sessions — the developer can inject messages into
   * sessions they started here, but not into sessions their end users
   * created via the invoke API. (A dev/prod deployment distinction may
   * replace this later; not MVP.)
   */
  origin: "playground" | "api";
  /** the deployment this session is pinned to */
  deployment_id: string;
  /** owner: the end user whose browser token created the session;
   *  absent for backend/platform-created sessions */
  external_user_id?: string;
  status: SessionStatus;
  created_at: string;
  last_event_at: string;
  /** number of journal events (for list views) */
  event_count: number;
  /** server-computed display label (today the first user message,
   *  truncated); absent when there's no text to show */
  title?: string;
}

export interface AppRoute {
  path: string;
  description: string;
}

export interface App {
  id: string;
  name: string;
  /** The URL/addressable identity (deploy-created; secrets CLI addressing). */
  slug: string;
  /** null on the dev gateway when the handler carries no framework marker */
  framework: "openai-agents" | "langgraph" | "pydantic-ai" | "claude-code" | null;
  description: string;
  /** null on the dev gateway — the app runs on localhost, not *.sectr.app */
  url: string | null; // *.sectr.app
  routes: AppRoute[];
  sessions_count: number;
  latest_deployment: Deployment;
  deployments: Deployment[];
}

export interface PendingApproval {
  session: Session;
  event: EventEnvelope<ToolApprovalRequiredPayload>;
}
