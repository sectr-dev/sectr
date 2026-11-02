/*
  The dashboard's API seam — the ONLY module UI code may call for backend
  data. This file owns the ONE mode decision; nothing below it (and no
  caller) knows about MOCK_MODE:

  - MOCK (NEXT_PUBLIC_SECTR_API unset, no ?gateway= param): fixtures +
    scripted streaming (`transport/mock-sessions-api.ts`,
    `transport/mock-control-plane.ts`).
  - DEV GATEWAY: the `sectr dev` gateway (base URL from the ?gateway=
    param `sectr dev` appends, persisted to localStorage) — no
    auth, one synthesized app (transport/dev-control-plane.ts).
  - PROD SERVER: the same-origin rewrite (`/api` → SECTR_SERVER_URL, see
    next.config.ts) with platform-key auth (the full skp_… value lives in
    browser localStorage and rides every request as a Bearer header via
    the credential-provider seam — transport/control-plane.ts). The key
    is exchanged from a Clerk session.

  The edge CANNOT reach a developer's localhost — so in dev-gateway mode
  ALL session I/O happens in the browser: session reads live in client
  components (hooks), never in server components. In prod mode the
  control plane is ALSO browser-side (the platform key is browser
  storage); server-side control-plane reads return with a server-side
  auth mechanism.

  Both transports implement the same `SessionsApi` interface
  (transport/types.ts) in CONTRACT types (generated from
  schema/sessions-api.yaml). UI-shaping (app_name joins, event
  projections) lives in hooks.ts / components — see `toUiSession` there.
*/

import { createSessionsApi } from "./transport/sessions-api";
import { mockSessionsApi } from "./transport/mock-sessions-api";
import { mockControlPlane } from "./transport/mock-control-plane";
import { createDevControlPlane } from "./transport/dev-control-plane";
import {
    createProdControlPlane,
    getPlatformKey,
    platformCredentials,
} from "./transport/control-plane";
import { fetchWithAuthRetry } from "./transport/auth-refresh";

/**
 * Base URL of the live backend. Empty = mock mode. Resolution order
 * (browser): `?gateway=<url>` (what `sectr dev` appends) >
 * localStorage (`sectr.gateway`, persisted by GatewayInit so navigations
 * keep the gateway without the param) > build-time env (prod) > mock.
 * On the server only env applies — the edge cannot reach a developer's
 * localhost, which is why session reads are browser-side.
 *
 * Invariant: production builds (NEXT_PUBLIC_SECTR_API set — the deploy
 * script inlines `/api`) can NEVER reach mock mode. Only non-empty
 * overrides count; `?gateway=` (empty) or an empty stored value falls
 * through to the build env. Local dev (env unset) keeps all three modes.
 */
export const API_URL = resolveApiUrl();

function resolveApiUrl(): string {
    if (typeof window !== "undefined") {
        const param = new URLSearchParams(window.location.search).get(
            "gateway",
        );
        if (param) return param;
        const stored = window.localStorage.getItem("sectr.gateway");
        if (stored) return stored;
    }
    return process.env.NEXT_PUBLIC_SECTR_API ?? "";
}

/** Mock mode: no backend configured. Callers never read this — api.ts does. */
export const MOCK_MODE = API_URL === "";

/** True when the sessions transport is a live gateway (not fixtures). */
export const SESSIONS_LIVE = !MOCK_MODE;

/** Local-gateway bases: loopback, and the local.sectr.dev TLS trick. */
export const isLocalGateway = (base: string) => {
    try {
        const host = new URL(base).hostname;
        return (
            host === "localhost" ||
            host === "127.0.0.1" ||
            host === "local.sectr.dev" ||
            host.endsWith(".local.sectr.dev")
        );
    } catch {
        return false; // relative prod base ("/api") and other non-URLs
    }
};

/** Prod mode: the same-origin rewrite to sectr-server. */
export const PROD_MODE = !MOCK_MODE && !isLocalGateway(API_URL);

// The ONE dispatch: same interface, fixtures / real gateway / prod server.
// The SSE tail gets its own base: in prod it connects DIRECT to the server
// (NEXT_PUBLIC_SECTR_SERVER_URL) — the streaming path must not traverse the
// /api rewrite, whose dev proxy starved streamed frames nondeterministically
// (M1 run finding). Cross-origin; the server answers any origin (bearer auth).
// Unset → fall back to the rewrite (correct, just proxy-mediated).
const SSE_BASE_URL =
    PROD_MODE && process.env.NEXT_PUBLIC_SECTR_SERVER_URL
        ? process.env.NEXT_PUBLIC_SECTR_SERVER_URL
        : API_URL;

const sessions = MOCK_MODE
    ? mockSessionsApi
    : createSessionsApi({
          baseUrl: API_URL,
          sseBaseUrl: SSE_BASE_URL,
          credentials: PROD_MODE ? platformCredentials : undefined,
          authRetry: PROD_MODE
              ? (send) => fetchWithAuthRetry(getPlatformKey, send)
              : undefined,
      });

export const {
    createSession,
    listSessions,
    getSession,
    sendSessionInput,
    decideApproval,
    listApprovals,
    devStatus,
    prodHealth,
    openSessionStream,
} = sessions;

// Control plane: mock fixtures in demo mode; on a dev base URL the
// gateway synthesizes the ONE local app from /dev/status; in prod mode
// the real control plane (apps/deployments/login) — all client-side
// (see transport/control-plane.ts).
const controlPlane = MOCK_MODE
    ? mockControlPlane
    : PROD_MODE
      ? createProdControlPlane()
      : createDevControlPlane({ devStatus: sessions.devStatus });

export const {
    me,
    listApps,
    getApp,
    listDeployments,
    activateDeployment,
    login,
    logout,
    listApiKeys,
    createApiKey,
    revokeApiKey,
    getCliLogin,
    approveCliLogin,
    denyCliLogin,
    listSecrets,
    setSecret,
    deleteSecret,
} = controlPlane;

export { ApiError } from "./transport/types";
export { getPlatformKey };
// Clerk signup (prod server only) — session-JWT exchange + remote logout.
export {
    exchangeClerkSession,
    logoutRemote,
    ClerkExchangeError,
    type ClerkExchangeResult,
} from "./transport/control-plane";
// Mid-session recovery from a dead key (transport/auth-refresh.ts).
export {
    markSignedOut,
    onAuthLost,
    setSessionRefresher,
} from "./transport/auth-refresh";
