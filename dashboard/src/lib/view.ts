/*
  UI-shaping helpers — pure functions, safe in BOTH server and client
  components (unlike hooks.ts, which is client-only). The transports speak
  contract types; this is where rows gain their UI projections (app_name
  joins — the rule: server returns canonical rows, the UI joins).
*/

import type { JournalEvent, Session as ContractSession } from "@/lib/transport/types";
import type { App, Deployment, PendingApproval, Session } from "@/lib/types";

/** Contract Session → UI Session: join app_name from the apps list. */
export function toUiSession(s: ContractSession, apps: App[]): Session {
  const app = apps.find((a) => a.id === s.app_id || a.name === s.app_id);
  return { ...s, app_name: app?.name ?? s.app_id };
}

export function toUiDeployment(d: Deployment): Deployment {
  return d;
}

export function toUiApproval(
  a: { session: ContractSession; event: JournalEvent },
  apps: App[],
): PendingApproval {
  return {
    session: toUiSession(a.session, apps),
    event: a.event as PendingApproval["event"],
  };
}

/** Deployment version label — dev deployments (version 0) render as
 *  "dev", prod ones as v1, v2, … (see dev-control-plane.ts). */
export function deploymentVersionLabel(d: Deployment): string {
  return d.version === 0 ? "dev" : `v${d.version}`;
}

/** The AG-UI endpoint for one of an app's routes, as `sectr deploy` prints
 *  it (`agui_endpoints` in sectr-cli): `<base>/ag-ui/<slug>/<route>`. An empty
 *  path has no endpoint. */
export function aguiUrl(
  app: Pick<App, "slug">,
  routePath: string,
  base: string,
): string | null {
  const route = routePath.replace(/^\/+/, "");
  if (!route) return null;
  return `${base.replace(/\/+$/, "")}/ag-ui/${app.slug}/${route}`;
}
