/*
  Transport-layer interfaces — the contract between api.ts's dispatch and
  each backend implementation. BOTH `sessions-api.ts` (real) and
  `mock-sessions-api.ts` (fixtures + scripted streaming) implement
  `SessionsApi`; api.ts re-exports one or the other based on MOCK_MODE, so
  nothing outside this directory knows which is live.

  The interface speaks CONTRACT types (generated from schema/sessions-api.yaml
  — see ./generated). UI-shaping (app_name joins, event projections) happens
  above the transport, in hooks/components — the rule: the server
  returns canonical rows, the UI joins.

  All methods are async (the mock resolves immediately; scripted streaming
  arrives through openSessionStream exactly as live events do).
*/

import type { components } from "./generated/sessions-api";
import type { components as devComponents } from "./generated/dev-api";

export type Session = components["schemas"]["Session"];
export type SessionStatus = components["schemas"]["SessionStatus"];
export type NewSessionRequest = components["schemas"]["NewSessionRequest"];
export type SessionCreated = components["schemas"]["SessionCreated"];
export type ApprovalDecision = components["schemas"]["ApprovalDecision"];
export type PendingApproval = components["schemas"]["PendingApproval"];
/** One journal event envelope — the shape of every SSE frame's `data:`. */
export type JournalEvent = components["schemas"]["event.schema"];
/** Dev-only: GET /dev/status (dev-api.yaml — a prod server has no such
 *  route, which makes it the local-vs-prod probe). */
export type DevStatus = devComponents["schemas"]["DevStatus"];

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: string,
  ) {
    super(`${status}: ${body || "(no body)"}`);
    this.name = "ApiError";
  }
}

export interface ListSessionsParams {
  app_id?: string;
  status?: SessionStatus;
  limit?: number;
  /** RFC 3339; keyset pagination. */
  created_before?: string;
}

export interface SessionStreamOptions {
  /** First-connect resume point (an event id); auto-reconnect uses headers. */
  lastEventId?: string;
  onEvent: (event: JournalEvent) => void;
  /** Connection dropped or errored. EventSource reconnects on its own. */
  onError?: (err: unknown) => void;
}

export interface SessionsApi {
  /** POST /sessions — create a session; its first turn starts immediately. */
  createSession(req: NewSessionRequest): Promise<SessionCreated>;
  /** GET /sessions — sessions tables + approvals inbox source. */
  listSessions(params?: ListSessionsParams): Promise<Session[]>;
  /** GET /sessions/{sessionId} — 404 → undefined. */
  getSession(sessionId: string): Promise<Session | undefined>;
  /** POST .../input — a follow-up turn; 202 = scheduled. */
  sendSessionInput(sessionId: string, input: string): Promise<void>;
  /** POST .../approval — decide a pending approval; 202 = journaled. */
  decideApproval(sessionId: string, decision: ApprovalDecision): Promise<void>;
  /** GET /approvals — pending approvals across all apps (admin inbox). */
  listApprovals(): Promise<PendingApproval[]>;
  /**
   * GET /dev/status — DEV-ONLY (dev-api.yaml; a prod server has no such
   * route): local-gateway identity for the backend badge. Any failure →
   * null; the mock returns null too (its badge state comes from
   * MOCK_MODE, not a fake probe).
   */
  devStatus(): Promise<DevStatus | null>;
  /**
   * GET /healthz — the PROD badge's liveness probe: "prod server"
   * becomes a real state (reachable) vs "server offline". Any failure →
   * false; the mock returns false (its badge comes from MOCK_MODE).
   */
  prodHealth(): Promise<boolean>;
  /**
   * GET /sessions/{sessionId}/events (text/event-stream) — the journal
   * tail: full replay from seq 0, then live. Never resolves; call the
   * returned close() to stop.
   */
  openSessionStream(sessionId: string, opts: SessionStreamOptions): () => void;
}

