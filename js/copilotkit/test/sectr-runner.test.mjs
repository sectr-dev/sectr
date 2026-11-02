// SectrRunner and SectrAgent (the built dist/) against a local server that plays Sectr's
// AG-UI endpoint: it serves the contract's golden streams
// (schema/fixtures/valid/agui) framed exactly as sectr-server does
// (`data: <json>\n\n`), plus the failures a real deployment can hit.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { after, describe, it } from "node:test";
import { fileURLToPath } from "node:url";

import { HttpAgent } from "@ag-ui/client";
import { AgentRunner } from "@copilotkit/runtime/v2";
import { firstValueFrom, lastValueFrom, toArray } from "rxjs";

import { SectrAgent, SectrRunner } from "../dist/index.js";

const FIXTURES = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "schema",
  "fixtures",
  "valid",
  "agui",
);

const fixtures = readdirSync(FIXTURES)
  .filter((f) => f.endsWith(".json"))
  .sort()
  .map((f) => ({
    name: f.replace(/\.json$/, ""),
    doc: JSON.parse(readFileSync(join(FIXTURES, f), "utf8")),
  }));
const connectFixtures = fixtures.filter(({ doc }) => doc.kind === "connect");
const runFixtures = fixtures.filter(({ doc }) => doc.kind !== "connect");
assert.ok(connectFixtures.length >= 3 && runFixtures.length >= 3, "golden fixtures present");

const THREAD = "0192f3a4-b5c6-7e2f-8a9b-1c2d3e4f5a6b";
const sse = (event) => `data: ${JSON.stringify(event)}\n\n`;

// node:test fails the file on any unhandled rejection, which is what the
// dropped-connection and unsubscribe tests guard: one would crash a plain Node
// host (see quietBody in src/index.ts).
const servers = [];
after(() => servers.forEach((s) => s.closeAllConnections() || s.close()));

/** A fake Sectr: `handle(req, res)` answers; every request is recorded. */
async function sectr(handle, { agentHeaders } = {}) {
  const requests = [];
  const server = createServer((req, res) => {
    const record = { method: req.method, url: new URL(req.url, "http://x"), headers: req.headers, closed: false };
    requests.push(record);
    req.on("close", () => (record.closed = true));
    req.resume();
    handle(req, res);
  });
  servers.push(server);
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const url = `http://127.0.0.1:${server.address().port}/ag-ui/support-agent/chat`;
  const agents = { default: new SectrAgent({ url, headers: agentHeaders }) };
  return { requests, url, agents, runner: new SectrRunner({ agents }) };
}

const streamOf = (events) => (_req, res) => {
  res.writeHead(200, { "content-type": "text/event-stream" });
  for (const e of events) res.write(sse(e));
  res.end();
};

const refuse = (status, message) => (_req, res) => {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: message }));
};

const connect = (runner, request = {}) =>
  lastValueFrom(
    runner.connect({ threadId: THREAD, agentId: "default", headers: {}, ...request }).pipe(toArray()),
  );

const lastOf = (events) => events[events.length - 1];

