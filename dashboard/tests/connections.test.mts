import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  PHRASES,
  deriveConnections,
  endpointUrlIn,
  healthLine,
  platformOf,
  runsOnFor,
  wakeHint,
} from "../src/lib/connections.ts";
import type { App, Deployment } from "../src/lib/types.ts";

const LAMBDA = "https://abc.lambda-url.us-east-1.on.aws/";
const RUN = "https://agent-xyz-uc.a.run.app/";
const OTHER = "https://agent.example.com/";

function deployment(over: Partial<Deployment> = {}): Deployment {
  return {
    id: "dep-1",
    app_id: "app-1",
    version: 4,
    status: "active",
    created_at: "2026-11-08T00:00:00Z",
    active: true,
    endpoint_url: null,
    ...over,
  };
}

function app(deployments: Deployment[], routes = ["/chat"]): App {
  return {
    id: "app-1",
    name: "support",
    slug: "support",
    framework: null,
    description: "",
    url: null,
    routes: routes.map((path) => ({ path, description: `${path} agent` })),
    sessions_count: 0,
    latest_deployment: deployments[0] ?? deployment({ active: false, version: 0 }),
    deployments,
  };
}

test("the phrases match the server's", () => {
  // The Rust constants are the source: a reworded server message must fail here.
  const rust = readFileSync(
    new URL("../../crates/sectr-common/src/endpoint.rs", import.meta.url),
    "utf8",
  );
  for (const [name, phrase] of Object.entries(PHRASES)) {
    assert.ok(
      rust.includes(`pub const ${name}: &str = ${JSON.stringify(phrase)};`),
      `${name} = ${JSON.stringify(phrase)} is not in sectr-common`,
    );
  }
});

test("platforms come from the host", () => {
  assert.equal(platformOf(LAMBDA), "lambda");
  assert.equal(platformOf("https://ABC.Lambda-Url.eu-west-1.on.aws."), "lambda");
  assert.equal(platformOf(RUN), "cloud-run");
  for (const url of [
    OTHER,
    "https://run.app/",
    "https://run.app.evil.com/",
    "https://lambda-url.us-east-1.on.aws/",
    "https://x.lambda-url.us-east-1.on.aws.evil.com/",
    "not a url",
  ]) {
    assert.equal(platformOf(url), "other", url);
  }
});

test("runs on reflects the active deployment", () => {
  const cases: [App, string, string, string | null, boolean][] = [
    [app([deployment({ endpoint_url: LAMBDA })]), "lambda", "Your AWS Lambda", "v4", true],
    [app([deployment({ endpoint_url: RUN })]), "cloud-run", "Your Google Cloud Run", "v4", true],
    [app([deployment({ endpoint_url: OTHER })]), "endpoint", "Your endpoint", "v4", true],
    [app([deployment()]), "sectr", "Sectr runners", "v4", false],
    [app([deployment({ active: false, status: "failed" })]), "none", "No active version", null, false],
  ];
  for (const [project, kind, title, version, health] of cases) {
    const node = runsOnFor(project, "server");
    assert.equal(node.kind, kind);
    assert.equal(node.title, title);
    assert.equal(node.version, version);
    assert.equal(node.healthDeploymentId !== null, health, kind);
  }
  // An older active version is what runs, not the newest row.
  const rolledBack = app([
    deployment({ id: "new", version: 5, active: false, status: "retired", endpoint_url: RUN }),
    deployment({ id: "old", version: 4, endpoint_url: LAMBDA }),
  ]);
  assert.equal(runsOnFor(rolledBack, "server").healthDeploymentId, "old");

  const local = runsOnFor(app([deployment({ version: 0 })]), "dev");
  assert.equal(local.title, "Your machine");
  assert.equal(local.healthDeploymentId, null);
});

