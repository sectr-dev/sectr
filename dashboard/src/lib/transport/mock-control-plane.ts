/*
  MOCK implementation of `ControlPlaneApi` (see ./types.ts) — fixtures.
  The prod control plane (apps, deployments, auth) arrives with the Rust
  server (Phase 5/7); until then api.ts wires this directly — there is no
  real implementation to dispatch to, and no MOCK_MODE split needed here.
*/

import type { App, Deployment } from "../types";
import { ApiError, type ApiKeyMeta, type ControlPlaneApi, type SecretMeta } from "./types";
import { mockApps as fixtureApps } from "../mock-data";
import { MOCK_EMPTY } from "../mock-scenario";

// The empty demo account (badge toggle) starts with nothing; creating a key
// or secret still works, in memory.
const mockApps: App[] = MOCK_EMPTY ? [] : fixtureApps;

// In-memory settings store — the mock page runs through the SAME
// transport surface as prod (api.ts re-export), just against fixtures.
const mockKeys: ApiKeyMeta[] = MOCK_EMPTY ? [] : [
  {
    id: "k1",
    name: "production",
    kind: "platform",
    app_id: null,
    created_at: "2026-07-01T09:00:00Z",
    last_used_at: "2026-07-26T18:12:00Z",
    revoked_at: null,
  },
];
const mockSecrets: SecretMeta[] = MOCK_EMPTY ? [] : [
  { name: "OPENAI_API_KEY", created_at: "2026-07-10T08:00:00Z", updated_at: "2026-08-02T11:20:00Z" },
  { name: "STRIPE_SECRET_KEY", created_at: "2026-07-18T16:45:00Z", updated_at: null },
];

export const mockControlPlane: ControlPlaneApi = {
  /** Demo identity — fake data is demo-mode only (the prod console gates). */
  async me() {
    return { org: { id: "acme-ai", name: "Acme AI" } };
  },

  async listApps(): Promise<App[]> {
    return mockApps;
  },

  async getApp(appId: string): Promise<App | undefined> {
    return mockApps.find((a) => a.id === appId);
  },

  async listDeployments(appId: string): Promise<Deployment[]> {
    return mockApps.find((a) => a.id === appId)?.deployments ?? [];
  },

  async activateDeployment(_deploymentId: string): Promise<void> {
    // Lazy import keeps sonner out of modules that never toast.
    const { toast } = await import("sonner");
    toast("Rollback is mocked in this preview", {
      description: "sectr-server will journal this as a deployment activation.",
    });
  },

  async login(_key: string): Promise<void> {
    // mock: the login page navigates straight through
  },

  async logout(): Promise<void> {
    // mock: the account menu links back to /login
  },

  async listApiKeys() {
    return mockKeys;
  },

  async createApiKey(name, kind, appId) {
    const key: ApiKeyMeta = {
      id: crypto.randomUUID(),
      name,
      kind,
      app_id: appId ?? null,
      created_at: new Date().toISOString(),
      last_used_at: null,
      revoked_at: null,
    };
    mockKeys.unshift(key);
    return { ...key, key: `${kind === "platform" ? "skp" : "ski"}_mock_value_shown_once` };
  },

  async revokeApiKey(keyId) {
    const k = mockKeys.find((x) => x.id === keyId);
    if (k) k.revoked_at = new Date().toISOString();
  },

  // Any code renders a pending request, so the approval page can be
  // previewed in mock mode; a code starting with X mimics an expired one.
  async getCliLogin(userCode) {
    if (userCode.toUpperCase().startsWith("X")) {
      throw new ApiError(404, "login request not found or expired");
    }
    return {
      user_code: userCode.toUpperCase(),
      client_name: "donald-mbp",
      created_at: new Date(Date.now() - 12_000).toISOString(),
      expires_at: new Date(Date.now() + 588_000).toISOString(),
      status: "pending",
    };
  },

  async approveCliLogin() {},

  async denyCliLogin() {},

  async listSecrets(appSlug) {
    // One store across apps is enough for fixtures.
    void appSlug;
    return mockSecrets;
  },

  async setSecret(appSlug, key) {
    void appSlug;
    const existing = mockSecrets.find((x) => x.name === key);
    if (existing) existing.updated_at = new Date().toISOString();
    else mockSecrets.unshift({ name: key, created_at: new Date().toISOString(), updated_at: null });
  },

  async deleteSecret(appSlug, key) {
    void appSlug;
    const i = mockSecrets.findIndex((x) => x.name === key);
    if (i >= 0) mockSecrets.splice(i, 1);
  },
};
