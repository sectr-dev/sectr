/*
  The project's Connections view, as data: how users reach the agents
  (frontends), the agents (one per route), and where they run (one Runs on
  node, from the active deployment). Only real nodes: every frontend here
  exists for every deployed route today.

  Also the endpoint-deployment helpers the dashboard shares with the CLI:
  the platform-by-host rule and the hints for wake failures. Both key on the
  server's error phrases, mirrored below from sectr_common::endpoint (a test
  checks they match). Pure: no runtime imports beyond view.ts, so
  `node --test` loads it directly.
*/

import type { App, Deployment, WakeSummary } from "./types";
import { aguiUrl } from "./view.ts";

/** The setup guide for running an agent on your own compute. */
export const ENDPOINT_GUIDE = "https://docs.sectr.dev/guides/your-own-compute/";

/** The server's endpoint error phrases (crates/sectr-common/src/endpoint.rs). */
export const PHRASES = {
  WAKE_FAILED: "endpoint wake failed:",
  DISABLED: "doesn't accept endpoint deployments",
  PRIVATE_ADDRESS: "resolves to a private address",
  COULD_NOT_CONNECT: "could not connect to",
  NO_ANSWER_IN_TIME: "didn't answer in time",
  ANSWERED_HTTP: "answered HTTP",
  WRONG_INGEST: "dialed a server that doesn't know this",
  DIAL_FAILED: "couldn't reach",
  WAKE_TIMEOUT: "did not start the turn within",
} as const;

export type Platform = "lambda" | "cloud-run" | "other";

/** Where an endpoint runs, as far as its URL's host tells (the same rule as
 *  `platform_of` in sectr-common). */
export function platformOf(url: string): Platform {
  let host: string;
  try {
    host = new URL(url).hostname;
  } catch {
    return "other";
  }
  const labels = host.toLowerCase().replace(/\.$/, "").split(".");
  const n = labels.length;
  if (
    n >= 5 &&
    labels[n - 4] === "lambda-url" &&
    labels[n - 2] === "on" &&
    labels[n - 1] === "aws"
  ) {
    return "lambda";
  }
  if (n >= 3 && labels[n - 2] === "run" && labels[n - 1] === "app") return "cloud-run";
  return "other";
}

/** The host of a URL, for compact display. */
export function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/** Whether a session error is a failed wake of the deployment's endpoint. */
export function isWakeFailure(message: string): boolean {
  return message.startsWith(PHRASES.WAKE_FAILED);
}

/** The endpoint a wake failure names, when it names its host (the server
 *  writes `endpoint wake failed: <host> answered …`). */
export function endpointUrlIn(message: string): string | null {
  if (!isWakeFailure(message)) return null;
  const host = message.slice(PHRASES.WAKE_FAILED.length).trim().split(/\s+/)[0] ?? "";
  return host.includes(".") ? `https://${host}/` : null;
}

/** What to fix on the customer's side for a wake failure, or null. Without
 *  an `endpointUrl`, the platform comes from the host in the message. */
export function wakeHint(message: string, endpointUrl?: string | null): string | null {
  const url = endpointUrl ?? endpointUrlIn(message);
  const platform = url ? platformOf(url) : "other";
  if (message.includes(`${PHRASES.ANSWERED_HTTP} 403`)) {
    if (platform === "lambda") {
      return "A Lambda function URL needs auth type NONE, and a resource policy that lets anyone call both lambda:InvokeFunctionUrl and lambda:InvokeFunction.";
    }
    if (platform === "cloud-run") {
      return "The service must allow unauthenticated invocations (deploy it with --allow-unauthenticated).";
    }
    return "The endpoint must accept requests without its own auth: Sectr's wake carries only the turn's token.";
  }
  if (message.includes(PHRASES.PRIVATE_ADDRESS)) {
    return "The server can only reach public endpoints.";
  }
  if (message.includes(PHRASES.COULD_NOT_CONNECT) || message.includes(PHRASES.NO_ANSWER_IN_TIME)) {
    return "Check the URL, and that the service is deployed and running.";
  }
  if (message.includes(PHRASES.WAKE_TIMEOUT)) {
    return "The endpoint answered but never started the turn: its cold start may be too slow, or the image may not run `sectr-worker --serve`.";
  }
  if (message.includes(PHRASES.WRONG_INGEST)) {
    // `sectr init`'s name for the platform; unknown without a URL.
    const arg = url ? { lambda: "lambda", "cloud-run": "cloud-run", other: "container" }[platform] : "<platform>";
    return `The image's SECTR_INGEST_URL points at another server. Fix it in the Dockerfile, or rerun \`sectr init --endpoint ${arg} --force\` (it rewrites the Dockerfile and .dockerignore from scratch), then rebuild and redeploy the image.`;
  }
  return null;
}

