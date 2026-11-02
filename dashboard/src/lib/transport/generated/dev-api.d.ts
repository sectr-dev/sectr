/* GENERATED from schema/dev-api.yaml (composes sessions-api.yaml) — do not edit.
   Run `npm run gen:api` after any contract change. */
export interface paths {
    "/sessions": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** List sessions (sessions tables, approvals inbox source) */
        get: operations["listSessions"];
        put?: never;
        /**
         * Create a session and start its first turn
         * @description Starts the turn immediately with `input` as a user_message reason. The creating principal (prod: invocation key, user token or platform key; dev: none) determines the server-derived `Session.origin`.
         */
        post: operations["createSession"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/sessions/{sessionId}": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** Session metadata (inspector meta bar, tables) */
        get: operations["getSession"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/sessions/{sessionId}/events": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /**
         * Journal tail (SSE) — the ONLY live view; never proxied from a runner
         * @description `text/event-stream`. Each frame's `data:` is one event envelope (event.schema.json); the SSE `id:` is the event's UUIDv7 `id`. Replays the full journal from seq 0, then holds the tail open. Resumption: standard `Last-Event-ID` header (or `last_event_id` query param for first-connect) → replay `WHERE seq > $1`.
         */
        get: operations["streamSessionEvents"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/sessions/{sessionId}/input": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Send a user message (follow-up turn)
         * @description Journaled as an INVOCATION_STARTED with reason user_message. 202 means the turn is SCHEDULED — turns run asynchronously; follow the SSE journal tail for the output. An input after an ERRORED turn is accepted (a fresh turn is the recovery path).
         */
        post: operations["sendSessionInput"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/sessions/{sessionId}/approval": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Decide a pending approval (banner, approvals inbox)
         * @description Journaled as TOOL_APPROVAL_GRANTED/DENIED, then the platform resumes the turn with an approval_decision invocation.
         */
        post: operations["decideApproval"];
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/approvals": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** Pending approvals across all apps (admin approvals inbox) */
        get: operations["listPendingApprovals"];
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/dev/status": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        /** Gateway/engine info (client handshake + dashboards) */
        get: {
            parameters: {
                query?: never;
                header?: never;
                path?: never;
                cookie?: never;
            };
            requestBody?: never;
            responses: {
                /** @description OK */
                200: {
                    headers: {
                        [name: string]: unknown;
                    };
                    content: {
                        "application/json": components["schemas"]["DevStatus"];
                    };
                };
            };
        };
        put?: never;
        post?: never;
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
    "/dev/reset": {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        get?: never;
        put?: never;
        /**
         * Wipe all local dev state (sessions, events, leases)
         * @description Deletes the project's `.agent/dev.db` contents in place (bootstrap ids in `dev.json` survive; deployments are kept). For demos and iteration — never exists in the public contract.
         */
        post: {
            parameters: {
                query?: never;
                header?: never;
                path?: never;
                cookie?: never;
            };
            requestBody?: never;
            responses: {
                /** @description state wiped; gateway still serving */
                204: {
                    headers: {
                        [name: string]: unknown;
                    };
                    content?: never;
                };
            };
        };
        delete?: never;
        options?: never;
        head?: never;
        patch?: never;
        trace?: never;
    };
}
export type webhooks = Record<string, never>;
export interface components {
    schemas: {
        /** @description Identity of the running dev deployment — only what clients consume (badge, app synthesis, session creation). Internal bookkeeping (deployment version, entry) stays in .agent/dev.json. */
        DevStatus: {
            /** @description project dir basename */
            project: string;
            /** @description app name from the manifest */
            app: string;
            /** @description the registry id sessions are stamped with — clients join session rows to the app by THIS, not by name */
            app_id: string;
            gateway_port: number;
            /** @description framework adapter reported by the SDK inspector (the handler's `sectr_framework` marker, e.g. openai-agents); absent when the handler carries no marker */
            framework?: string;
            /** @description routable paths from the manifest — clients need one to create a session (`POST /sessions` requires app + route), so the status carries them rather than forcing clients to re-inspect the app. */
            routes: string[];
        };
        /** @enum {string} */
        SessionStatus: "active" | "awaiting_approval" | "ended" | "error";
        Session: {
            /** Format: uuid */
            id: string;
            /** @description dashboard joins app_name client-side */
            app_id: string;
            /** @description the agent this session talked to */
            route: string;
            /**
             * @description server-derived from the creating principal
             * @enum {string}
             */
            origin: "api" | "playground";
            deployment_id: string;
            status: components["schemas"]["SessionStatus"];
            /** Format: date-time */
            created_at: string;
            /** Format: date-time */
            last_event_at: string;
            event_count: number;
            /** @description Display label, server-computed: today the first user message's text, truncated. Display it, don't parse it: the derivation may change. Absent when there's no text to show (no user message yet, or one without text). */
            title?: string;
            /** @description owner — the end user whose browser token created the session; absent for backend/platform-created sessions */
            external_user_id?: string;
        };
        NewSessionRequest: {
            /** @description app id or name */
            app: string;
            /** @description agent route, e.g. /customer-support */
            route: string;
            /** @description The user's message text, journaled verbatim as INVOCATION_STARTED.reason.input. A non-string, or a string of only whitespace, is a 400. */
            input: string;
        };
        SessionCreated: {
            /** Format: uuid */
            session_id: string;
        };
        /** @description The standard `{error}` body, plus an optional machine-readable `code` on errors that have one (additive: existing errors carry no code). */
        ErrorWithCode: {
            /** @description human-readable message */
            error: string;
            /** @description `concurrency_limit` (the app's concurrent-turn cap), `platform_capacity` (the platform-wide cap), `rate_limited` (session-creation rate limit), `beta_full` (signup cap; 403 on POST /alpha/provision). */
            code?: string;
        };
        /** @description Runner-emitted. Start of an assistant message. */
        MESSAGE_START: {
            /** @description e.g. "assistant"; opaque to the platform. */
            role: string;
            /** @description Runner-scoped message identifier; correlates START/DELTA/END. */
            message_id: string;
        };
        /** @description Runner-emitted. Content delta for a message. Deltas are INTERPRETABLE, not opaque: the platform (e.g. a future AG-UI-compatible endpoint) must be able to reconstruct both the exact transcript and its streaming semantics from the journal alone. Tool-argument streaming is deliberately NOT a delta kind — TOOL_CALL journals complete arguments, which a resumed runner executes from journal data alone. */
        MESSAGE_DELTA: {
            message_id: string;
            /** @description Interpretable content delta: {type, text} with a CLOSED kind vocabulary — `text` (maps to AG-UI TEXT_MESSAGE_CONTENT) and `reasoning` (maps to THINKING_CONTENT). One wire shape only; unknown kinds are invalid: framework chunk types outside the vocabulary must be normalized by the SDK adapter before journaling (e.g. Anthropic input_json_delta accumulates into TOOL_CALL.arguments). */
            delta: {
                /** @enum {unknown} */
                type: "text" | "reasoning";
                text: string;
            };
        };
        /** @description Runner-emitted. End of an assistant message. */
        MESSAGE_END: {
            message_id: string;
            /** @description Opaque to the platform, e.g. "stop", "length", "tool_use". */
            finish_reason?: string;
            /** @description What the adapter that wrote it needs to rehydrate and replay THIS model response as its agent framework stored it, beyond the message's text and tool calls (e.g. signed reasoning); never state of other responses or of the run as a whole. Its `data` is opaque to the platform, which stores and serves it but never interprets it; only the adapter named by `adapter` reads it back. At most 256 KiB serialized: the platform drops a larger one. */
            framework_state?: {
                /** @description The adapter that wrote it, e.g. "langgraph". */
                adapter: string;
                /** @description The adapter's format version for `data`. */
                v: number;
                /** @description The adapter's own data, in its format `v`. Opaque to the platform. The journal keeps a string value exactly, byte for byte, but an object or a number only by value: an object's keys may come back in another order, and a number in another notation. So a value the adapter needs back exactly (e.g. text a provider gets verbatim) is stored as a string; structure it only reads may be stored as JSON. */
                data: Record<string, never>;
            };
        };
        /** @description Runner-emitted. Journal record of a tool call initiated inside the runner — the platform NEVER executes tools. Arguments are opaque JSON (conventionally an object) journaled verbatim; name + arguments must be sufficient for a resumed runner to execute an approved call from journal data alone. */
        TOOL_CALL: {
            tool_call_id: string;
            name: string;
            arguments: unknown;
        };
        /** @description Runner-emitted. Requests human approval for a tool call. MUST be the final runner event of a turn; the turn ends with TURN_END{stop_reason: "awaiting_approval"}. */
        TOOL_APPROVAL_REQUIRED: {
            tool_call_id: string;
            name: string;
            arguments: unknown;
            /** @description Human-readable explanation shown in the approval inbox. */
            reason?: string;
        };
        /** @description Runner-emitted. Runner-level error; ends the turn (followed by TURN_END{stop_reason: "error"}). */
        ERROR: {
            message: string;
            code?: string;
            /** @description True if a retry of the same invocation may succeed. */
            transient?: boolean;
        };
        /** @description Runner-emitted (synthesized by the SDK driver on generator exhaustion or after an unhandled error's ERROR event; synthesized by the sidecar after SESSION_ERROR on a runner crash). THE turn-ending sentinel: 'the agent has no pending actions — the ball is in the user's court.' Deliberately NOT emitted on an approval pause: a gated tool is a pending action, so the turn is suspended, not over. Raw (no-SDK) runners MUST end their stream with this event when the turn is complete. */
        NO_MORE_ACTIONS: Record<string, never>;
        /** @description Canonical lowercase UUID (any version). */
        uuid: string;
        /** @description Platform-emitted (seq 0 of a session). Optional `external_user_id`: the session's owner when created via a user-scope browser token — the app-defined end-user identity; absent for backend/platform-created sessions. */
        SESSION_CREATED: {
            app_id: components["schemas"]["uuid"];
            route: string;
            /** @description owner identity: the (app-scoped) end-user the session belongs to; absent when backend/platform-created */
            external_user_id?: string;
        };
        /** @description Why an invocation exists. `tool_result` kind is specified for post-MVP externally-executed tools (the platform never executes tools in MVP). */
        invocationReason: {
            /** @constant */
            kind: "user_message";
            /** @description The user's message text. Starts a turn (first or follow-up). */
            input: string;
        } | {
            /** @constant */
            kind: "approval_decision";
            decision: {
                approved: boolean;
                approver?: string;
            };
        } | {
            /** @constant */
            kind: "tool_result";
            tool_call_id: string;
            /** @description Output of an externally-executed tool (post-MVP). */
            result: unknown;
        };
        /** @description Platform-emitted. One per invocation, appended BEFORE the sidecar fetches the transcript — so the runner's transcript already includes its own INVOCATION_STARTED, and the canonical messages view already contains the current user message. `reason` says why this invocation exists. */
        INVOCATION_STARTED: {
            invocation_id: components["schemas"]["uuid"];
            route: string;
            deployment_id: components["schemas"]["uuid"];
            reason: components["schemas"]["invocationReason"];
        };
        /** @description Runner-emitted. Journaled after every SUCCESSFUL tool execution — normal mid-turn calls and post-approval executions alike — and for a denied approval (the rejection text). A model conversation needs an output for every function_call, but the journal does not guarantee one: every prefix of a valid journal is itself valid (a crash can cut it anywhere, including between TOOL_CALL and TOOL_RESULT), and a tool that fails journals no TOOL_RESULT. The canonical fold supplies the missing output: a TOOL_CALL with no TOOL_RESULT that is followed by more conversation gets a synthesized, never-persisted tool output; a trailing one is a suspended approval. The approval decision itself is journaled by the platform as TOOL_APPROVAL_GRANTED/DENIED. */
        TOOL_RESULT: {
            tool_call_id: string;
            result: unknown;
        };
        /** @description Platform-emitted. The approval decision itself, journaled. */
        TOOL_APPROVAL_GRANTED: {
            tool_call_id: string;
            approver?: string;
        };
        /** @description Platform-emitted. The approval decision itself, journaled. */
        TOOL_APPROVAL_DENIED: {
            tool_call_id: string;
            approver?: string;
        };
        /** @description Platform-emitted. Platform-level failure (runner crash, timeout, lease expiry). */
        SESSION_ERROR: {
            message: string;
        };
        /** @description Runner-emitted when the agent framework compacts context (framework's algorithm, framework's model access — the SDK/adapter captures the result and journals it mid-stream). Payload is the FULL post-compaction context in canonical message form: context assembly = these messages + the canonical view of everything journaled after this event (positional fold; `replaces_up_to_seq` is metadata — the journal head at compaction time). Constraint: the snapshot must not drop an unanswered tool call (an open function_call without its output is an invalid context on replay). */
        TRANSCRIPT_COMPACTED: {
            replaces_up_to_seq: number;
            /** @description OpenAI-style message dicts (the canonical view) — the entire new context, framework compaction output normalized. */
            messages: Record<string, never>[];
        };
        /**
         * Sectr journal event envelope
         * @description Canonical event model for Sectr. Every journal row and every client SSE frame is one envelope. The envelope ties a closed `type` enum to a per-type payload schema via the `typedEvent` oneOf.
         *
         *     Forward compatibility: `additionalProperties` is deliberately NOT false anywhere in this schema — payload/envelope objects may carry unknown fields, and readers must preserve them, so events written by newer writers remain valid for older readers.
         *
         *     `seq` is a per-session monotonic integer assigned by the journal writer (the server's ingest path), gap-free within a turn, starting at 0. It is never supplied by user code. `id` is a UUIDv7 (time-ordered) minted by the platform's event producer (the engine — never user code) and deduped by the journal writer, enabling exactly-once reconnect replay; the SSE frame's `id:` field carries it, enabling `Last-Event-ID` resumption.
         *
         *     Runner-emitted event types may only appear on the SSE response stream of the invoke contract (see `invocation.schema.json` $defs/runnerEvent); platform-emitted types are written by the server/sidecar only.
         *
         *     Turn semantics: a turn starts with a user_message invocation and ends when NO_MORE_ACTIONS is journaled within it (also synthesized by the sidecar after SESSION_ERROR). An approval pause SUSPENDS the turn — TOOL_APPROVAL_REQUIRED is then the last runner event, and no NO_MORE_ACTIONS is emitted until the decision's resume invocation completes.
         */
        "event.schema": {
            id: components["schemas"]["uuidv7"];
            session_id: components["schemas"]["uuid"];
            deployment_id: components["schemas"]["uuid"];
            seq: number;
            type: components["schemas"]["eventType"];
            payload: unknown;
            created_at: components["schemas"]["timestamp"];
            $defs: {
                /** @description Canonical lowercase UUID (any version). */
                uuid: string;
                /** @description Lowercase UUIDv7 — version nibble 7, RFC 9562 variant. */
                uuidv7: string;
                /**
                 * Format: date-time
                 * @description RFC 3339 timestamp, always UTC (Z).
                 */
                timestamp: string;
                /** @enum {unknown} */
                eventType: "MESSAGE_START" | "MESSAGE_DELTA" | "MESSAGE_END" | "TOOL_CALL" | "TOOL_APPROVAL_REQUIRED" | "ERROR" | "NO_MORE_ACTIONS" | "SESSION_CREATED" | "INVOCATION_STARTED" | "TOOL_RESULT" | "TOOL_APPROVAL_GRANTED" | "TOOL_APPROVAL_DENIED" | "SESSION_ERROR" | "TRANSCRIPT_COMPACTED";
                /**
                 * @description The subset of event types a runner (user process) may emit over the invoke response stream. Everything else is platform-emitted. The turn ends when NO_MORE_ACTIONS is emitted; an approval pause (TOOL_APPROVAL_REQUIRED last) does NOT end the turn. TRANSCRIPT_COMPACTED is emitted by the SDK/adapter when the agent framework compacts context.
                 * @enum {unknown}
                 */
                runnerEventType: "MESSAGE_START" | "MESSAGE_DELTA" | "MESSAGE_END" | "TOOL_CALL" | "TOOL_RESULT" | "TOOL_APPROVAL_REQUIRED" | "ERROR" | "NO_MORE_ACTIONS" | "TRANSCRIPT_COMPACTED";
                /** @description Discriminated union: each branch pins `type` to a const and binds the payload to that type's schema. */
                typedEvent: {
                    /** @constant */
                    type: "MESSAGE_START";
                    payload: components["schemas"]["MESSAGE_START"];
                } | {
                    /** @constant */
                    type: "MESSAGE_DELTA";
                    payload: components["schemas"]["MESSAGE_DELTA"];
                } | {
                    /** @constant */
                    type: "MESSAGE_END";
                    payload: components["schemas"]["MESSAGE_END"];
                } | {
                    /** @constant */
                    type: "TOOL_CALL";
                    payload: components["schemas"]["TOOL_CALL"];
                } | {
                    /** @constant */
                    type: "TOOL_APPROVAL_REQUIRED";
                    payload: components["schemas"]["TOOL_APPROVAL_REQUIRED"];
                } | {
                    /** @constant */
                    type: "ERROR";
                    payload: components["schemas"]["ERROR"];
                } | {
                    /** @constant */
                    type: "NO_MORE_ACTIONS";
                    payload: components["schemas"]["NO_MORE_ACTIONS"];
                } | {
                    /** @constant */
                    type: "SESSION_CREATED";
                    payload: components["schemas"]["SESSION_CREATED"];
                } | {
                    /** @constant */
                    type: "INVOCATION_STARTED";
                    payload: components["schemas"]["INVOCATION_STARTED"];
                } | {
                    /** @constant */
                    type: "TOOL_RESULT";
                    payload: components["schemas"]["TOOL_RESULT"];
                } | {
                    /** @constant */
                    type: "TOOL_APPROVAL_GRANTED";
                    payload: components["schemas"]["TOOL_APPROVAL_GRANTED"];
                } | {
                    /** @constant */
                    type: "TOOL_APPROVAL_DENIED";
                    payload: components["schemas"]["TOOL_APPROVAL_DENIED"];
                } | {
                    /** @constant */
                    type: "SESSION_ERROR";
                    payload: components["schemas"]["SESSION_ERROR"];
                } | {
                    /** @constant */
                    type: "TRANSCRIPT_COMPACTED";
                    payload: components["schemas"]["TRANSCRIPT_COMPACTED"];
                };
                /** @description Runner-emitted. Start of an assistant message. */
                MESSAGE_START: {
                    /** @description e.g. "assistant"; opaque to the platform. */
                    role: string;
                    /** @description Runner-scoped message identifier; correlates START/DELTA/END. */
                    message_id: string;
                };
                /** @description Runner-emitted. Content delta for a message. Deltas are INTERPRETABLE, not opaque: the platform (e.g. a future AG-UI-compatible endpoint) must be able to reconstruct both the exact transcript and its streaming semantics from the journal alone. Tool-argument streaming is deliberately NOT a delta kind — TOOL_CALL journals complete arguments, which a resumed runner executes from journal data alone. */
                MESSAGE_DELTA: {
                    message_id: string;
                    /** @description Interpretable content delta: {type, text} with a CLOSED kind vocabulary — `text` (maps to AG-UI TEXT_MESSAGE_CONTENT) and `reasoning` (maps to THINKING_CONTENT). One wire shape only; unknown kinds are invalid: framework chunk types outside the vocabulary must be normalized by the SDK adapter before journaling (e.g. Anthropic input_json_delta accumulates into TOOL_CALL.arguments). */
                    delta: {
                        /** @enum {unknown} */
                        type: "text" | "reasoning";
                        text: string;
                    };
                };
                /** @description Runner-emitted. End of an assistant message. */
                MESSAGE_END: {
                    message_id: string;
                    /** @description Opaque to the platform, e.g. "stop", "length", "tool_use". */
                    finish_reason?: string;
                    /** @description What the adapter that wrote it needs to rehydrate and replay THIS model response as its agent framework stored it, beyond the message's text and tool calls (e.g. signed reasoning); never state of other responses or of the run as a whole. Its `data` is opaque to the platform, which stores and serves it but never interprets it; only the adapter named by `adapter` reads it back. At most 256 KiB serialized: the platform drops a larger one. */
                    framework_state?: {
                        /** @description The adapter that wrote it, e.g. "langgraph". */
                        adapter: string;
                        /** @description The adapter's format version for `data`. */
                        v: number;
                        /** @description The adapter's own data, in its format `v`. Opaque to the platform. The journal keeps a string value exactly, byte for byte, but an object or a number only by value: an object's keys may come back in another order, and a number in another notation. So a value the adapter needs back exactly (e.g. text a provider gets verbatim) is stored as a string; structure it only reads may be stored as JSON. */
                        data: Record<string, never>;
                    };
                };
                /** @description Runner-emitted. Journal record of a tool call initiated inside the runner — the platform NEVER executes tools. Arguments are opaque JSON (conventionally an object) journaled verbatim; name + arguments must be sufficient for a resumed runner to execute an approved call from journal data alone. */
                TOOL_CALL: {
                    tool_call_id: string;
                    name: string;
                    arguments: unknown;
                };
                /** @description Runner-emitted. Requests human approval for a tool call. MUST be the final runner event of a turn; the turn ends with TURN_END{stop_reason: "awaiting_approval"}. */
                TOOL_APPROVAL_REQUIRED: {
                    tool_call_id: string;
                    name: string;
                    arguments: unknown;
                    /** @description Human-readable explanation shown in the approval inbox. */
                    reason?: string;
                };
                /** @description Runner-emitted. Runner-level error; ends the turn (followed by TURN_END{stop_reason: "error"}). */
                ERROR: {
                    message: string;
                    code?: string;
                    /** @description True if a retry of the same invocation may succeed. */
                    transient?: boolean;
                };
                /** @description Platform-emitted (seq 0 of a session). Optional `external_user_id`: the session's owner when created via a user-scope browser token — the app-defined end-user identity; absent for backend/platform-created sessions. */
                SESSION_CREATED: {
                    app_id: components["schemas"]["uuid"];
                    route: string;
                    /** @description owner identity: the (app-scoped) end-user the session belongs to; absent when backend/platform-created */
                    external_user_id?: string;
                };
                /** @description Platform-emitted. One per invocation, appended BEFORE the sidecar fetches the transcript — so the runner's transcript already includes its own INVOCATION_STARTED, and the canonical messages view already contains the current user message. `reason` says why this invocation exists. */
                INVOCATION_STARTED: {
                    invocation_id: components["schemas"]["uuid"];
                    route: string;
                    deployment_id: components["schemas"]["uuid"];
                    reason: components["schemas"]["invocationReason"];
                };
                /** @description Runner-emitted. Journaled after every SUCCESSFUL tool execution — normal mid-turn calls and post-approval executions alike — and for a denied approval (the rejection text). A model conversation needs an output for every function_call, but the journal does not guarantee one: every prefix of a valid journal is itself valid (a crash can cut it anywhere, including between TOOL_CALL and TOOL_RESULT), and a tool that fails journals no TOOL_RESULT. The canonical fold supplies the missing output: a TOOL_CALL with no TOOL_RESULT that is followed by more conversation gets a synthesized, never-persisted tool output; a trailing one is a suspended approval. The approval decision itself is journaled by the platform as TOOL_APPROVAL_GRANTED/DENIED. */
                TOOL_RESULT: {
                    tool_call_id: string;
                    result: unknown;
                };
                /** @description Platform-emitted. The approval decision itself, journaled. */
                TOOL_APPROVAL_GRANTED: {
                    tool_call_id: string;
                    approver?: string;
                };
                /** @description Platform-emitted. The approval decision itself, journaled. */
                TOOL_APPROVAL_DENIED: {
                    tool_call_id: string;
                    approver?: string;
                };
                /** @description Platform-emitted. Platform-level failure (runner crash, timeout, lease expiry). */
                SESSION_ERROR: {
                    message: string;
                };
                /** @description Alias reference to this schema's root envelope, for cross-file $refs (e.g. invocation.schema.json transcripts). */
                envelope: components["schemas"]["event.schema"];
                /** @description One frame of the runner's SSE response stream in the invoke contract: `{type, payload}` only — `id`, `seq`, `session_id`, `deployment_id` and `created_at` are assigned by the journal writer (the server's ingest path) when the event is journaled. Restricted to runner-emitted types. */
                runnerEvent: {
                    type: components["schemas"]["runnerEventType"];
                } & components["schemas"]["typedEvent"];
                /** @description Full session history as journal envelopes, rehydrated by the platform and passed to the runner in the invoke request. */
                transcript: components["schemas"]["event.schema"][];
                /** @description Runner-emitted (synthesized by the SDK driver on generator exhaustion or after an unhandled error's ERROR event; synthesized by the sidecar after SESSION_ERROR on a runner crash). THE turn-ending sentinel: 'the agent has no pending actions — the ball is in the user's court.' Deliberately NOT emitted on an approval pause: a gated tool is a pending action, so the turn is suspended, not over. Raw (no-SDK) runners MUST end their stream with this event when the turn is complete. */
                NO_MORE_ACTIONS: Record<string, never>;
                /** @description Why an invocation exists. `tool_result` kind is specified for post-MVP externally-executed tools (the platform never executes tools in MVP). */
                invocationReason: {
                    /** @constant */
                    kind: "user_message";
                    /** @description The user's message text. Starts a turn (first or follow-up). */
                    input: string;
                } | {
                    /** @constant */
                    kind: "approval_decision";
                    decision: {
                        approved: boolean;
                        approver?: string;
                    };
                } | {
                    /** @constant */
                    kind: "tool_result";
                    tool_call_id: string;
                    /** @description Output of an externally-executed tool (post-MVP). */
                    result: unknown;
                };
                /** @description Runner-emitted when the agent framework compacts context (framework's algorithm, framework's model access — the SDK/adapter captures the result and journals it mid-stream). Payload is the FULL post-compaction context in canonical message form: context assembly = these messages + the canonical view of everything journaled after this event (positional fold; `replaces_up_to_seq` is metadata — the journal head at compaction time). Constraint: the snapshot must not drop an unanswered tool call (an open function_call without its output is an invalid context on replay). */
                TRANSCRIPT_COMPACTED: {
                    replaces_up_to_seq: number;
                    /** @description OpenAI-style message dicts (the canonical view) — the entire new context, framework compaction output normalized. */
                    messages: Record<string, never>[];
                };
            };
        } & components["schemas"]["typedEvent"];
        /** @description Discriminated union: each branch pins `type` to a const and binds the payload to that type's schema. */
        typedEvent: {
            /** @constant */
            type: "MESSAGE_START";
            payload: components["schemas"]["MESSAGE_START"];
        } | {
            /** @constant */
            type: "MESSAGE_DELTA";
            payload: components["schemas"]["MESSAGE_DELTA"];
        } | {
            /** @constant */
            type: "MESSAGE_END";
            payload: components["schemas"]["MESSAGE_END"];
        } | {
            /** @constant */
            type: "TOOL_CALL";
            payload: components["schemas"]["TOOL_CALL"];
        } | {
            /** @constant */
            type: "TOOL_APPROVAL_REQUIRED";
            payload: components["schemas"]["TOOL_APPROVAL_REQUIRED"];
        } | {
            /** @constant */
            type: "ERROR";
            payload: components["schemas"]["ERROR"];
        } | {
            /** @constant */
            type: "NO_MORE_ACTIONS";
            payload: components["schemas"]["NO_MORE_ACTIONS"];
        } | {
            /** @constant */
            type: "SESSION_CREATED";
            payload: components["schemas"]["SESSION_CREATED"];
        } | {
            /** @constant */
            type: "INVOCATION_STARTED";
            payload: components["schemas"]["INVOCATION_STARTED"];
        } | {
            /** @constant */
            type: "TOOL_RESULT";
            payload: components["schemas"]["TOOL_RESULT"];
        } | {
            /** @constant */
            type: "TOOL_APPROVAL_GRANTED";
            payload: components["schemas"]["TOOL_APPROVAL_GRANTED"];
        } | {
            /** @constant */
            type: "TOOL_APPROVAL_DENIED";
            payload: components["schemas"]["TOOL_APPROVAL_DENIED"];
        } | {
            /** @constant */
            type: "SESSION_ERROR";
            payload: components["schemas"]["SESSION_ERROR"];
        } | {
            /** @constant */
            type: "TRANSCRIPT_COMPACTED";
            payload: components["schemas"]["TRANSCRIPT_COMPACTED"];
        };
        /**
         * @description The subset of event types a runner (user process) may emit over the invoke response stream. Everything else is platform-emitted. The turn ends when NO_MORE_ACTIONS is emitted; an approval pause (TOOL_APPROVAL_REQUIRED last) does NOT end the turn. TRANSCRIPT_COMPACTED is emitted by the SDK/adapter when the agent framework compacts context.
         * @enum {unknown}
         */
        runnerEventType: "MESSAGE_START" | "MESSAGE_DELTA" | "MESSAGE_END" | "TOOL_CALL" | "TOOL_RESULT" | "TOOL_APPROVAL_REQUIRED" | "ERROR" | "NO_MORE_ACTIONS" | "TRANSCRIPT_COMPACTED";
        /** @description Lowercase UUIDv7 — version nibble 7, RFC 9562 variant. */
        uuidv7: string;
        /** @enum {unknown} */
        eventType: "MESSAGE_START" | "MESSAGE_DELTA" | "MESSAGE_END" | "TOOL_CALL" | "TOOL_APPROVAL_REQUIRED" | "ERROR" | "NO_MORE_ACTIONS" | "SESSION_CREATED" | "INVOCATION_STARTED" | "TOOL_RESULT" | "TOOL_APPROVAL_GRANTED" | "TOOL_APPROVAL_DENIED" | "SESSION_ERROR" | "TRANSCRIPT_COMPACTED";
        /**
         * Format: date-time
         * @description RFC 3339 timestamp, always UTC (Z).
         */
        timestamp: string;
        SessionInput: {
            /** @description The user's message text, journaled verbatim as INVOCATION_STARTED.reason.input. A non-string, or a string of only whitespace, is a 400. */
            input: string;
        };
        ApprovalDecision: {
            approved: boolean;
            /** @description attribution for the decision. Omitted, a user token's decision is attributed to its end user (`external_user_id`); other callers record none. */
            approver?: string;
        };
        PendingApproval: {
            session: components["schemas"]["Session"];
            /** @description the TOOL_APPROVAL_REQUIRED event envelope */
            event: components["schemas"]["event.schema"];
        };
    };
    responses: {
        /** @description `input` isn't a string, or is only whitespace. Nothing is journaled. */
        InvalidInput: {
            headers: {
                [name: string]: unknown;
            };
            content: {
                "application/json": components["schemas"]["ErrorWithCode"];
            };
        };
        /** @description concurrency, capacity or rate limit (server policy — the dev gateway never returns it). Retry the same request after the `Retry-After` delay. On `/input` and `/approval` nothing was journaled. On `POST /sessions` normally nothing was either, but if a concurrent request took the last slot between admission and the first turn's claim, an idle session (only `SESSION_CREATED`) may be left behind; it is listed by `GET /sessions` and accepts `/input` later. `code` says which limit tripped (see the body schema). */
        TooManyRequests: {
            headers: {
                /** @description seconds to wait before retrying */
                "Retry-After"?: number;
                [name: string]: unknown;
            };
            content: {
                "application/json": components["schemas"]["ErrorWithCode"];
            };
        };
    };
    parameters: never;
    requestBodies: never;
    headers: never;
    pathItems: never;
}
export type $defs = Record<string, never>;
export interface operations {
    listSessions: {
        parameters: {
            query?: {
                app_id?: string;
                status?: components["schemas"]["SessionStatus"];
                /** @description owner filter (backend/platform callers only; rejected for browser tokens, which are always scoped to their own user) */
                external_user_id?: string;
                limit?: number;
                /** @description RFC 3339; keyset pagination */
                created_before?: string;
            };
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description OK */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Session"][];
                };
            };
        };
    };
    createSession: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["NewSessionRequest"];
            };
        };
        responses: {
            /** @description created; turn started */
            201: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["SessionCreated"];
                };
            };
            400: components["responses"]["InvalidInput"];
            /** @description unknown app, no active deployment, or a route the deployment doesn't serve */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            429: components["responses"]["TooManyRequests"];
        };
    };
    getSession: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                sessionId: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description OK */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["Session"];
                };
            };
            /** @description unknown session */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
        };
    };
    streamSessionEvents: {
        parameters: {
            query?: {
                last_event_id?: string;
            };
            header?: never;
            path: {
                sessionId: string;
            };
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description SSE stream */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "text/event-stream": components["schemas"]["event.schema"];
                };
            };
        };
    };
    sendSessionInput: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                sessionId: string;
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["SessionInput"];
            };
        };
        responses: {
            /** @description turn scheduled */
            202: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            400: components["responses"]["InvalidInput"];
            /** @description unknown session */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description session not accepting input — mid-turn (a live lease holds the session), awaiting an approval decision (decide it first: a new message would strand the unanswered tool call in the conversation), or ended. NOT a 409 after an errored turn. */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            429: components["responses"]["TooManyRequests"];
        };
    };
    decideApproval: {
        parameters: {
            query?: never;
            header?: never;
            path: {
                sessionId: string;
            };
            cookie?: never;
        };
        requestBody: {
            content: {
                "application/json": components["schemas"]["ApprovalDecision"];
            };
        };
        responses: {
            /** @description decision journaled; resume scheduled */
            202: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description unknown session */
            404: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            /** @description no pending approval on this session */
            409: {
                headers: {
                    [name: string]: unknown;
                };
                content?: never;
            };
            429: components["responses"]["TooManyRequests"];
        };
    };
    listPendingApprovals: {
        parameters: {
            query?: never;
            header?: never;
            path?: never;
            cookie?: never;
        };
        requestBody?: never;
        responses: {
            /** @description OK */
            200: {
                headers: {
                    [name: string]: unknown;
                };
                content: {
                    "application/json": components["schemas"]["PendingApproval"][];
                };
            };
        };
    };
}

