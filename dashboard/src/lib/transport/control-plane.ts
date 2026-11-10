/*
  PROD implementation of `ControlPlaneApi` — the real
  sectr-server control plane, reached through the same-origin rewrite
  (`/api` → SECTR_SERVER_URL, see next.config.ts).

  Auth is a platform key (no cookies). The full `skp_…`
  value lives in localStorage (`sectr.platformKey`) and rides every request
  as `Authorization: Bearer …` via the credential-provider seam. It is
  swapped in for a Clerk session (`exchangeClerkSession`). That is why the prod control
  plane — like the dev one — is consumed CLIENT-side only: server
  components cannot reach browser storage. (Server-side control-plane
  rendering returns when the server gains a real session mechanism.)

  Shapes are hand-mirrored from schema/public-api.yaml (AppSummary /
  AppDetail / Deployment) — the codegen covers sessions-api.yaml +
  dev-api.yaml; public-api types stay hand-mirrored until they're
  generated too.
*/

import type { App, Deployment, WakeSummary } from "../types";
import { fetchWithAuthRetry } from "./auth-refresh";
import {
  ApiError,
  type ApiKeyKind,
  type ApiKeyMeta,
  type CliLoginRequest,
  type ControlPlaneApi,
  type Me,
  type SecretMeta,
} from "./types";

/** localStorage key for the platform API key (full value, browser-only). */
const STORAGE_KEY = "sectr.platformKey";

/** Credential provider for BOTH transports in prod mode. */
export function platformCredentials(): Record<string, string> {
  if (typeof window === "undefined") return {};
  const key = window.localStorage.getItem(STORAGE_KEY);
  return key ? { Authorization: `Bearer ${key}` } : {};
}

export function getPlatformKey(): string | null {
  if (typeof window === "undefined") return null;
  return window.localStorage.getItem(STORAGE_KEY);
}

// ---------------------------------------------------------------------------
// Clerk signup. The dashboard authenticates the HUMAN with Clerk
// (GitHub / Google), then swaps Clerk's short-lived session JWT for a
// dashboard-only, EXPIRING platform key through `POST /auth/clerk` and
// stores it (`login(key)` below) — every
// transport, SSE and `GET /me` stay unchanged. Prod server only: the dev
// gateway has no such endpoint (the server answers 503 without Clerk
// config), so the login UI only offers Clerk in PROD_MODE.
// ---------------------------------------------------------------------------

/** Success shape of POST /auth/clerk. */
export interface ClerkExchangeResult {
  /** Dashboard-session platform key (`skp_…`); never shown, only stored. */
  key: string;
  org_slug: string;
  /** True when this sign-in created the org. */
  created: boolean;
  expires_at: string;
}

/** Error from the exchange, carrying the server's message. */
export class ClerkExchangeError extends Error {
  readonly status: number;
  /** 403 with `code: "beta_full"` — the signup cap is reached. */
  readonly betaFull: boolean;

  constructor(status: number, message: string, betaFull = false) {
    super(message);
    this.name = "ClerkExchangeError";
    this.status = status;
    this.betaFull = betaFull;
  }
}

/** POST /auth/clerk — the token rides the BODY, never `Authorization`. */
export async function exchangeClerkSession(
  sessionToken: string,
): Promise<ClerkExchangeResult> {
  const res = await fetch("/api/auth/clerk", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session_token: sessionToken }),
  });
  if (!res.ok) {
    // The endpoint's error shape: {"error": <human message>, "code"?: …};
    // `code` is the machine-readable part (`beta_full` on the signup cap).
    const text = await res.text().catch(() => "");
    let message = text;
    let code: string | undefined;
    try {
      const body = JSON.parse(text) as { error?: string; code?: string };
      message = body.error ?? text;
      code = body.code;
    } catch {
      // Non-JSON (proxy/edge) — surface the raw text.
    }
    throw new ClerkExchangeError(
      res.status,
      message,
      res.status === 403 && code === "beta_full",
    );
  }
  return (await res.json()) as ClerkExchangeResult;
}

/**
 * POST /auth/logout — best effort: revokes the presented dashboard-session
 * key. Failures are swallowed — signing out must always work.
 */
export async function logoutRemote(): Promise<void> {
  try {
    await fetch("/api/auth/logout", {
      method: "POST",
      headers: platformCredentials(),
    });
  } catch {
    // Offline / server down: the local sign-out proceeds regardless.
  }
}

// --- hand-mirrored public-api.yaml shapes ---------------------------------

interface DeploymentDto {
  id: string;
  app_id: string;
  version: number;
  status: string;
  created_at: string | null;
  error?: string | null;
  active: boolean;
  endpoint_url?: string | null;
}

interface AppSummaryDto {
  id: string;
  name: string;
  slug: string;
  framework: string | null;
  description: string | null;
  url: string | null;
  routes: { path: string; description: string }[];
  sessions_count: number;
  latest_deployment: DeploymentDto | null;
}

interface AppDetailDto extends AppSummaryDto {
  deployments: DeploymentDto[];
}

// registry status enum (data-model.sql — source of truth)
const DEPLOYMENT_STATUSES = ["building", "active", "failed", "retired"] as const;
function toDeployment(d: DeploymentDto): Deployment {
  const status = (DEPLOYMENT_STATUSES as readonly string[]).includes(d.status)
    ? (d.status as Deployment["status"])
    : "building";
  return { ...d, error: d.error ?? undefined, status };
}

