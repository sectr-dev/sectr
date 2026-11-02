# Sectr contracts

The files in this directory define every contract that crosses a component
boundary. Components conform to them and never redefine them. This page holds
the rules the schemas can't express. For a field-by-field reference generated
from the schemas, see [`REFERENCE.md`](REFERENCE.md).

| File | What it defines | Used by |
|---|---|---|
| `event.schema.json` | Event envelope and per-type payloads | `sectr-common`, `sdk-python`, dashboard |
| `invocation.schema.json` | Runner invoke request and stream | `sectr-worker`, `sdk-python` |
| `sidecar.schema.json` | Sidecar to server WebSocket frames | `sectr-server`, `sectr-worker` |
| `sessions-api.yaml` | Session API, without auth | Composed into the two below |
| `public-api.yaml` | Full server API: sessions plus control plane | `sectr-server`, dashboard, `sectr-cli` |
| `dev-api.yaml` | Local `sectr dev` gateway: sessions plus `/dev/*`, no auth | `sectr-cli` |
| `agui-mapping.md` | Journal to AG-UI 1.0 mapping (normative) | `sectr-common`, `js/copilotkit` |
| `data-model.sql` | Canonical tables for the journal and registry | Migrations in `sectr-common` |

Rust and Python types are written by hand and kept honest by golden tests over
`fixtures/`, rather than generated. The type surface is small, so codegen would
add a build step for little gain.

## Event envelope

Every journal row and every SSE frame sent to clients has:

