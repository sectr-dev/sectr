# Sectr contracts — human-readable reference

> **Generated** from `event.schema.json` / `invocation.schema.json` by `uv run schema/render_docs.py`. Do not edit by hand — the JSON Schemas are the source of truth. See `schema/README.md` for the rules and `schema/schema.mmd` for the database diagram.

## Event envelope

Every journal row and every client SSE frame. `payload` is bound to a schema by the `type` discriminator (see below).

| field | type | req | description |
|---|---|---|---|
| `id` | UUIDv7 | ✔ |  |
| `session_id` | UUID | ✔ |  |
| `deployment_id` | UUID | ✔ | Deployment the session is pinned to. |
| `seq` | integer ≥ 0 | ✔ | Per-session, monotonic, gap-free within a turn, starts at 0. Assigned by the journal writer, never user code. |
| `type` | event type (enum below) | ✔ |  |
| `payload` | any JSON | ✔ | Object; schema per event type (see tables below). |
| `created_at` | RFC 3339 timestamp | ✔ |  |

## Event types

### Runner-emitted

> Emitted by the user process on the invoke response stream; validated and journaled by the sidecar.

#### `MESSAGE_START` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `role` | string | ✔ | e.g. "assistant"; opaque to the platform. |
| `message_id` | string | ✔ | Runner-scoped message identifier; correlates START/DELTA/END. |

#### `MESSAGE_DELTA` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `message_id` | string | ✔ |  |
| `delta` | object | ✔ | Interpretable content delta: {type, text} with a CLOSED kind vocabulary — `text` (maps to AG-UI TEXT_MESSAGE_CONTENT) and `reasoning` (maps to THINKING_CONTENT). One wire shape only; unknown kinds are invalid: framework chunk types outside the vocabulary must be normalized by the SDK adapter before journaling (e.g. Anthropic input_json_delta accumulates into TOOL_CALL.arguments). |

#### `MESSAGE_END` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `message_id` | string | ✔ |  |
| `finish_reason` | string |  | Opaque to the platform, e.g. "stop", "length", "tool_use". |
| `framework_state` | object |  | What the adapter that wrote it needs to rehydrate and replay THIS model response as its agent framework stored it, beyond the message's text and tool calls (e.g. signed reasoning); never state of other responses or of the run as a whole. Its `data` is opaque to the platform, which stores and serves it but never interprets it; only the adapter named by `adapter` reads it back. At most 256 KiB serialized: the platform drops a larger one. |

#### `TOOL_CALL` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `tool_call_id` | string | ✔ |  |
| `name` | string | ✔ |  |
| `arguments` | any JSON | ✔ |  |

#### `TOOL_RESULT` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `tool_call_id` | string | ✔ |  |
| `result` | any JSON | ✔ |  |

#### `TOOL_APPROVAL_REQUIRED` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `tool_call_id` | string | ✔ |  |
| `name` | string | ✔ |  |
| `arguments` | any JSON | ✔ |  |
| `reason` | string |  | Human-readable explanation shown in the approval inbox. |

#### `ERROR` — Runner-emitted

| field | type | req | description |
|---|---|---|---|
| `message` | string | ✔ |  |
| `code` | string |  |  |
| `transient` | boolean |  | True if a retry of the same invocation may succeed. |

#### `NO_MORE_ACTIONS` — Runner-emitted (synthesized by the SDK driver on generator exhaustion or after an unhandled error's ERROR event; synthesized by the sidecar after SESSION_ERROR on a runner crash)

| field | type | req | description |
|---|---|---|---|

#### `TRANSCRIPT_COMPACTED` — Runner-emitted when the agent framework compacts context (framework's algorithm, framework's model access — the SDK/adapter captures the result and journals it mid-stream)

| field | type | req | description |
|---|---|---|---|
| `replaces_up_to_seq` | integer ≥ 0 | ✔ |  |
| `messages` | array | ✔ | OpenAI-style message dicts (the canonical view) — the entire new context, framework compaction output normalized. |

### Platform-emitted

> Written by the server/sidecar only — never by runners.

#### `SESSION_CREATED` — Platform-emitted (seq 0 of a session)

| field | type | req | description |
|---|---|---|---|
| `app_id` | UUID | ✔ |  |
| `route` | string | ✔ |  |
| `external_user_id` | string |  | owner identity: the (app-scoped) end-user the session belongs to; absent when backend/platform-created |

#### `INVOCATION_STARTED` — Platform-emitted

| field | type | req | description |
|---|---|---|---|
| `invocation_id` | UUID | ✔ |  |
| `route` | string | ✔ |  |
| `deployment_id` | UUID | ✔ |  |
| `reason` | invocationReason | ✔ |  |

#### `TOOL_APPROVAL_GRANTED` — Platform-emitted

| field | type | req | description |
|---|---|---|---|
| `tool_call_id` | string | ✔ |  |
| `approver` | string |  |  |

#### `TOOL_APPROVAL_DENIED` — Platform-emitted

| field | type | req | description |
|---|---|---|---|
| `tool_call_id` | string | ✔ |  |
| `approver` | string |  |  |

#### `SESSION_ERROR` — Platform-emitted

