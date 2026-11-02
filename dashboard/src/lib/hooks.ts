/*
  Client-side data hooks over api.ts — the UI's read layer. No external
  state library (deliberate): a tiny useAsync for REST reads, and
  useSessionEvents as the one subscription point for the journal tail.

  CLIENT-ONLY (react hooks) — server components shape with the pure
  helpers in `view.ts` instead.
*/

import { useEffect, useMemo, useState, useSyncExternalStore } from "react";

import * as api from "@/lib/api";
import { peekRememberedGateway } from "@/lib/gateway-swap";
import { createSessionsApi } from "@/lib/transport/sessions-api";
import type { App, Deployment, Session } from "@/lib/types";
import type { DevStatus, JournalEvent } from "@/lib/transport/types";

import { toUiApproval, toUiSession } from "@/lib/view";
export { toUiApproval, toUiDeployment, toUiSession } from "@/lib/view";
import type { PendingApproval } from "@/lib/types";

// ---------------------------------------------------------------------------
// REST reads
// ---------------------------------------------------------------------------

interface AsyncState<T> {
  data: T | undefined;
  loading: boolean;
  error: unknown;
}

/** Minimal async loader — no external state library; callers render
 *  loading/empty states. Reloads when deps change. */
export function useAsync<T>(load: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ data: undefined, loading: true, error: undefined });
  useEffect(() => {
    let on = true;
    // Initial state is already loading:true; on dep change we swap data for
    // loading asynchronously (in the promise callbacks) — never synchronously
    // in the effect, which would cascade renders.
    load().then(
      (data) => on && setState({ data, loading: false, error: undefined }),
      (error) => on && setState({ data: undefined, loading: false, error }),
    );
    return () => {
      on = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}

// Every useApps() fetches its own copy of the project list (the sidebar's
// lives as long as the shell), so a change one component sees has to reach
// them all: refreshApps() makes every mounted useApps() refetch.
let appsVersion = 0;
const appsListeners = new Set<() => void>();

/** Refetch the project list everywhere (sidebar, palette, pages) — e.g.
 *  once the welcome sees the first project. */
export function refreshApps(): void {
  appsVersion += 1;
  appsListeners.forEach((listener) => listener());
}

function subscribeApps(listener: () => void): () => void {
  appsListeners.add(listener);
  return () => appsListeners.delete(listener);
}

export function useApps(): { apps: App[]; loading: boolean } {
  const version = useSyncExternalStore(subscribeApps, () => appsVersion, () => appsVersion);
  const { data, loading } = useAsync(api.listApps, [version]);
  return { apps: data ?? [], loading };
}

/** One app by id. In gateway mode this resolves client-side from the dev
 *  gateway's /dev/status — the edge can't reach localhost, so app pages
 *  must be client components (see api.ts). */
export function useApp(id: string): { app: App | undefined; loading: boolean } {
  const { data, loading } = useAsync(() => api.getApp(id), [id]);
  return { app: data, loading };
}

export function useDeployments(appId: string): Deployment[] {
  const { data } = useAsync(() => api.listDeployments(appId), [appId]);
  return data ?? [];
}

/** Sessions (UI-shaped, app_name joined). All sessions, or one app's. */
export function useSessions(appId?: string): Session[] {
  return useSessionList(appId).sessions;
}

/** Like useSessions, plus `loading` — for empty states that must not flash
 *  before the first response. */
export function useSessionList(appId?: string): { sessions: Session[]; loading: boolean } {
  const { apps } = useApps();
  const { data, loading } = useAsync(() => api.listSessions(appId ? { app_id: appId } : {}), [appId]);
  const sessions = useMemo(
    () => (data ?? []).map((s) => toUiSession(s, apps)),
    [data, apps],
  );
  return { sessions, loading };
}

export function useSession(sessionId: string): Session | undefined {
  const { apps } = useApps();
  const { data } = useAsync(() => api.getSession(sessionId), [sessionId]);
  return useMemo(() => (data ? toUiSession(data, apps) : undefined), [data, apps]);
}

/** Approvals poll — new pauses show up without a reload. */
const APPROVALS_POLL_MS = 10_000;

/** Pending approvals across all apps (admin inbox), UI-shaped. Polls. */
export function useApprovals(): PendingApproval[] {
  const { apps } = useApps();
  const [data, setData] = useState<Awaited<ReturnType<typeof api.listApprovals>>>();
  useEffect(() => {
    let on = true;
    const load = () =>
      api.listApprovals().then(
        (d) => on && setData(d),
        () => {}, // keep the last good list; the next tick retries
      );
    load();
    const id = setInterval(() => {
      if (!document.hidden) load();
    }, APPROVALS_POLL_MS);
    const onVisible = () => !document.hidden && load();
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      on = false;
      clearInterval(id);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, []);
  return useMemo(() => (data ?? []).map((a) => toUiApproval(a, apps)), [data, apps]);
}

// ---------------------------------------------------------------------------
// The journal tail (one subscription point)
// ---------------------------------------------------------------------------

/**
 * A session's journal: full replay from seq 0, then live — via
 * api.openSessionStream (EventSource against the real gateway; timers
 * behind the mock). Exactly-once by event id; seq-ordered.
 *
 * Frames are BATCHED into state (≤ every 50ms) rather than applied per
 * frame: a full replay is hundreds of frames, and one setState per frame
 * made the inspector re-render hundreds of times — the journal "popped
 * in" over ~30s after a reload while the stream itself had finished in
 * milliseconds (M1 run finding; the page stays responsive throughout,
 * which is why it didn't look like a render stall).
 */
export function useSessionEvents(sessionId: string): JournalEvent[] {
  const [events, setEvents] = useState<JournalEvent[]>([]);
  useEffect(() => {
    // TEMP diagnostics (M1 Safari-stall hunt) — t0 was used by the commented
    // [events] logs below.
    // const t0 = performance.now();
    const seen = new Set<string>();
    let buffer: JournalEvent[] = [];
    let flushTimer: ReturnType<typeof setTimeout> | undefined;
    const flush = () => {
      flushTimer = undefined;
      if (buffer.length === 0) return;
      const batch = buffer;
      buffer = [];
      // TEMP diagnostics (M1 Safari-stall hunt) — commented out, keep for
      // the next transport mystery:
      // console.debug(
      //   `[events] flush ${batch.length} (cum ${seen.size}) @ ${Math.round(performance.now() - t0)}ms`,
      // );
      setEvents((prev) => [...prev, ...batch]); // seq is monotonic per session
    };
    const close = api.openSessionStream(sessionId, {
      onEvent: (e) => {
        if (seen.has(e.id)) return; // exactly-once (replays + live overlap)
        seen.add(e.id);
        buffer.push(e);
        flushTimer ??= setTimeout(flush, 50);
        // TEMP diagnostics (M1 run): where do replay frames stall?
        // TEMP diagnostics (M1 Safari-stall hunt) — commented out, keep for
        // the next transport mystery:
        // if (seen.size % 50 === 0)
        //   console.debug(
        //     `[events] received ${seen.size} @ ${Math.round(performance.now() - t0)}ms`,
        //   );
      },
    });
    // A closed subscription must not flush its pending batch: the next one
    // (e.g. dev StrictMode's remount) replays with a fresh `seen`, so a
    // late flush would land every replayed event twice.
    return () => {
      clearTimeout(flushTimer);
      close();
    };
  }, [sessionId]);
  return events;
}

// ---------------------------------------------------------------------------
// Backend identity (the topbar badge)
// ---------------------------------------------------------------------------

/**
 * Whether the prod pills can swap back to a remembered dev gateway
 *. `none` covers both "never attached" and "probe in flight" —
 * either way no affordance. `stale` = remembered but dead (never revisited
 * this tab: ports are OS-assigned per `sectr dev` run).
 */
export type RememberedState =
  | { state: "none" }
  | { state: "live"; project?: string }
  | { state: "stale"; project?: string };

type ProdExtras = { remembered: RememberedState };

export type Backend =
  | { kind: "mock" }
  | { kind: "local-gateway"; status: DevStatus }
  | { kind: "local-gateway-offline"; baseUrl: string }
  | ({ kind: "prod" } & ProdExtras)
  | ({ kind: "prod-offline" } & ProdExtras);

type ProbedBackend =
  | Exclude<Backend, { kind: "prod" | "prod-offline" }>
  | { kind: "prod" }
  | { kind: "prod-offline" };

// Remembered-gateway burst: ~3 tries over ~10s, then give up for the tab's
// life. NEVER a recurring timer — a recycled port could answer as a
// different project's gateway.
const REMEMBERED_TRIES = 3;
const REMEMBERED_RETRY_MS = 5_000;
const REMEMBERED_TIMEOUT_MS = 3_000;

async function probeRemembered(url: string): Promise<DevStatus | null> {
  const probe = createSessionsApi({ baseUrl: url }).devStatus();
  const timeout = new Promise<null>((resolve) =>
    setTimeout(() => resolve(null), REMEMBERED_TIMEOUT_MS),
  );
  return Promise.race([probe, timeout]);
}

const isLocal = api.isLocalGateway;

/**
 * What backend is this console attached to? Polls /dev/status (dev-only
 * route — a prod server has none) every 5s when live, so killing `sectr
 * dev` flips the badge to offline without a refresh. Mock mode needs no
 * probe: the badge reads fixtures directly. In prod mode with a remembered
 * dev gateway, also runs the one-shot bounded burst probe (see above).
 */
export function useBackend(): Backend | null {
  // null until the first client-side probe resolves — the initial render
  // MUST be identical on server and browser (API_URL differs: env vs
  // localStorage), or React tears the tree down with a hydration error.
  // BackendBadge renders a skeleton pill for the null state.
  const [backend, setBackend] = useState<ProbedBackend | null>(null);
  const [remembered, setRemembered] = useState<RememberedState>({ state: "none" });
  useEffect(() => {
    if (api.MOCK_MODE) {
      // Deferred one tick — no setState inside the effect body.
      const t = setTimeout(() => setBackend({ kind: "mock" }), 0);
      return () => clearTimeout(t);
    }
    let on = true;
    const probe = () => {
      // PROD mode: skip the dev probe entirely — /dev/status doesn't exist
      // on the real server, so every 5s poll 404'd into the console (M1
      // run finding); the badge is the prod-health probe alone.
      if (api.PROD_MODE) {
        void api.prodHealth().then((up) => {
          if (!on) return;
          setBackend(up ? { kind: "prod" } : { kind: "prod-offline" });
        });
        return;
      }
      api
        .devStatus()
        .then((status) => {
          if (!on) return;
          if (status) {
            setBackend({ kind: "local-gateway", status });
          } else if (isLocal(api.API_URL)) {
            setBackend({ kind: "local-gateway-offline", baseUrl: api.API_URL });
          } else {
            return api.prodHealth().then((up) => {
              if (!on) return;
              setBackend(up ? { kind: "prod" } : { kind: "prod-offline" });
            });
          }
        })
        .catch(() => {
          if (!on) return;
          if (isLocal(api.API_URL)) {
            setBackend({ kind: "local-gateway-offline", baseUrl: api.API_URL });
          } else {
            setBackend({ kind: "prod-offline" });
          }
        });
    };
    probe();
    const t = setInterval(probe, 5_000);
    return () => {
      on = false;
      clearInterval(t);
    };
  }, []);

  useEffect(() => {
    if (!api.PROD_MODE) return;
    const target = peekRememberedGateway();
    if (!target) return;
    let on = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const attempt = (n: number) => {
      void probeRemembered(target.url).then((status) => {
        if (!on) return;
        if (status) {
          setRemembered({ state: "live", project: status.project });
        } else if (n + 1 < REMEMBERED_TRIES) {
          timer = setTimeout(() => attempt(n + 1), REMEMBERED_RETRY_MS);
        } else {
          setRemembered({ state: "stale", project: target.project });
        }
      });
    };
    attempt(0);
    return () => {
      on = false;
      clearTimeout(timer);
    };
  }, []);

  return useMemo<Backend | null>(
    () =>
      backend && (backend.kind === "prod" || backend.kind === "prod-offline")
        ? { ...backend, remembered }
        : backend,
    [backend, remembered],
  );
}