export type FrontendKind = "http" | "ag-ui";

export interface FrontendNode {
  id: string;
  kind: FrontendKind;
  label: string;
  detail: string;
  /** The agent (route) this frontend serves. */
  route: string;
}

export interface AgentNode {
  route: string;
  description: string;
}

export type RunsOnKind = "lambda" | "cloud-run" | "endpoint" | "sectr" | "local" | "none";

export interface RunsOnNode {
  kind: RunsOnKind;
  title: string;
  detail: string;
  /** The endpoint URL, for endpoint deployments. */
  url: string | null;
  /** "v4" / "dev", or null with no active deployment. */
  version: string | null;
  /** The active deployment, whose wakes give the health line (endpoints
   *  only; null otherwise). */
  healthDeploymentId: string | null;
}

export type ConsoleMode = "server" | "dev";

/** The deployment the project's routes run on: the active one. */
export function activeDeployment(app: App): Deployment | null {
  return app.deployments.find((d) => d.active) ?? (app.latest_deployment.active ? app.latest_deployment : null);
}

/** The Runs on node for a project. `mode` is "dev" under `sectr dev`. */
export function runsOnFor(app: App, mode: ConsoleMode): RunsOnNode {
  if (mode === "dev") {
    return {
      kind: "local",
      title: "Your machine",
      detail: "sectr dev",
      url: null,
      version: "dev",
      healthDeploymentId: null,
    };
  }
  const active = activeDeployment(app);
  if (!active) {
    return {
      kind: "none",
      title: "No active version",
      detail: "Deploy to run this project",
      url: null,
      version: null,
      healthDeploymentId: null,
    };
  }
  const version = active.version === 0 ? "dev" : `v${active.version}`;
  if (active.endpoint_url) {
    const platform = platformOf(active.endpoint_url);
    const title =
      platform === "lambda"
        ? "Your AWS Lambda"
        : platform === "cloud-run"
          ? "Your Google Cloud Run"
          : "Your endpoint";
    return {
      kind: platform === "other" ? "endpoint" : platform,
      title,
      detail: active.endpoint_url,
      url: active.endpoint_url,
      version,
      healthDeploymentId: active.id,
    };
  }
  return {
    kind: "sectr",
    title: "Sectr runners",
    detail: "Sectr builds and runs each version",
    url: null,
    version,
    healthDeploymentId: null,
  };
}

/** The project's diagram: two frontends per route (the invoke API and, for
 *  a non-empty path, the AG-UI endpoint), one agent per route, one Runs on
 *  node wired to every agent. `base` is the public server base. */
export function deriveConnections(
  app: App,
  base: string,
  mode: ConsoleMode,
): { frontends: FrontendNode[]; agents: AgentNode[]; runsOn: RunsOnNode } {
  const frontends = app.routes.flatMap((r): FrontendNode[] => {
    const http: FrontendNode = {
      id: `http:${r.path}`,
      kind: "http",
      label: "HTTP API",
      detail: `POST /sessions · ${r.path}`,
      route: r.path,
    };
    return aguiUrl(app, r.path, base)
      ? [
          http,
          {
            id: `ag-ui:${r.path}`,
            kind: "ag-ui",
            label: "AG-UI endpoint",
            detail: `For CopilotKit and other AG-UI frontends · ${r.path}`,
            route: r.path,
          },
        ]
      : [http];
  });
  return {
    frontends,
    agents: app.routes.map((r) => ({ route: r.path, description: r.description })),
    runsOn: runsOnFor(app, mode),
  };
}

export interface HealthLine {
  tone: "ok" | "warn" | "muted";
  text: string;
  /** The newest failure, when some failed. */
  failure: WakeSummary["latest_failure"];
}

/** The Runs on node's health line from a wakes summary. */
export function healthLine(summary: WakeSummary): HealthLine {
  if (summary.turns === 0) return { tone: "muted", text: "No turns yet", failure: null };
  if (summary.failed === 0) {
    return {
      tone: "ok",
      text: summary.turns === 1 ? "The last turn woke" : `Last ${summary.turns} turns woke`,
      failure: null,
    };
  }
  return {
    tone: "warn",
    text: `${summary.failed} of the last ${summary.turns} ${summary.turns === 1 ? "wake" : "wakes"} failed`,
    failure: summary.latest_failure,
  };
}