/**
 * GET /me — the signed-in identity: the org (org-per-person, so there is
 * no separate `user`).
 */
export interface Me {
  org: { id: string; name: string };
  /** The server's dashboard origin. The dev gateway and mock don't set it. */
  dashboard_url?: string;
}

/**
 * Control-plane surface (apps, deployments, auth, settings). Three
 * implementations: mock (fixtures + a local store), dev (honest no-ops —
 * the gateway has no control plane), prod (the real server).
 */
export interface ControlPlaneApi {
  /** GET /me — the signed-in identity (also validates the stored key). */
  me(): Promise<Me>;
  listApps(): Promise<App[]>;
  getApp(appId: string): Promise<App | undefined>;
  listDeployments(appId: string): Promise<Deployment[]>;
  activateDeployment(deploymentId: string): Promise<void>;
  /** GET /deployments/{id}/wakes — absent on the dev gateway, which has no
   *  endpoint deployments. */
  getDeploymentWakes?(deploymentId: string, limit?: number): Promise<WakeSummary>;
  /**
   * Platform-key login (no cookies): the key exchanged
   * from a Clerk session. The impl validates
   * the key against the server and persists it (browser-only storage); a
   * rejected key throws ApiError(401).
   */
  login(key: string): Promise<void>;
  logout(): Promise<void>;
  // -- settings (real server endpoints; public-api.yaml) ------------------

  /** GET /api-keys — metadata only (hashes; nothing to display). */
  listApiKeys(): Promise<ApiKeyMeta[]>;
  /** POST /api-keys — the full value is returned EXACTLY ONCE. */
  createApiKey(
    name: string,
    kind: ApiKeyKind,
    appId?: string,
  ): Promise<ApiKeyMeta & { key: string }>;
  /** DELETE /api-keys/{id} — idempotent. */
  revokeApiKey(keyId: string): Promise<void>;
  // -- CLI browser login (public-api.yaml /auth/cli/requests) ------

  /** GET /auth/cli/requests/{code} — 404 if unknown or expired. */
  getCliLogin(userCode: string): Promise<CliLoginRequest>;
  /** POST …/approve — 404 unknown/expired, 409 already decided. */
  approveCliLogin(userCode: string): Promise<void>;
  /** POST …/deny — same errors as approve. */
  denyCliLogin(userCode: string): Promise<void>;
  /** GET /secrets?app=<slug> — names + timestamps only (write-only contract). */
  listSecrets(appSlug: string): Promise<SecretMeta[]>;
  /** PUT /secrets/{key}?app=<slug> — create or replace; never echoed. */
  setSecret(appSlug: string, key: string, value: string): Promise<void>;
  /** DELETE /secrets/{key}?app=<slug> — idempotent. */
  deleteSecret(appSlug: string, key: string): Promise<void>;
}

/** A pending `sectr auth login` request, as the approval page shows it. */
export interface CliLoginRequest {
  user_code: string;
  /** The CLI machine's hostname (display only). */
  client_name: string;
  created_at: string;
  expires_at: string;
  status: "pending" | "approved" | "denied" | "consumed";
}

/** public-api.yaml ApiKeyMeta — metadata only; the store keeps hashes. */
export interface ApiKeyMeta {
  id: string;
  name: string;
  kind: ApiKeyKind;
  app_id: string | null;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
  /** Hard expiry (dashboard-session keys); null/absent = never. */
  expires_at?: string | null;
}

export type ApiKeyKind = "platform" | "invocation";

/** public-api.yaml SecretMeta — no value field exists, by contract. */
export interface SecretMeta {
  name: string;
  created_at: string;
  /** When the value was last replaced; null if never. */
  updated_at: string | null;
}

// UI-view types (control-plane domain is hand-mirrored until its contract
// exists; see src/lib/types.ts)
import type { App, Deployment, WakeSummary } from "../types";
export type { App, Deployment };
