/*
  The REAL transport — the typed client of a sectr gateway, implementing
  `SessionsApi` (see ./types.ts for the interface and wire conventions).

  Covers exactly `schema/sessions-api.yaml` (the auth-agnostic core): the
  six session paths + the journal-tail SSE stream. This ONE transport
  serves dev and prod — they differ only in config (base URL + credential
  provider), never in code. The Rust reference implementation of the same
  surface is sectr-cli's `GatewayClient`.

  Types are GENERATED from the contract (`./generated/sessions-api.d.ts`,
  via `npm run gen:api` — openapi-typescript). Contract first, regen, then
  this file.

  Wire conventions from the contract:
  - POST /input + /approval return 202 with an EMPTY body (turn scheduled —
    output arrives on the SSE tail, never in the POST response).
  - POST /sessions returns 201 {session_id}.
  - 409 = well-formed request vs session state (mid-turn / awaiting
    approval / ended) — see `ApiError`; 404 = unknown session.
  - SSE frames carry journal event envelopes; the SSE `id:` is the event's
    UUIDv7. `EventSource` reconnects resume automatically via
    Last-Event-ID; a first-connect resume uses the `last_event_id` query
    param (EventSource can't set headers).
*/

import {
  ApiError,
  type ApprovalDecision,
  type DevStatus,
  type JournalEvent,
  type ListSessionsParams,
  type NewSessionRequest,
  type PendingApproval,
  type Session,
  type SessionCreated,
  type SessionStreamOptions,
  type SessionsApi,
} from "./types";

export { ApiError };

export interface SessionsApiConfig {
  /**
   * Base of the gateway. Dev: `http://localhost:<port>` (no auth). Prod:
   * the same-origin rewrite root (`/api/v1`) so the platform-session
   * cookie rides along — or the direct server URL for server components.
   */
  baseUrl: string;
  /**
   * Base for the SSE journal tail ONLY (defaults to `baseUrl`). Prod sets
   * it to the DIRECT server origin (NEXT_PUBLIC_SECTR_SERVER_URL): the
   * streaming path must not traverse the /api rewrite — the dev proxy
   * starved streamed frames nondeterministically (M1 run finding). JSON
   * keeps the same-origin base. Cross-origin, hence the server.s
   * any-origin CORS layer for the tail.
   */
  sseBaseUrl?: string;
  /** Extra headers per request (the credential-provider seam). Dev: omit. */
  credentials?: () => Record<string, string>;
  /**
   * Wraps every authenticated send (prod: refresh the session once on a
   * 401 and resend — transport/auth-refresh.ts). `send` reads
   * the current credentials on each call. Dev: omit (no auth).
   */
  authRetry?: (send: () => Promise<Response>) => Promise<Response>;
}

