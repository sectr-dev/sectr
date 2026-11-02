/*
  DEV implementation of `ControlPlaneApi` — synthesizes the ONE local app
  from the dev gateway's `/dev/status` (DevStatus: project, app, routes,
  framework). The real control plane (apps,
  deployments, auth) arrives with the prod server (Phase 5/7); until then
  api.ts dispatches to this in gateway mode and to the mock in demo mode.

  All reads happen in the BROWSER — the edge cannot reach a developer's
  localhost (see api.ts). Null devStatus (gateway offline/unreachable)
  means an empty app list, honestly: nothing is reachable.
*/

import type { App, Deployment } from "../types";
import type { ControlPlaneApi, DevStatus } from "./types";

const FRAMEWORKS = ["openai-agents", "langgraph", "pydantic-ai", "claude-code"] as const;

/** Contract → UI shape. The dev gateway knows one app, one active
 *  deployment, and its routable paths. The deployment is deliberately
 *  minimal: version 0 renders as "dev" (dev versions track the manifest
 *  surface, not code edits — meaningless to show as a number), and
 *  created_at is null (a dev deployment is a running process, not an
 *  artifact with a birthdate). */
function toApp(status: DevStatus): App {
  const framework = FRAMEWORKS.includes(
    status.framework as (typeof FRAMEWORKS)[number],
  )
    ? (status.framework as App["framework"])
    : null;
  const deployment: Deployment = {
    id: `dev-${status.app}`,
    app_id: status.app,
    version: 0,
    status: "active",
    created_at: null,
    active: true,
  };
  return {
    // The registry id — sessions are stamped with THIS, so the
    // session→app join in view.ts works (the name alone would leave
    // raw UUIDs in the tables).
    id: status.app_id,
    name: status.app,
    slug: status.app,
    framework,
    description: `Local dev gateway — project ${status.project}, port ${status.gateway_port}`,
    url: null,
    routes: status.routes.map((r) => ({ path: r, description: "" })),
    sessions_count: 0,
    latest_deployment: deployment,
    deployments: [deployment],
  };
}

export function createDevControlPlane({
  devStatus,
}: {
  devStatus: () => Promise<DevStatus | null>;
}): ControlPlaneApi {
  // One probe per page load; the BackendBadge re-polls /dev/status for
  // liveness — this cache only wants identity, not freshness.
  let app: Promise<App | null> | undefined;
  const localApp = () => (app ??= devStatus().then((s) => (s ? toApp(s) : null), () => null));

  return {
    /** The dev gateway is unauthenticated — the org IS "the local project". */
    async me() {
      const a = await localApp();
      return { org: { id: a?.id ?? "dev", name: a?.name ?? "Local dev" } };
    },

    async listApps(): Promise<App[]> {
      const a = await localApp();
      return a ? [a] : [];
    },

    async getApp(appId: string): Promise<App | undefined> {
      const a = await localApp();
      return a?.id === appId ? a : undefined;
    },

    async listDeployments(appId: string): Promise<Deployment[]> {
      const a = await localApp();
      return a?.id === appId ? a.deployments : [];
    },

    async activateDeployment(_deploymentId: string): Promise<void> {
      const { toast } = await import("sonner");
      toast("Rolling back is a control-plane action", {
        description:
          "The dev gateway runs your local project — activate versions with sectr deploy (Phase 5 brings the prod control plane).",
      });
    },

    async login(_key: string): Promise<void> {
      // The dev gateway is unauthenticated and local to your machine.
    },

    async logout(): Promise<void> {
      // No session to clear — the account menu links back to /login.
    },

    // Settings: the dev gateway has no control plane — the honest answer
    // is a toast, not fake data.
    async listApiKeys() {
      await toastUnavailable();
      return [];
    },
    async createApiKey() {
      await toastUnavailable();
      throw new Error("unavailable on the dev gateway");
    },
    async revokeApiKey() {
      await toastUnavailable();
    },
    async getCliLogin() {
      await toastUnavailable();
      throw new Error("unavailable on the dev gateway");
    },
    async approveCliLogin() {
      await toastUnavailable();
    },
    async denyCliLogin() {
      await toastUnavailable();
    },
    async listSecrets() {
      await toastUnavailable();
      return [];
    },
    async setSecret() {
      await toastUnavailable();
    },
    async deleteSecret() {
      await toastUnavailable();
    },
  };
}

async function toastUnavailable() {
  const { toast } = await import("sonner");
  toast("Not available on the dev gateway", {
    description:
      "API keys and server-side secrets live on the control plane — run against a server (or demo data) to manage them.",
  });
}
