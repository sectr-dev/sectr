// Replays Sectr's golden AG-UI streams (schema/fixtures/valid/agui) through the
// REAL @ag-ui/client: SSE parsing, the stream verifier (verifyEvents), and the
// default reducer — served by a tiny local server that frames events exactly as
// sectr-server does (`data: <json>\n\n`).
//
// What this adds over the schema checks (test_agui_conformance.py): the client's
// stream-discipline rules (every bracket closed, one terminal event, …) and the
// reducer's behavior — notably that applying a run's events to the client's own
// messages yields the SAME messages as the journal fold (snapshot and stream
// agree: no duplicated user messages, reasoning not merged into assistant
// content, tool calls attached to the right message). A connect fixture
// (`"kind": "connect"`) is applied to a client holding nothing — a
// reloaded page — and must rebuild the whole thread.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { after, describe, it } from "node:test";
import { fileURLToPath } from "node:url";

import { HttpAgent, verifyEvents } from "@ag-ui/client";
import { from, lastValueFrom, toArray } from "rxjs";

const FIXTURES = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
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

assert.ok(fixtures.length >= 6, "golden fixtures present");

const servers = [];
after(() => servers.forEach((s) => s.close()));

/** Serve `events` as one SSE response per POST. */
async function serve(events) {
  const server = createServer((req, res) => {
    req.resume();
    req.on("end", () => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      for (const e of events) res.write(`data: ${JSON.stringify(e)}\n\n`);
      res.end();
    });
  });
  servers.push(server);
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return `http://127.0.0.1:${server.address().port}`;
}

/** Message shape the comparison cares about (ids, roles, content, tool calls). */
function normalize(messages) {
  return messages.map((m) => {
    const out = { id: m.id, role: m.role };
    if (m.content !== undefined && m.content !== "") out.content = m.content;
    if (m.toolCallId !== undefined) out.toolCallId = m.toolCallId;
    if (m.toolCalls?.length)
      out.toolCalls = m.toolCalls.map((c) => ({
        id: c.id,
        name: c.function.name,
        arguments: c.function.arguments,
      }));
    return out;
  });
}

describe("golden streams through the upstream AG-UI client", () => {
  for (const { name, doc } of fixtures) {
    it(`${name}: passes the stream verifier`, async () => {
      // The verifier is the client's own conformance gate: a violation errors the stream.
      const out = await lastValueFrom(
        from(doc.expected).pipe(verifyEvents(false), toArray()),
        { defaultValue: [] },
      ).catch((e) => assert.fail(`verifyEvents rejected the stream: ${e.message}`));
      assert.equal(out.length, doc.expected.length);
    });

    it(`${name}: applying the run to the client's messages equals the journal fold`, async () => {
      const url = await serve(doc.expected);
      const connect = doc.kind === "connect";
      const agent = new HttpAgent({
        url,
        threadId: doc.request.threadId,
        initialMessages: connect ? [] : doc.request.messages,
      });
      await agent.runAgent({ runId: connect ? doc.expected[0].runId : doc.request.runId });

      // A run that ended in an interrupt deliberately differs from the fold:
      // the pending approval call is folded into the NEXT run's snapshot.
      if (doc.journal.at(-1).type === "TOOL_APPROVAL_REQUIRED") return;

      assert.deepEqual(normalize(agent.messages), normalize(doc.final_messages));
    });
  }
});