export function createSessionsApi(config: SessionsApiConfig): SessionsApi {
  /** The gateway path prefixed with the configured base. */
  const url = (path: string, query?: Record<string, string | undefined>) => {
    const qs = query
      ? "?" +
        new URLSearchParams(
          Object.entries(query).filter((entry): entry is [string, string] => entry[1] !== undefined),
        )
      : "";
    return `${config.baseUrl}${path}${qs}`;
  };

  /** fetch with the CURRENT credentials, through the auth-retry seam. */
  const authedFetch = (input: string, init: RequestInit = {}): Promise<Response> => {
    const send = () =>
      fetch(input, {
        ...init,
        headers: { ...(init.headers as Record<string, string>), ...config.credentials?.() },
      });
    return config.authRetry ? config.authRetry(send) : send();
  };

  /** JSON fetch; 202/204 (empty body) resolve to undefined. */
  async function request<T>(path: string, init: RequestInit): Promise<T | undefined> {
    const res = await authedFetch(url(path), {
      ...init,
      headers: { "Content-Type": "application/json" },
    });
    if (!res.ok) {
      throw new ApiError(res.status, await res.text().catch(() => ""));
    }
    const text = await res.text();
    return text.trim() === "" ? undefined : (JSON.parse(text) as T);
  }

  return {
    /** POST /sessions — create a session; its first turn starts immediately. */
    async createSession(req: NewSessionRequest): Promise<SessionCreated> {
      const body = await request<SessionCreated>("/sessions", {
        method: "POST",
        body: JSON.stringify(req),
      });
      if (!body) throw new ApiError(201, "expected {session_id}, got empty body");
      return body;
    },

    /** GET /sessions — sessions tables + approvals inbox source. */
    async listSessions(params: ListSessionsParams = {}): Promise<Session[]> {
      const res = await authedFetch(
        url("/sessions", {
          app_id: params.app_id,
          status: params.status,
          limit: params.limit?.toString(),
          created_before: params.created_before,
        }),
      );
      if (!res.ok) throw new ApiError(res.status, await res.text().catch(() => ""));
      return (await res.json()) as Session[];
    },

    /** GET /sessions/{sessionId} — 404 → undefined. */
    async getSession(sessionId: string): Promise<Session | undefined> {
      try {
        return await request<Session>(`/sessions/${sessionId}`, { method: "GET" });
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) return undefined;
        throw e;
      }
    },

    /** POST .../input — a follow-up turn; 202 = scheduled. */
    async sendSessionInput(sessionId: string, input: string): Promise<void> {
      await request(`/sessions/${sessionId}/input`, {
        method: "POST",
        body: JSON.stringify({ input }),
      });
    },

    /** POST .../approval — decide; 202 = journaled, resume scheduled. */
    async decideApproval(sessionId: string, decision: ApprovalDecision): Promise<void> {
      await request(`/sessions/${sessionId}/approval`, {
        method: "POST",
        body: JSON.stringify(decision),
      });
    },

    /** GET /approvals — pending approvals across all apps (admin inbox). */
    async listApprovals(): Promise<PendingApproval[]> {
      return (await request<PendingApproval[]>("/approvals", { method: "GET" })) ?? [];
    },

    /** GET /dev/status — DEV-ONLY; null on any failure (see types.ts). */
    async devStatus(): Promise<DevStatus | null> {
      try {
        const status = await request<DevStatus>("/dev/status", { method: "GET" });
        return status ?? null;
      } catch {
        return null;
      }
    },

    /**
     * GET /healthz — the PROD badge's liveness probe (the rewrite makes it
     * same-origin). True only on a 200; anything else (404 on a dev
     * gateway? no — dev gateways have /healthz too) resolves honestly.
     */
    async prodHealth(): Promise<boolean> {
      try {
        const res = await fetch(url("/healthz"), { headers: config.credentials?.() });
        return res.ok;
      } catch {
        return false;
      }
    },

    /**
     * GET /sessions/{sessionId}/events (text/event-stream) — the journal
     * tail: full replay from seq 0, then live. Never resolves; call the
     * returned close() to stop.
     *
     * Fetch-streamed rather than EventSource: the prod server requires an
     * Authorization header (EventSource can't set one), and a manual
     * reconnect loop via `last_event_id` replaces EventSource's native
     * Last-Event-ID resume (same semantics — the tail replays from the
     * given id). Applies to the dev gateway too (one code path).
     */
    openSessionStream(sessionId: string, opts: SessionStreamOptions): () => void {
      let closed = false;
      let lastId = opts.lastEventId;
      let current: AbortController | null = null;

      // SSE tracing (console.debug, target `[sse]`): connection lifecycle,
      // reconnects, and inter-frame gaps > 1s. The other half of the
      // server's `sectr_sse` DEBUG trace — together they answer "did the
      // server emit late, or did the transport deliver late?" (M1 run's
      // nondeterministic journal-prefix bug).
      const sseUrl = (path: string, query?: Record<string, string | undefined>) => {
        const qs = query
          ? "?" +
            new URLSearchParams(
              Object.entries(query).filter((entry): entry is [string, string] => entry[1] !== undefined),
            )
          : "";
        return `${config.sseBaseUrl ?? config.baseUrl}${path}${qs}`;
      };
      const trace = (msg: string, ...rest: unknown[]) =>
        console.debug(`[sse] ${msg}`, ...rest);
      // One-time per stream: which base is actually in use (direct server
      // vs same-origin rewrite) — the M1 stall looked identical on both
      // until this was logged.
      trace(`base ${config.sseBaseUrl ?? config.baseUrl}${config.sseBaseUrl ? " (direct)" : ""}`);

      // SAFARI stall workaround (M1 run): after a page RELOAD, Safari
      // sometimes stops delivering an in-flight fetch body (~50% of
      // reloads, at an arbitrary prefix — 200/384/432 frames observed)
      // and only resumes ~15s later; Firefox never stalls, navigation
      // never stalls, and the server emitted every frame in <10ms (both
      // ends traced). The tail's resume semantics make the cure simple:
      // if NO bytes (not even keepalives — the server heartbeats every
      // 2s) arrive for STALL_MS, drop the connection and re-open with
      // last_event_id — the fresh stream replays the remainder instantly.
      // Exact-once is preserved by the caller's event-id dedup.
      const STALL_MS = 4_000;

      const consume = async () => {
        while (!closed) {
          const controller = new AbortController();
          current = controller;
          try {
            const t0 = performance.now();
            trace(`connect attempt ${lastId ? `resume after ${lastId}` : "full replay"}`);
            const res = await authedFetch(
              sseUrl(`/sessions/${sessionId}/events`, { last_event_id: lastId }),
              {
                headers: { Accept: "text/event-stream" },
                signal: controller.signal,
              },
            );
            if (res.status === 401) {
              // The key is dead and the auth-retry seam could not replace
              // it (the app is routing to /login). Stop: reconnecting every
              // 500 ms would just 401 forever.
              trace("401 after auth retry — stopping the stream");
              opts.onError?.(new ApiError(401, await res.text().catch(() => "")));
              return;
            }
            if (!res.ok || !res.body) {
              throw new ApiError(res.status, await res.text().catch(() => ""));
            }
            trace(`connected ${res.status} in ${Math.round(performance.now() - t0)}ms`);
            let frames = 0;
            const reader = res.body.getReader();
            const decoder = new TextDecoder();
            let buf = "";
            let eof = false;
            while (!eof) {
              // Byte-level stall race: no chunk (not even a keepalive)
              // within STALL_MS → abort this connection and let the loop
              // reconnect with resume. Losing side of the race is
              // swallowed — its abort rejection is expected.
              const read = reader.read();
              read.catch(() => {});
              let stallTimer: ReturnType<typeof setTimeout> | undefined;
              const chunk = await Promise.race([
                read,
                new Promise<never>((_, reject) => {
                  stallTimer = setTimeout(() => reject(new Error("sse stall")), STALL_MS);
                }),
              ]);
              if (stallTimer !== undefined) clearTimeout(stallTimer);
              const { done, value } = chunk;
              if (done) {
                eof = true;
                break;
              }
              buf += decoder.decode(value, { stream: true }).replaceAll("\r\n", "\n");
              let frameEnd;
              while ((frameEnd = buf.indexOf("\n\n")) !== -1) {
                const frame = buf.slice(0, frameEnd);
                buf = buf.slice(frameEnd + 2);
                let id: string | undefined;
                let data = "";
                for (const line of frame.split("\n")) {
                  if (line.startsWith("id:")) id = line.slice(3).trim();
                  else if (line.startsWith("data:")) data += line.slice(5).trim();
                }
                if (data) {
                  if (id) lastId = id;
                  frames += 1;
                  opts.onEvent(JSON.parse(data) as JournalEvent);
                }
              }
            }
            trace(`stream ended after ${frames} frames (last id ${lastId})`);
            // Server closed the stream (or the connection dropped) —
            // fall through to the reconnect below, resuming from lastId.
          } catch (e) {
            if (closed) return;
            if (e instanceof Error && e.message === "sse stall") {
              trace(
                `stall: no bytes for ${STALL_MS}ms — reconnecting to resume after ${lastId}`,
              );
            } else {
              trace("stream error", e);
              opts.onError?.(e);
            }
          }
          if (closed) return;
          await new Promise((r) => setTimeout(r, 500));
        }
      };
      void consume();

      return () => {
        closed = true;
        current?.abort();
      };
    },
  };
}