describe("connect()", () => {
  it("GETs the agent's URL with ?threadId= and the forwarded headers", async () => {
    const { requests, runner } = await sectr(streamOf([]));
    await connect(runner, { headers: { authorization: "Bearer sks_test", "x-trace": "t1" } });
    assert.equal(requests.length, 1);
    const [req] = requests;
    assert.equal(req.method, "GET");
    assert.equal(req.url.pathname, "/ag-ui/support-agent/chat");
    assert.equal(req.url.searchParams.get("threadId"), THREAD);
    assert.equal(req.headers.authorization, "Bearer sks_test");
    assert.equal(req.headers["x-trace"], "t1");
    assert.equal(req.headers.accept, "text/event-stream");
  });

  for (const { name, doc } of connectFixtures) {
    it(`${name}: streams the golden events as is`, async () => {
      const { runner } = await sectr(streamOf(doc.expected));
      assert.deepEqual(await connect(runner), doc.expected);
    });
  }

  it("an empty 200 (a new thread) is an empty stream, not an error", async () => {
    const { runner } = await sectr(streamOf([]));
    assert.deepEqual(await connect(runner), []);
  });

  for (const [status, message] of [
    [404, "unknown app: support-agent"],
    [409, "thread is busy"],
  ]) {
    it(`an HTTP ${status} ends in a RUN_ERROR carrying Sectr's message`, async () => {
      const { runner } = await sectr(refuse(status, message));
      const events = await connect(runner);
      assert.equal(events.length, 1);
      assert.equal(events[0].type, "RUN_ERROR");
      assert.match(events[0].message, new RegExp(String(status)));
      assert.ok(events[0].message.includes(message), events[0].message);
    });
  }

  it("a dropped connection closes the open message, then RUN_ERROR", async () => {
    const started = { type: "RUN_STARTED", threadId: THREAD, runId: "run-1" };
    const { runner } = await sectr((_req, res) => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write(sse(started));
      res.write(sse({ type: "TEXT_MESSAGE_START", messageId: "m1", role: "assistant" }));
      res.write(sse({ type: "TEXT_MESSAGE_CONTENT", messageId: "m1", delta: "Let me" }));
      setTimeout(() => res.socket.destroy(), 50);
    });
    const events = await connect(runner);
    assert.deepEqual(
      events.map((e) => e.type),
      ["RUN_STARTED", "TEXT_MESSAGE_START", "TEXT_MESSAGE_CONTENT", "TEXT_MESSAGE_END", "RUN_ERROR"],
    );
    assert.equal(events[3].messageId, "m1");
    assert.equal(lastOf(events).code, "INCOMPLETE_STREAM");
  });

  it("an unknown agentId ends in a RUN_ERROR, with no request", async () => {
    const { requests, runner } = await sectr(streamOf([]));
    const events = await connect(runner, { agentId: "nope" });
    assert.equal(events.length, 1);
    assert.equal(events[0].type, "RUN_ERROR");
    assert.match(events[0].message, /no agent "nope"/);
    assert.equal(requests.length, 0);
  });

  it("forwarded headers override the agent's own, whatever their case", async () => {
    const { requests, runner } = await sectr(streamOf([]), {
      agentHeaders: { Authorization: "Bearer agent-default", "X-App": "a" },
    });
    await connect(runner, { headers: { authorization: "Bearer sks_user" } });
    assert.equal(requests[0].headers.authorization, "Bearer sks_user");
    assert.equal(requests[0].headers["x-app"], "a");
  });

  it("a plain HttpAgent in the map ends in a RUN_ERROR naming SectrAgent, with no request", async () => {
    const { requests, url } = await sectr(streamOf([]));
    const runner = new SectrRunner({ agents: { default: new HttpAgent({ url }) } });
    const events = await connect(runner);
    assert.equal(events.length, 1);
    assert.equal(events[0].type, "RUN_ERROR");
    assert.match(events[0].message, /plain HttpAgent; use SectrAgent/);
    assert.equal(requests.length, 0);
  });

  it("unsubscribing cancels the request to Sectr", async () => {
    const { requests, runner } = await sectr((_req, res) => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write(sse({ type: "RUN_STARTED", threadId: THREAD, runId: "run-1" }));
      // ... and the turn keeps running: the stream stays open.
    });
    const first = await firstValueFrom(
      runner.connect({ threadId: THREAD, agentId: "default", headers: {} }),
    );
    assert.equal(first.type, "RUN_STARTED");
    for (let i = 0; i < 100 && !requests[0].closed; i++) await new Promise((r) => setTimeout(r, 10));
    assert.ok(requests[0].closed, "the server saw the request close");
  });
});