test("wake failures get the fix for their platform", () => {
  const forbidden = `${PHRASES.WAKE_FAILED} abc.lambda-url.us-east-1.on.aws ${PHRASES.ANSWERED_HTTP} 403: Forbidden`;
  assert.match(wakeHint(forbidden, LAMBDA) ?? "", /auth type NONE/);
  assert.match(wakeHint(forbidden, RUN) ?? "", /--allow-unauthenticated/);
  assert.match(wakeHint(forbidden, OTHER) ?? "", /without its own auth/);
  assert.equal(wakeHint(`${PHRASES.WAKE_FAILED} x ${PHRASES.ANSWERED_HTTP} 502: Bad Gateway`, LAMBDA), null);

  const cases: [string, RegExp][] = [
    [`${PHRASES.WAKE_FAILED} ${PHRASES.COULD_NOT_CONNECT} x (refused)`, /Check the URL/],
    [`${PHRASES.WAKE_FAILED} the endpoint ${PHRASES.WAKE_TIMEOUT} 60s`, /cold start/],
    [`${PHRASES.WAKE_FAILED} the endpoint's sidecar ${PHRASES.WRONG_INGEST} turn.`, /SECTR_INGEST_URL/],
    [`${PHRASES.WAKE_FAILED} x ${PHRASES.PRIVATE_ADDRESS}, which…`, /public endpoints/],
  ];
  for (const [message, hint] of cases) {
    for (const url of [LAMBDA, RUN, OTHER]) assert.match(wakeHint(message, url) ?? "", hint, message);
  }
  assert.equal(wakeHint(`${PHRASES.WAKE_FAILED} the wake was cancelled`, LAMBDA), null);

  // The --force fix names the platform and says what it replaces.
  const wrong = `${PHRASES.WAKE_FAILED} the endpoint's sidecar ${PHRASES.WRONG_INGEST} turn.`;
  for (const [url, arg] of [[LAMBDA, "lambda"], [RUN, "cloud-run"], [OTHER, "container"], [null, "<platform>"]] as const) {
    const hint = wakeHint(wrong, url) ?? "";
    assert.ok(hint.includes(`sectr init --endpoint ${arg} --force`), hint);
    assert.match(hint, /rewrites the Dockerfile and \.dockerignore from scratch/);
  }

  // Without the deployment's URL, the host in the message decides.
  assert.match(wakeHint(forbidden) ?? "", /auth type NONE/);
  assert.equal(endpointUrlIn(forbidden), "https://abc.lambda-url.us-east-1.on.aws/");
  assert.equal(endpointUrlIn(`${PHRASES.WAKE_FAILED} the endpoint ${PHRASES.WAKE_TIMEOUT} 60s`), null);
  assert.equal(endpointUrlIn("the agent raised"), null);
});

test("the diagram has two frontends per route, one agent each", () => {
  const project = app([deployment({ endpoint_url: LAMBDA })], ["/chat", "/plan", ""]);
  const { frontends, agents, runsOn } = deriveConnections(project, "https://api.sectr.dev", "server");
  assert.deepEqual(
    frontends.map((f) => f.id),
    ["http:/chat", "ag-ui:/chat", "http:/plan", "ag-ui:/plan", "http:"],
  );
  assert.deepEqual(agents.map((a) => a.route), ["/chat", "/plan", ""]);
  assert.equal(runsOn.kind, "lambda");
});

test("the health line reads the summary", () => {
  assert.deepEqual(healthLine({ turns: 0, failed: 0, latest_failure: null }), {
    tone: "muted",
    text: "No turns yet",
    failure: null,
  });
  assert.equal(healthLine({ turns: 20, failed: 0, latest_failure: null }).text, "Last 20 turns woke");
  assert.equal(healthLine({ turns: 1, failed: 0, latest_failure: null }).text, "The last turn woke");
  const failure = { message: "endpoint wake failed: x", session_id: "s", created_at: "t" };
  const line = healthLine({ turns: 20, failed: 3, latest_failure: failure });
  assert.equal(line.tone, "warn");
  assert.equal(line.text, "3 of the last 20 wakes failed");
  assert.deepEqual(line.failure, failure);
});