| field | type | req | description |
|---|---|---|---|
| `message` | string | ✔ |  |


## Runner invoke contract

`POST http://localhost:$SECTR_PORT/invocations` — sidecar → user process. Response: `200`, `text/event-stream`; each frame's `data:` is a runner event (`{type, payload}` only). Stream end = invocation end. The TURN ends when `NO_MORE_ACTIONS` is emitted (SDK driver: generator exhaustion, or after an unhandled error's ERROR event); an approval pause leaves `TOOL_APPROVAL_REQUIRED` as the final frame and does NOT end the turn. The transcript already includes this invocation's `INVOCATION_STARTED` — the canonical messages view already contains the current user message.

### Request body

| field | type | req | description |
|---|---|---|---|
| `session_id` | UUID | ✔ | Session being invoked. |
| `invocation_id` | UUID | ✔ | This turn's invocation; matches the INVOCATION_STARTED event. |
| `route` | string | ✔ | Agent route being invoked, e.g. "/customer-support". |
| `transcript` | array of event envelopes | ✔ |  |
| `reason` | invocationReason | ✔ | Why this invocation exists — mirrors the INVOCATION_STARTED event's reason. |

### Invocation reasons (`reason`)

| kind | fields | meaning |
|---|---|---|
| `user_message` | `input` | Starts a turn (first or follow-up); `input` is the user's message text. |
| `approval_decision` | `decision` | Resumes a turn suspended by TOOL_APPROVAL_REQUIRED; `decision` = `{approved, approver?}`. |
| `tool_result` | `tool_call_id`, `result` | Post-MVP: resumes with an externally-computed tool output. |


## Sidecar↔server protocol (Contract 4)

One WebSocket per sidecar container, dialed by the sidecar (`sidecar.schema.json`); JSON frames, no heartbeat frames — ingest traffic refreshes the lease. Reconnect: replay the unacked batch, dedupe by event id.

| kind | direction | carries |
|---|---|---|
| `ready` | sidecar→server | First frame after dialing; auth is the ingest token on the WS upgrade. |
| `event` | sidecar→server | Validated runner events (newEvent) in stream order; ONE batch in flight. Receiving it IS the lease heartbeat (heartbeat-before-append). |
| `complete` | sidecar→server | Turn ended: completed / suspended / failed / ingest_lost. The journal already carries the end; server releases the lease. |
| `assignment` | server→sidecar | One invocation: transcript (incl. INVOCATION_STARTED), reason, env (secrets), runner spawn spec, approval tools. |
| `ack` | server→sidecar | Batch journaled: per-event assigned seq + duplicate flag (exactly-once reconnect replay). |
| `abort` | server→sidecar | Turn is no longer this sidecar's (reconciler reclaim / fatal ingest error); the sink fails and the turn task exits. |


## Sidecar serve mode (HTTP)

`sectr-worker --serve` on the customer's own serverless or container platform (`serve.schema.json`). A wake is held open for the whole turn, which then runs over Contract 4 as above. The sidecar never answers 429 or 503 itself.

| message | route | carries |
|---|---|---|
| `identity` | `GET /` → 200 | `sectr_sidecar.version` and `mode: serve`; the platform's readiness check. |
| `wakeRequest` | `POST /wake` | The turn's ingest token. The sidecar dials its OWN `SECTR_INGEST_URL`, never a URL from the request. |
| `wakeResponse` | `POST /wake` → 200 | Sent when the invocation ends: `turn_ended`, or `token_rejected` (401 at the dial, nothing ran). |
| `manifestRequest` | `POST /manifest` | A registration token and the app's entry point. |
| `manifestResponse` | `POST /manifest` → 200 | `manifest` (+ `uses_uv`), or `token_rejected` / `dial_failed` (never imported) / `import_failed`. |
| `error` | 400 / 409 / 413 / 500 | `bad_request`, `busy` (one turn per instance, never queued), `too_large` (bodies over 4 KiB), `internal` (the sidecar itself failed). |


## Worked example — approval pause

The turn below is SUSPENDED by `TOOL_APPROVAL_REQUIRED` (no NO_MORE_ACTIONS — the gated tool is a pending action); the platform journals the decision, then re-invokes with `reason: approval_decision` (fresh runner, full transcript).

```json
{
  "id": "0192f3a4-b600-7000-8000-000000000006",
  "session_id": "0192f3a4-b5c6-7e2f-8a9b-1c2d3e4f5a6b",
  "deployment_id": "0192f3a0-1111-7abc-9def-0123456789ab",
  "seq": 6,
  "type": "TOOL_APPROVAL_REQUIRED",
  "payload": {
    "tool_call_id": "call_01",
    "name": "lookup_order",
    "arguments": {
      "order_id": "A-1234"
    },
    "reason": "Order lookup requires manager approval"
  },
  "created_at": "2026-01-15T12:00:06.123456Z"
}
```


## Fixture library

Golden files under `fixtures/` feed conformance checks and the Rust/Python typed-model tests:

- `fixtures/valid/{event,runner-event,invocation,sidecar,serve}/` — must validate
- `fixtures/invalid/{event,runner-event,invocation,sidecar,serve}/` — must be rejected
- Check everything: `uv run pytest` (from `schema/`)