describe("run()", () => {
  const input = (runId) => ({
    threadId: THREAD,
    runId,
    messages: [{ id: "msg-user-1", role: "user", content: "hello" }],
    tools: [],
    context: [],
    state: {},
    forwardedProps: {},
  });
  const run = (runner, agents, runId) =>
    lastValueFrom(
      runner.run({ threadId: THREAD, agent: agents.default, input: input(runId) }).pipe(toArray()),
    );

  for (const { name, doc } of runFixtures) {
    it(`${name}: POSTs through the HttpAgent and streams to the terminal event`, async () => {
      const { requests, agents, runner } = await sectr(streamOf(doc.expected));
      const runId = doc.expected[0].runId ?? "run-1";
      const events = await run(runner, agents, runId);
      assert.equal(requests[0].method, "POST");
      assert.deepEqual(
        events.map((e) => e.type),
        doc.expected.map((e) => e.type),
      );
      assert.equal(lastOf(events).type, lastOf(doc.expected).type);
    });
  }

  it("an HTTP 404 ends in a RUN_ERROR", async () => {
    const { agents, runner } = await sectr(refuse(404, "unknown app: support-agent"));
    const events = await run(runner, agents, "run-1");
    assert.equal(lastOf(events).type, "RUN_ERROR");
    assert.match(lastOf(events).message, /404/);
  });
});

describe("SectrAgent (direct mode)", () => {
  // What CopilotKit calls on page load: connectAgent() → the agent's connect().
  const reconnect = async (agent) => {
    const events = [];
    agent.threadId = THREAD;
    await agent.connectAgent({}, { onEvent: ({ event }) => events.push(event) });
    return events;
  };

  it("connectAgent() GETs ?threadId= with the agent's own headers", async () => {
    const { requests, agents } = await sectr(streamOf([]), {
      agentHeaders: { Authorization: "Bearer sks_browser" },
    });
    await reconnect(agents.default);
    assert.equal(requests.length, 1);
    assert.equal(requests[0].method, "GET");
    assert.equal(requests[0].url.searchParams.get("threadId"), THREAD);
    assert.equal(requests[0].headers.authorization, "Bearer sks_browser");
  });

  for (const { name, doc } of connectFixtures) {
    it(`${name}: connectAgent() replays the golden events`, async () => {
      const { agents } = await sectr(streamOf(doc.expected));
      const events = await reconnect(agents.default);
      assert.deepEqual(
        events.map((e) => e.type),
        doc.expected.map((e) => e.type),
      );
    });
  }

  it("a pending approval comes back as the interrupt, in the agent's state", async () => {
    const pending = connectFixtures.find(({ doc }) =>
      doc.expected.some((e) => e.type === "RUN_FINISHED" && e.outcome?.type === "interrupt"),
    );
    assert.ok(pending, "a connect fixture ends in an interrupt");
    const { agents } = await sectr(streamOf(pending.doc.expected));
    const events = await reconnect(agents.default);
    assert.equal(lastOf(events).outcome.type, "interrupt");
    assert.ok(agents.default.messages.length > 0, "history restored");
  });

  it("a new thread (empty 200) reconnects to nothing, without an error", async () => {
    const { agents } = await sectr(streamOf([]));
    assert.deepEqual(await reconnect(agents.default), []);
  });

  it("run() still POSTs (it's an HttpAgent)", async () => {
    const run = runFixtures[0];
    const { requests, agents } = await sectr(streamOf(run.doc.expected));
    const agent = agents.default;
    agent.threadId = THREAD;
    agent.messages = [{ id: "msg-user-1", role: "user", content: "hello" }];
    await agent.runAgent({ runId: run.doc.expected[0].runId ?? "run-1" });
    assert.equal(requests[0].method, "POST");
  });

  it("clone() is still a SectrAgent (CopilotKit clones agents per thread)", () => {
    const agent = new SectrAgent({ url: "http://127.0.0.1:1/ag-ui/a/r" });
    assert.ok(agent.clone() instanceof SectrAgent);
    assert.ok(agent.clone() instanceof HttpAgent);
  });
});

describe("shape", () => {
  it("is an AgentRunner from the app's own @copilotkit/runtime", async () => {
    const { runner } = await sectr(streamOf([]));
    assert.ok(runner instanceof AgentRunner);
    assert.equal(await runner.isRunning(), false);
    assert.equal(await runner.stop(), false);
  });
});