| Field | Type | Notes |
|---|---|---|
| `id` | UUIDv7 | Globally unique, time-ordered. Minted by the event's producer, deduped by the journal writer |
| `session_id` | UUID | |
| `deployment_id` | UUID | The deployment the session is pinned to |
| `seq` | int64 | Per session, starts at 0 with `SESSION_CREATED`, gap-free within a turn. Assigned by the journal writer (the server's ingest path), never by user code |
| `type` | string enum | Closed. Adding a type is a minor version bump of `event.schema.json` |
| `payload` | object | Schema per event type |
| `created_at` | timestamptz | |

Rules:

- **Forward compatibility.** Unknown fields are preserved, not stripped, so old
  readers never break. No schema uses `additionalProperties: false`; strictness
  comes from required fields, enums and types. The journal stores unknown
  envelope fields in `events.extra`.
- **Resumption.** An SSE frame's `id:` is the event `id`, which enables
  `Last-Event-ID` catch-up.
- **Runner events** are `{type, payload}` only, restricted to the runner-emitted
  types (`$defs/runnerEvent`). The platform assigns the rest when journaling.
- **Integration extras** go under one namespace key named for the integration
  (`ag_ui`, ...), never as top-level payload fields. Core fields stay
  un-namespaced.

## Turn semantics

- **Invocation:** one runner process execution, from spawn to stream end. It has
  exactly one `reason`: `user_message` (starts a turn), `approval_decision`
  (resumes a suspended turn), or `tool_result` (reserved for externally executed
  tools).
- **Turn:** a unit of user intent. It starts with a `user_message` invocation and
  **ends when `NO_MORE_ACTIONS` is journaled within it**.
- **Suspension:** an approval pause suspends the turn. `TOOL_APPROVAL_REQUIRED`
  is then the last runner event, no sentinel is emitted, and the gated tool is a
  pending action.
- **Errors end the turn.** After `ERROR` or `SESSION_ERROR` the platform
  synthesizes the sentinel, so the rule stays uniform.
- **The runner's transcript includes its own `INVOCATION_STARTED`,** which
  carries the current user message. User code never re-appends it.
- The SDK driver (`sectr.drive`) emits the sentinel when the handler finishes or
  raises. Runners without the SDK must end their stream with `NO_MORE_ACTIONS`
  themselves.

## Crash mid-turn

If a runner crashes mid-turn, the journal holds a partial message (deltas with no
`MESSAGE_END`), followed by `SESSION_ERROR` and `NO_MORE_ACTIONS`. The next
invocation's runner sees that transcript.

1. **The journal is never rewritten, and deltas are never transient.** Client SSE
   is always a journal tail, and `Last-Event-ID` replays the journal. Whether a
   partial message counts for the model is decided by the fold, not the write
   path.
2. **The canonical fold continues with a synthesized user message.** A trailing
   unterminated assistant message is emitted with its partial content, followed
   by: *"An error interrupted the generation of your previous message. Please
   continue from where it left off."* This is portable across providers, unlike
   prefill. It's derived from the journal and never persisted, so the dashboard
   doesn't show it.
3. **Every prefix of a valid journal is a valid journal.** A crash can cut the
   journal anywhere, including between a `TOOL_CALL` and its `TOOL_RESULT`.
   Providers reject a tool call with no result, so the fold answers the gap: a
   `TOOL_CALL` with no `TOOL_RESULT` that's followed by more conversation gets a
   synthesized output (*"This tool call did not complete; no result was
   recorded."*), placed right after that call's real results, and never
   persisted. A *trailing* unanswered call is a suspended approval and is left for
   the approval path.
4. **A later `user_message` abandons an unanswered call.** `pending_tool_call`
   stops reporting it, and a late decision gets a `409`. Accepting a decision has
   a stricter rule (`journal::awaiting_approval`): the session's last event must
   be the `TOOL_APPROVAL_REQUIRED`. So a call is decided exactly once, even if its
   resume crashed before a `TOOL_RESULT`.

**Where the fold lives.** The sidecar sends the runner the raw journal, every
delta included, with no folding or filtering. There are several legitimate folds
over one journal (LLM context, dashboard, CLI), and the wire format doesn't choose
one. The stitching above is SDK behavior in `SessionContext.messages`, below the
framework adapters. A runner without the SDK folds its own way.

## Approvals

- **Resume with a decision.** The platform never executes tools. After a decision
  the platform invokes a fresh runner with `reason: approval_decision`. The runner
  rebuilds its conversation from the transcript, runs the tool itself and journals
  `TOOL_RESULT`. That's why `TOOL_CALL` must journal `name` and `arguments`.
- **The decision is its own event:** `TOOL_APPROVAL_GRANTED` or
  `TOOL_APPROVAL_DENIED`.
- **Policy has one source:** `[deploy] approval_tools` in `sectr.toml`, in dev and
  deployed. The platform passes it to the user process as `SECTR_APPROVAL_POLICY`,
  a JSON list of gated tool names. It's an error for a user to set that variable.
  The effective gate is code-level marking (`@tool(needs_approval=True)`, native
  framework idioms) **plus** the policy, so the policy still gates a tool whose
  decorator was forgotten.
- **One decision per resume.** Frameworks can produce several gated calls in one
  model response. Adapters turn them into sequential approvals, one per resume,
  because the contract has no batch shape yet.

## Journal content

- **Deltas are interpretable.** `MESSAGE_DELTA.delta` is `{type, text}` with a
  closed vocabulary: `text` and `reasoning`. The platform must be able to interpret
  every delta from the journal alone, for example to serve AG-UI. Adapters normalize
  framework chunks before journaling. Tool arguments don't stream: `TOOL_CALL`
  journals complete arguments, which approval resume needs anyway.
- **A user message's `input` is a non-blank string, and nothing else.** Any other
  value is a `400` on session create, `/input` and AG-UI's trailing user message,
  and is never journaled. User inputs are journaled in `INVOCATION_STARTED.input`
  (absent on approval resumes).
- **`TOOL_RESULT` is runner-emitted, after every tool execution,** so the transcript
  can rebuild the conversation. A later turn must send the model exactly what the
  first turn sent, or the provider's prompt cache misses. So the adapter journals
  what the framework *stores* for the result, when that survives the journal
  unchanged. A string always does. An object does only where the provider reads its
  fields rather than its text, because the journal sorts object keys. Otherwise the
  adapter makes the framework store a form that does. The conformance `results` and
  `mcp` cases check both turns.
- **`MESSAGE_END.framework_state` is opaque and adapter-owned.** It's what the
  adapter that wrote it needs to replay that one model response as its framework
  stored it, beyond text and tool calls, such as signed reasoning.
  - It holds the framework's own objects, never provider payloads, so no provider
    vocabulary enters the schema.
  - Its shape is `{adapter, v, data}`. Only the adapter that wrote it reads it, and
    that adapter drops any blob it can't read.
  - The journal keeps strings byte-exact but objects and numbers only by value, so
    anything that must come back exactly is stored as a string.
  - It's capped at 256 KiB, by the SDK and again by the worker, which drops a larger
    one and still journals the event.
  - It isn't part of the AG-UI mapping.
- **The journal stores any string, NUL included.** Postgres `jsonb` can't hold U+0000,
  so `PgJournal` escapes it in `payload` and `extra` and unescapes on read (scheme in
  `data-model.sql`). This is a storage detail, with no API or event schema change.

## Compaction

The journal stays append-only. Compaction affects only the LLM context view, which
is a positional fold: a `TRANSCRIPT_COMPACTED` event **resets** the accumulator to
its `messages`, so the context is always the last snapshot plus everything after it.

- The agent framework computes the compaction. The adapter captures the result and
  journals it as a runner-emitted event.
- The payload is a full context snapshot, not a prose summary, so any algorithm
  replays faithfully. `replaces_up_to_seq` is metadata.
- A snapshot must not drop an unanswered tool call.
- Snapshots are journal events, so dashboards can show a "history compacted" marker.

## Runner invoke contract

This is the only thing a user process must implement (`invocation.schema.json`):

```
POST /invocations        # sidecar to user process, on localhost:$SECTR_PORT
{ session_id, invocation_id, route, transcript, reason }
-> 200, text/event-stream, frames = runner events
```

- The process reads its port from `$SECTR_PORT` and binds localhost. The sidecar is
  the only expected caller.
- The stream ending is the invocation ending. `TOOL_APPROVAL_REQUIRED` must be final.
- The process performs no journal or database access.
- Env vars for user code are injected by the sidecar. Platform credentials are never
  present in the user process's environment.

## Sidecar and server protocol

The sidecar holds **no database credentials**, only a short-lived ingest token scoped
to one turn. It dials the server over WebSocket, with JSON frames that reuse the schema
types (`sidecar.schema.json`):

- **Sidecar to server:** `ready`, `event`, `complete`.
- **Server to sidecar:** `assignment` (transcript, reason, env, deployment metadata,
  approval policy), `ack`, `abort`.
- **Lease heartbeat:** ingest traffic is the heartbeat. There's no heartbeat frame.
- **Reconnect:** the sidecar replays unacked events, and the server dedupes by event id,
  so ingest is exactly-once.
- **Lifetime:** one connection per sidecar process, for one turn. Whether a runner tears
  down at a turn boundary is one policy choice in shared code, and today it always does.
- **Dev:** `sectr dev` hosts the same engine in-process and bypasses this protocol.

## API contract layering

Three OpenAPI files, composed by external `$ref`:

- `sessions-api.yaml` is the session core and has no auth knowledge. It's the one
  client-facing contract for live agent interaction.
- `public-api.yaml` adds the control plane and auth. Core operations that data-plane
  credentials may call are re-declared inline with their `security`, because a path-item
  `$ref` can't carry it. A conformance test fails any other difference from the core.
- `dev-api.yaml` is the core, verbatim, plus `/dev/*` extras, with no auth.

The dashboard uses one data layer in production and local dev. A change to a shared
shape edits `sessions-api.yaml` first.

## Deploys

Deploys are asynchronous. `POST /deployments` answers `202` with the row in `building`,
and the build runs in a background task. The row moves to `active` or `failed`, and an
active deployment becomes `retired` when replaced. Clients poll
`GET /apps/{appId}/deployments`. A failed row carries an optional `error` with a truncated
tail of the output. Rows orphaned by a server restart are marked failed at startup.
Validation problems are still synchronous `4xx` errors. A `building` deployment never takes
routing.

Two artifacts are called "manifest":

1. **`sectr.toml`** is the developer's project file, uploaded as-is.
2. **The registry manifest** is stored in `deployments.manifest` and composed by the server's
   build step. It combines a real import inside the built image (`routes[]`, `env[]`, ...)
   with platform fields (`runner.*`, `approval_tools`). Its shape lives in
   `crates/sectr-server/src/deploy.rs`.

Tarball members are relative to the project root, with no wrapper directory.

## Secrets

Secrets are per app. `/secrets` takes a required `app` query parameter (the app's slug). Key
names must be env-safe (`[A-Za-z_][A-Za-z0-9_]*`). They're write-only: no endpoint ever
returns a value, and values leave the store only through the assignment's env.

## AG-UI

Every deployed route is also an AG-UI 1.0 endpoint, `POST /ag-ui/{app}/{route}`, served as
another view over the journal. [`agui-mapping.md`](agui-mapping.md) is normative, pinned by
golden fixtures in `fixtures/{valid,invalid}/agui/` and checked against the vendored upstream
schema. `threadId` is the session id, and `TOOL_APPROVAL_REQUIRED`'s journal id is the
interrupt id.

`GET /ag-ui/{app}/{route}?threadId=` (connect) re-streams the thread's latest run for a client
that holds nothing, such as a reloaded page. This is a Sectr extension.

## Limits

Limits are server policy, and the `sectr dev` gateway never returns them. A rejection happens
before the turn journals anything, so retrying is safe, with one exception: a `POST /sessions`
that loses the atomic lease claim gets a `429` after its session was created, leaving an idle
session behind.

`429` applies to `POST /sessions`, `POST /sessions/{id}/input`, `POST /sessions/{id}/approval`
and AG-UI runs, with `{error, code}` and `Retry-After`:

- `concurrency_limit`: the app is at its concurrent-turn limit.
- `platform_capacity`: the platform-wide turn limit is reached.
- `rate_limited`: session creation is rate limited, per `(app, external_user_id)` for a user
  token and per app for an invocation key. Platform principals are exempt.

A "running turn" is a live lease. A suspended turn holds no lease, but its approval resume needs
one.

`403 {code: "beta_full"}` on `POST /auth/clerk` means the signup cap was reached and the sign-in
would create a new org. The `429` on `POST /tokens` is a separate limit with no `code`.

## Credentials

Three bearer key classes, stored as hashed opaque rows in `api_keys`:

| Prefix | Class | Used by |
|---|---|---|
| `skp_` | Platform | CLI and dashboard, for the whole org |
| `ski_` | Invocation | An app's trusted backend, for that app only. Mints user tokens |
| `sks_` | User | A browser, for one end user of one app |

**User tokens.** `POST /tokens {app, external_user_id}` with the app's invocation key returns a
token, shown once. Sectr never authenticates end users; the backend asserts the user id after its
own auth.

- Tokens have a hard expiry (default 24 hours). Revoking the minting key revokes its tokens.
- `sessions.external_user_id` is stamped at creation and immutable. A user token can use its own
  sessions only; foreign and ownerless sessions are `404`.
- `GET /approvals` is platform-only.
- `GET /sessions/{id}/events` also accepts `?token=`, because EventSource can't set headers. That
  stream ends at the token's expiry or on revocation.

**Dashboard sessions.** On hosted Sectr the dashboard signs in with Clerk and exchanges the session
at `POST /auth/clerk` for an ordinary platform key with a hard expiry. `users.clerk_user_id` is the
account's only identity key; there's no linking by email. `POST /auth/logout` revokes the presented
key only if it has an expiry, so it never kills a long-lived key.

## Reserved: mutable agent state

Not built. Agents keep mutable state in the transcript, for example a `todo_write` tool whose result
lives in the conversation, which rehydrates and survives crashes for free. A future `STATE_PATCH`
event would define a closed set of operations with state as a deterministic fold over the journal.
User-defined reducers stay out, because they'd bring back code-replay determinism.