function toApp(s: AppSummaryDto, deployments?: DeploymentDto[]): App {
  const history = (deployments ?? (s.latest_deployment ? [s.latest_deployment] : [])).map(toDeployment);
  const latest = s.latest_deployment ? toDeployment(s.latest_deployment) : null;
  return {
    id: s.id,
    name: s.name,
    slug: s.slug,
    framework:
      s.framework === "openai-agents" ||
      s.framework === "langgraph" ||
      s.framework === "pydantic-ai" ||
      s.framework === "claude-code"
        ? s.framework
        : null,
    description: s.description ?? "",
    url: s.url,
    routes: s.routes.map((r) => ({ path: r.path, description: r.description ?? "" })),
    sessions_count: s.sessions_count,
    latest_deployment: latest ?? {
      // The UI type requires a latest_deployment; an app with no
      // deployments yet (created but first build pending/failed) renders
      // an honest empty shell rather than breaking the overview.
      id: `none-${s.id}`,
      app_id: s.id,
      version: 0,
      status: "building",
      created_at: null,
      active: false,
    },
    deployments: history,
  };
}

export function createProdControlPlane(): ControlPlaneApi {
  const base = "/api";

  /** fetch with the CURRENT platform key; a 401 refreshes the session once
   *  and resends (transport/auth-refresh.ts). */
  function authedFetch(input: string, init: RequestInit = {}): Promise<Response> {
    return fetchWithAuthRetry(getPlatformKey, () =>
      fetch(input, {
        ...init,
        headers: { ...(init.headers as Record<string, string>), ...platformCredentials() },
      }),
    );
  }

  async function request<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const res = await authedFetch(`${base}${path}`, {
      method,
      headers: { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    return (await res.json()) as T;
  }

  return {
    /** GET /me — the signed-in identity; a rejected key throws ApiError(401). */
    async me(): Promise<Me> {
      return request<Me>("GET", "/me");
    },

    /** GET /me — validates an exchanged key (401 → rejected, nothing stored). */
    async login(key: string): Promise<void> {
      const res = await fetch(`${base}/me`, {
        headers: { Authorization: `Bearer ${key}` },
      });
      if (res.status === 401) {
        throw new ApiError(401, "that key was rejected by the server");
      }
      if (!res.ok) {
        throw new ApiError(res.status, await res.text().catch(() => ""));
      }
      window.localStorage.setItem(STORAGE_KEY, key);
    },

    async logout(): Promise<void> {
      window.localStorage.removeItem(STORAGE_KEY);
      // Not the gateway pin — that belongs to `sectr dev` sessions and is
      // cleared via ?gateway= (GatewayInit). Only the platform key goes.
    },

    async listApps(): Promise<App[]> {
      return ((await request<AppSummaryDto[]>("GET", "/apps")) ?? []).map((s) => toApp(s));
    },

    async getApp(appId: string): Promise<App | undefined> {
      try {
        const detail = await request<AppDetailDto>("GET", `/apps/${appId}`);
        return toApp(detail, detail.deployments);
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) return undefined;
        throw e;
      }
    },

    async listDeployments(appId: string): Promise<Deployment[]> {
      const detail = await request<AppDetailDto>("GET", `/apps/${appId}`);
      return detail.deployments.map(toDeployment);
    },

    /** POST /deployments/{id}/activate — rollback/promote. */
    async activateDeployment(deploymentId: string): Promise<void> {
      const res = await authedFetch(`${base}/deployments/${deploymentId}/activate`, {
        method: "POST",
      });
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },

    /** GET /deployments/{id}/wakes — the endpoint's health line. */
    async getDeploymentWakes(deploymentId: string, limit = 20): Promise<WakeSummary> {
      return request<WakeSummary>("GET", `/deployments/${deploymentId}/wakes?limit=${limit}`);
    },

    // -- settings (WS: the real settings page) ---------------------------

    async listApiKeys() {
      return (await request("GET", "/api-keys")) ?? [];
    },

    async createApiKey(name, kind, appId) {
      const body = await request(
        "POST",
        "/api-keys",
        { name, kind, ...(appId ? { app_id: appId } : {}) },
      );
      return body as ApiKeyMeta & { key: string };
    },

    async revokeApiKey(keyId) {
      const res = await authedFetch(`${base}/api-keys/${keyId}`, {
        method: "DELETE",
      });
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },

    async getCliLogin(userCode) {
      return (await request(
        "GET",
        `/auth/cli/requests/${encodeURIComponent(userCode)}`,
      )) as CliLoginRequest;
    },

    async approveCliLogin(userCode) {
      const res = await authedFetch(
        `${base}/auth/cli/requests/${encodeURIComponent(userCode)}/approve`,
        { method: "POST" },
      );
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },

    async denyCliLogin(userCode) {
      const res = await authedFetch(
        `${base}/auth/cli/requests/${encodeURIComponent(userCode)}/deny`,
        { method: "POST" },
      );
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },

    async listSecrets(appSlug) {
      return (
        (await request(
          "GET",
          `/secrets?app=${encodeURIComponent(appSlug)}`,
        )) ?? []
      );
    },

    async setSecret(appSlug, key, value) {
      const res = await authedFetch(
        `${base}/secrets/${encodeURIComponent(key)}?app=${encodeURIComponent(appSlug)}`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ value }),
        },
      );
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },

    async deleteSecret(appSlug, key) {
      const res = await authedFetch(
        `${base}/secrets/${encodeURIComponent(key)}?app=${encodeURIComponent(appSlug)}`,
        { method: "DELETE" },
      );
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
    },
  };
}
