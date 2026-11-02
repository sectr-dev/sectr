# AG-UI mapping — journal → AG-UI 1.0

**Normative.** The AG-UI endpoint is another *view over the journal* (the same
move as platform SSE, transcripts and the dashboard): the runner stays pure
(no runner-side AG-UI emission), the journal stays the single source of truth,
platform SSE (`GET /sessions/{id}/events`) stays the session-tail power path.
If AG-UI forks or fades, the projection is swappable.

What we pin is **this mapping**; the AG-UI schema itself is theirs. We pin
`protocolVersion: "1.0"` and vendor the upstream schema unmodified
(`vendor/ag-ui/`, see `UPSTREAM.md`). Golden fixtures:
`fixtures/valid/agui/` (journal + request → expected AG-UI stream) and
`fixtures/invalid/agui/` (documents the upstream schema must reject, each for
a stated reason). Implementation: `crates/sectr-common/src/agui.rs` (pure
projection), `crates/sectr-common/src/api/agui.rs` (the endpoint, shared by
prod and `sectr dev`) and `crates/sectr-server/src/agui.rs` (prod auth/CORS
layering).

## Endpoint

`/ag-ui/{app}/{route}` — one endpoint per deployed agent route. `{app}` is
the app id or slug; `{route}` is the manifest route path without its leading
slash (`/chat` → `/ag-ui/pm-agent/chat`; nested routes keep their slashes).
`sectr deploy` prints the full URL per route, so clients are zero-config.
Two operations on it:

- `POST` — a **run** (AG-UI 1.0): starts one turn and streams it.
- `GET ?threadId=` — **connect**: re-streams the thread's latest
  run, read-only (see [Connect](#connect-get-threadid)). Outside AG-UI 1.0.

The same endpoint is served by the **`sectr dev` gateway** — one shared
implementation (`sectr_common::api::agui`), no auth (like every dev route),
CORS open to any origin so a frontend under `next dev` can call it:
`sectr dev` prints `http(s)://<gateway>/ag-ui/<app>/<route>`; `{app}` is the
local app's name or id.

- Body: `RunAgentInput` JSON. Response: `text/event-stream`, one
  JSON-encoded event per `data:` frame, keep-alive comments every 15 s. No
  `id:` field and no resume cursor: AG-UI runs are self-contained POSTs.
- `threadId` **is** the Sectr session id (must be a UUID). Every run starts
  exactly one invocation, whose id is always server-minted; the client's
  `runId` (any string) is journaled alongside it as `ag_ui.run_id` (see
  [Journaled extras](#journaled-extras-the-ag_ui-namespace)), so a
  client-side run can be looked up in the journal.
- `protocolVersion`, when present, must be a `1.x` version (`"1.0"`,
  `"1.1"`, `"1.0.1"`, …): minor versions are additive, so a client upgrade
  within major 1 must not break runs. Any other major, or a non-numeric
  version, is a 400 (`unsupported protocolVersion`). Absent is accepted. We
  always declare `"1.0"` in `RUN_STARTED`.

### Run ↔ turn model

**One AG-UI run = one Sectr turn.** The run stream is a **filtered journal
read**: exactly one invocation's events, starting at its `INVOCATION_STARTED`
and ending at the first terminal event — sentinel (success), approval request
(interrupt), error. It is *not* the session tail.

- A run for a nonexistent `threadId` creates the session (owner = the token's
  `external_user_id`) on the requested route. The thread starts empty: if
  `messages` carries earlier conversation history (any message before the
  trailing one other than `system`/`developer`), the run is a `400` — history
  import is not supported, and silently dropping it would run the agent cold
  while the client shows a conversation the agent never saw. On an existing
  thread the client's history is ignored: the journal is the history, and
  `MESSAGES_SNAPSHOT` reconciles the client to it.
- **Busy thread.** A user-message run arriving while a turn is live is an
  HTTP `409` before the stream starts — even when it re-sends the same
  message. The live turn is never adopted by a run: a page that reloads
  mid-turn gets the live turn back through [connect](#connect-get-threadid),
  a read, so a run is only ever a new question — and a second tab or client
  must not be handed another run's answer. The live turn runs to completion.
- A concurrency / capacity / rate limit is an HTTP `429` before the
  stream starts, like the `404`/`409` above: body `{"error", "code"}` with
  `code` ∈ `concurrency_limit` | `platform_capacity` | `rate_limited`, plus a
  `Retry-After` header. Nothing is journaled for a rejected run; retry the same
  request. Runs only: connect creates nothing and never answers `429`.
- A user message on a thread awaiting an approval decision → `409` (answer the
  interrupt with `resume`). A user message on an errored thread starts a new
  turn (recovery, same as the sessions API).

### The user message → `input`

The request's trailing message must be `role: "user"`. Its `content` (a string,
or a content-parts array verbatim) is journaled as
`INVOCATION_STARTED.reason.input`. The client's message `id` is journaled as
`ag_ui.client_message_id` so that `MESSAGES_SNAPSHOT` returns the user message
under the id the client already holds (snapshots reconcile by id; without it
every snapshot would duplicate the user's messages). A non-user trailing
message is a `400` (frontend-tool result runs are v2). `system`/`developer`
messages are accepted but not journaled or passed to the runner.

### Journaled extras (the `ag_ui` namespace)

Everything AG-UI-specific that the endpoint journals lives in ONE
forward-compat extra on the run's `INVOCATION_STARTED`: an object under the
key `ag_ui`. Nothing protocol-specific is written as a top-level payload
field, so AG-UI fields cannot collide with core fields or with another
integration's extras. No other event carries AG-UI extras.

| Field | Written on | Meaning |
| --- | --- | --- |
| `ag_ui.run_id` | every run (user message and resume) | the request's `runId` (the invocation id stays server-minted) |
| `ag_ui.client_message_id` | user-message runs whose trailing message has an `id` | the client's id for the user message; the snapshot fold uses it |
| `ag_ui.forwarded_props` | user-message runs, when present and not `{}` | `RunAgentInput.forwardedProps`, verbatim: app-specific values for the agent |
| `ag_ui.context` | user-message runs, when non-empty | `RunAgentInput.context`, verbatim: `{description, value}` ambient information for the agent |

The runner receives these in its transcript (the invoke contract's `input`
stays the bare message content). The Python SDK does not surface
`forwarded_props` / `context` to handlers yet.

## Journal → AG-UI mapping

Field names are the upstream 1.0 schema's.

| Journal event | AG-UI event(s) |
| --- | --- |
| `INVOCATION_STARTED` | `RUN_STARTED {threadId, runId, protocolVersion: "1.0", input}` (`input` echoes the request); on an existing thread followed by `MESSAGES_SNAPSHOT` |
| `MESSAGE_START {role, message_id}` | `TEXT_MESSAGE_START {messageId: message_id, role: "assistant"}` |
| `MESSAGE_DELTA {delta: {type: "text"}}` | `TEXT_MESSAGE_CONTENT {messageId, delta: text}` |
| `MESSAGE_DELTA {delta: {type: "reasoning"}}` | `REASONING_START` · `REASONING_MESSAGE_START {role: "reasoning"}` on the first reasoning delta, `REASONING_MESSAGE_CONTENT {delta}` per delta, `REASONING_MESSAGE_END` · `REASONING_END` when the reasoning stops (next text delta, message end, tool call/result, run end) — all under the **derived id** `<message_id>:reasoning` |
| `MESSAGE_END {message_id}` | `TEXT_MESSAGE_END {messageId}` |
| `TOOL_CALL {tool_call_id, name, arguments}` | `TOOL_CALL_START {toolCallId, toolCallName: name, parentMessageId}` + `TOOL_CALL_ARGS {toolCallId, delta: arguments}` (one chunk: the journal holds complete arguments) + `TOOL_CALL_END {toolCallId}` |
| `TOOL_RESULT {tool_call_id, result}` | `TOOL_CALL_RESULT {messageId: tool-result-<id>, toolCallId, content: result, role: "tool"}` |
| `NO_MORE_ACTIONS` (sentinel) | `RUN_FINISHED {outcome: {type: "success"}}` |
| `TOOL_APPROVAL_REQUIRED` (final event) | `RUN_FINISHED {outcome: {type: "interrupt", interrupts: [{…}]}}` — see Approvals |
| `ERROR {message, code?}` (+ sentinel) | `RUN_ERROR {message, code}` |
| `SESSION_ERROR {message}` (+ synthesized sentinel) | `RUN_ERROR {message}` |
| `SESSION_CREATED`, `TOOL_APPROVAL_GRANTED`, `TOOL_APPROVAL_DENIED`, `TRANSCRIPT_COMPACTED` | not surfaced (journaled, not UI-relevant; compaction is LLM-context management — the snapshot still folds the full conversation) |

Every event carries `timestamp` (ms, from the journal `created_at`) and
`metadata: {seq}` (the journal `seq`; all events one journal row yields share
it). Tokens/usage (`RUN_FINISHED.usage`) are not journaled yet → not emitted.

### Ids

- Assistant text: the journal's `message_id` (runner-minted, unique).
- Reasoning: `<message_id>:reasoning`. **Never** the assistant's own id: AG-UI
  clients key messages by id, and `REASONING_MESSAGE_START` on an existing
  assistant message streams the reasoning text into that message's visible
  `content`.
- Tool results: `tool-result-<tool_call_id>`. A tool call with no preceding
  assistant message mints one with id = the call id (what a client does for a
  parent-less `TOOL_CALL_START`).
- User messages: the client's own id when journaled, else `user-<event id>`.

### Stream discipline

Everything a run opens is closed before its terminal event: a crashed turn
(partial message, no `MESSAGE_END`, then `SESSION_ERROR`) gets its
`TEXT_MESSAGE_END`s emitted before `RUN_ERROR`. Exactly one terminal event ends
every stream; if the tail ends or fails first (a user token expiring ends the
tail), the endpoint emits `RUN_ERROR` (`code: "stream_ended"` /
`"journal_error"`) rather than a silent close. A newer invocation appearing
before the terminal event ends the run with `RUN_ERROR` (`code: "superseded"`).

## Approvals = AG-UI interrupts

`TOOL_APPROVAL_REQUIRED` ends the run exactly as it suspends the turn:

```
Run 1: … → RUN_FINISHED { outcome: { type: "interrupt", interrupts: [{
  id:             <journal event id of the TOOL_APPROVAL_REQUIRED row>,
  reason:         "tool_approval",
  message:        <payload.reason, when present>,
  toolCallId:     <payload.tool_call_id>,
  responseSchema: { "type": "boolean" },
  metadata:       { toolName, arguments }   // what is being approved
}] } }

Run 2 (the client's resume run): RunAgentInput.resume = [
  { interruptId, status: "resolved", payload: true }    // grant
  { interruptId, status: "resolved", payload: false }   // deny
  { interruptId, status: "cancelled" }                  // deny (abandoned)
]
```

The resume entry maps onto the existing approval path — journaled as
`TOOL_APPROVAL_GRANTED`/`DENIED` with `approver` = the token's
`external_user_id` — then the turn resumes and streams as the run-2 stream
(`RUN_STARTED`, `MESSAGES_SNAPSHOT`, …). An unknown or mismatched
`interruptId`, no pending interrupt, a resolved entry without a boolean
payload, or more than one entry → `RUN_STARTED` + `RUN_ERROR` (a 200 stream,
`code: "invalid_run"`, or the sessions-API message). `expiresAt` is omitted:
our approvals do not expire.

`metadata` carries the tool name and arguments because the AG-UI `Interrupt`
has no such fields and the runner may journal only the approval request (no
`TOOL_CALL`) — the client would otherwise not know what it is approving.

## History / `connect()`

AG-UI 1.0 has no reconnect on the wire (`connect()` is SDK-side), so history
reaches a client in a `MESSAGES_SNAPSHOT`, two ways:

- **Runs:** every run on an existing thread begins with `RUN_STARTED` then a
  `MESSAGES_SNAPSHOT` folded from the journal through this run's own
  `INVOCATION_STARTED` — complete, tool-call results included (the gap that
  forced AWS AgentCore's integration to synthesize empty results). A new
  thread's first run has no prior history → no snapshot.
- **Connect** (below): the same snapshot, through the thread's latest run,
  then that run replayed — for a client that holds nothing (a reloaded page,
  a restarted or serverless Copilot Runtime).

The fold mirrors the SDK's canonical `ctx.messages`:

- user turns ← `INVOCATION_STARTED` with a `user_message` reason;
- assistant messages ← `MESSAGE_START/DELTA/END` (`text` → `content`);
  `reasoning` deltas → a separate `reasoning` message;
- `TOOL_CALL` (or an approval request with no `TOOL_CALL`) → `toolCalls` on the
  latest assistant message **of the same invocation**, else a new assistant
  message with id = the call id (what a client builds from a parent-less
  `TOOL_CALL_START`); an unanswered call stays unanswered;
- `TOOL_RESULT` → a `tool` message, placed right after the assistant message
  that issued the call (the AG-UI reducer's own placement rule, so the
  snapshot and the live stream give the client the same list);
- `TRANSCRIPT_COMPACTED` and the crash-continuation prompt are LLM-context
  management, never conversation history: the journal is not rewritten, so
  the user still sees the full original conversation.

## Connect (`GET ?threadId=`)

`GET /ag-ui/{app}/{route}?threadId=<uuid>` is the server half of an
SDK `connect()`: a reloaded page (or a Copilot Runtime that kept nothing, like
`SectrRunner` from `@sectr/copilotkit`) gets its thread back from the journal. It is
**read-only**: it starts no turn, creates no thread and writes nothing; two
tabs connecting are two independent readers. It is outside AG-UI 1.0, which
has no reconnect. Same credentials, app/route resolution, ownership and
existence rules as runs (below); not subject to the concurrency and rate limits.

Let *L* be the thread's latest `INVOCATION_STARTED`, whatever its reason
(`user_message` or `approval_decision`). The stream is *L*'s run, projected
exactly as a run is:

1. `RUN_STARTED {threadId, runId, protocolVersion: "1.0"}` — `runId` is *L*'s
   journaled `ag_ui.run_id` (the original run's id), else *L*'s invocation id
   (a turn started outside AG-UI). `input` is omitted (optional in the 1.0
   schema; the original request is not journaled).
2. `MESSAGES_SNAPSHOT` — the fold through *L*'s `INVOCATION_STARTED`, the
   same cut a run on an existing thread uses (always present).
3. *L*'s events — replayed, then live — to its terminal event.

| Thread | Response |
| --- | --- |
| doesn't exist | `200`, empty SSE body (CopilotKit connects on every page load, fresh threads included); nothing is created |
| exists, no invocation yet (only `SESSION_CREATED`) | `200`, empty SSE body |
| last turn finished | replay, then `RUN_FINISHED` success (history-only reconnect) |
| mid-turn | replay, then the live tail to the terminal event (reattach) |
| last turn abandoned (lease expired, not yet reclaimed) | replay, then the tail waits for the reconciler's `SESSION_ERROR` → `RUN_ERROR` |
| awaiting approval | replay ending in the SAME `RUN_FINISHED {outcome: interrupt}` (same interrupt id); the client answers with a normal resume run |
| last turn errored | replay ending in `RUN_ERROR` (faithful: the error the user saw) |
| foreign / other-app thread | `404` (as runs) |
| other route | `409` (as runs) |

The existence oracle is the one runs already have: an empty `200` for a free
id, `404` for a foreign one.

## Auth, CORS, errors

- `Authorization: Bearer` with `sks_` (browser, ownership-scoped) or `ski_`
  (backend) tokens. Platform keys: `403`. No `?token=` fallback (AG-UI clients
  are fetch-based). No new credential class: thread access is ownership-scoped
  (`external_user_id`), not token-instance-scoped, so `sks_` rotation per page
  load is invisible to existing threads.
- Token ↔ app binding: the token's app must be `{app}` (`403` otherwise); a
  foreign or ownerless thread is `404`, never `403`, and reveals nothing
  about the thread's owner or contents. Because an unknown `threadId` creates
  a thread, a caller *can* tell "exists but not yours" (`404`) from "free"
  (thread created): the endpoint is an existence oracle for thread ids.
  Accepted: ids are random UUIDs the caller must already hold, and the oracle
  reveals nothing else. All session
  creation, visibility and journaling runs through the same
  `SessionService` internals as the sessions API (single authorization path).
- `{route}` must be in the **active deployment's** manifest; unknown route →
  `404` naming the available routes. A thread belongs to its route (`409` for
  another).
- CORS: `OPTIONS` is answered before auth with `Access-Control-Allow-Origin: *`
  (Bearer-header auth, no cookies), `GET` and `POST`, headers `authorization,
  content-type, accept`.
- HTTP errors are `{"error": "…"}` with: `400` malformed `RunAgentInput` /
  unsupported `protocolVersion` / non-UUID `threadId` / non-user trailing
  message / history on a new thread; `401` bad credential; `403` platform key
  or wrong app; `404` unknown app/route/thread; `409` busy thread, thread
  awaiting approval, route mismatch; `429` concurrency/capacity/rate limit
  (above). Run-level failures are `RUN_ERROR` events. Connect: `400` for a
  missing or non-UUID `threadId`; a thread with no run is an empty `200`, not
  a `404`.

## Non-goals (v1)

`STATE_SNAPSHOT`/`STATE_DELTA` sync (v2, with `STATE_PATCH`); frontend tools
(`tools` is accepted but unused; frontend-tool execution is v2); token usage
(`RUN_FINISHED.usage`; needs journaled token counts); `*_CHUNK` events (we emit
the explicit start/content/end triads); `ACTIVITY_*`/`SUBAGENT_*`;
`RUN_FINISHED.outcome.cancelled`. `forwardedProps`/`context` are accepted,
echoed in `RUN_STARTED.input` and journaled under `ag_ui` (see Journaled
extras), but agents can't use them through the SDK yet. Raw journal SSE
remains the primary documented contract; AG-UI is a projection.

## Conformance

The upstream `spec/harness` is a regression suite for the AG-UI *schema* and
its first-party clients, not a tool that can be pointed at an endpoint. We
check conformance three ways:

1. `test_agui_conformance.py` (`uv run pytest` in `schema/`) validates every
   golden stream, request and journal against the vendored upstream schema,
   and the invalid fixtures against the stated defect. Connect fixtures
   (`connect-*.json`, `"kind": "connect"`, request `{threadId}`) are the
   journal's latest run re-streamed: no `input`, always a snapshot.
2. `agui-conformance/` (Node) replays the golden streams through the real
   `@ag-ui/client` stream verifier and reducer, and checks that applying a
   run's events to the client's own messages agrees with the journal fold (a
   connect's, to a client holding nothing).
3. sectr-common `agui::tests` pins the projection (golden streams) and stream
   discipline; sectr-server `agui::tests` runs the endpoint end to end over a
   real Postgres journal: full turn, approval suspend→resume via `resume[]`,
   busy-thread 409, `SESSION_ERROR`, replay snapshot, auth, CORS — and
   connect (live tail beside the run, pending approval, thread states, auth,
   limits).

## Corrections to earlier notes

- AG-UI pre-1.0 used `THINKING_CONTENT` for `reasoning`; 1.0 renamed the
  family to `REASONING_*`.
- Field names follow the 1.0 schema: `TOOL_CALL_START.toolCallName` (not
  `toolName`), and `TOOL_CALL_RESULT` requires `messageId`.
- The reasoning id is derived, not the assistant's id (see Ids).
- Runner `ERROR` events map to `RUN_ERROR` (the original table omitted them).
