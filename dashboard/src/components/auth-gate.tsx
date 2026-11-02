"use client";

/**
 * PROD-mode auth gate (the console's only choke point — every console
 * route renders inside this). In prod mode the console is UNVIEWABLE
 * without a validated platform key:
 *
 *   no stored key      → redirect /login, nothing renders
 *   key fails GET /me  → the transport already tried a silent
 *                        Clerk re-exchange + retry (transport/
 *                        auth-refresh.ts); still 401 → key cleared,
 *                        redirect /login, nothing renders
 *   other server error → "server unreachable" screen (a transient outage
 *                        must not purge a valid key), retry button
 *   key valid          → children render
 *
 * Dev-gateway and mock modes pass through untouched — the fake/demo
 * experience is for local console development only (the dev gateway has
 * no auth to gate on; the mock has nothing real to gate).
 *
 * The gate also validates BEFORE first paint of the shell, so an invalid
 * key can never flash real chrome (let alone data — every data call would
 * 401 anyway).
 *
 * AFTER mount, a key that dies mid-session (expiry, revoked in
 * Settings, signed out in another tab) is refreshed by the transports on
 * the first 401; when that fails they fire `onAuthLost`, and the gate
 * purges the key and routes to /login.
 */

import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import { useRouter } from "next/navigation";
import { LoaderCircle, ServerOff } from "lucide-react";

import * as api from "@/lib/api";
import { identifyOrg } from "@/lib/analytics";
// Side effect: registers the Clerk refresher with the transports.
import "@/lib/clerk-session";
import { Button } from "@/components/ui/button";
import { loginUrlFor } from "@/lib/next-url";

type GateState =
  | { phase: "checking" }
  | { phase: "ok" }
  | { phase: "redirecting" }
  | { phase: "unreachable"; message: string };

// Never notifies: these are read once per render, and a render follows
// every change that matters (sign-out navigates, the gate's own state).
const subscribeNone = () => () => {};

export function AuthGate({ children }: { children: React.ReactNode }) {
  const router = useRouter();
  const [state, setState] = useState<GateState>({ phase: "checking" });
  // Browser-only facts, read without a hydration mismatch: the server
  // snapshots ("gated, key present") match what the server rendered — the
  // checking spinner — and the browser's real values take over right after
  // hydration. Mock/dev modes are not gated: that experience is for local
  // console dev.
  const gated = useSyncExternalStore(subscribeNone, () => api.PROD_MODE, () => true);
  const hasKey = useSyncExternalStore(
    subscribeNone,
    () => api.getPlatformKey() !== null,
    () => true,
  );

  const redirectToLogin = useCallback(() => {
    // Keep where they were headed (e.g. a CLI approval link).
    // Read at call time (not via usePathname) so navigating inside the
    // console doesn't re-run the sign-in check.
    router.replace(
      loginUrlFor(window.location.pathname + window.location.search),
    );
  }, [router]);

  const toLogin = useCallback(() => {
    setState({ phase: "redirecting" });
    redirectToLogin();
  }, [redirectToLogin]);

  // State changes only in the promise callbacks — never synchronously in
  // the effect that runs this.
  const check = useCallback(() => {
    // A dead key is refreshed + retried inside the transport.
    api.me().then(
      (me) => {
        // Identify by org (the /me response's org id) — the same
        // distinct_id the server-side PostHog events use. No-op without a
        // PostHog key / in dev-gateway mode.
        identifyOrg(me.org.id, me.org.name);
        setState({ phase: "ok" });
      },
      (err) => {
        if (!(err instanceof api.ApiError && err.status === 401)) {
          setState({
            phase: "unreachable",
            message: err instanceof Error ? err.message : "unknown error",
          });
          return;
        }
        // Dead/revoked key that could not be refreshed: purge it, start
        // clean at /login.
        void api.logout();
        toLogin();
      },
    );
  }, [toLogin]);

  useEffect(() => {
    if (!gated) return;
    if (!hasKey) {
      redirectToLogin();
      return;
    }
    check();
  }, [gated, hasKey, check, redirectToLogin]);

  // Mid-session: a key that died and could not be refreshed.
  useEffect(() => {
    if (!api.PROD_MODE) return;
    return api.onAuthLost(() => {
      void api.logout();
      toLogin();
    });
  }, [toLogin]);

  if (!gated || (hasKey && state.phase === "ok")) return <>{children}</>;
  const phase = hasKey ? state.phase : "redirecting";

  return (
    <div className="flex min-h-svh items-center justify-center">
      {phase === "checking" || phase === "redirecting" ? (
        <div
          className="flex flex-col items-center gap-3 text-muted-foreground"
          role="status"
          aria-live="polite"
        >
          <LoaderCircle className="size-5 animate-spin" />
          <span className="text-sm">
            {phase === "checking" ? "Checking sign-in…" : "Redirecting to sign-in…"}
          </span>
        </div>
      ) : (
        <div className="flex max-w-sm flex-col items-center gap-3 text-center">
          <ServerOff className="size-6 text-muted-foreground" />
          <div className="text-sm font-medium">Server unreachable</div>
          <p className="text-xs text-muted-foreground">
            Your platform key is kept, but the server could not be reached to
            validate it. Nothing was signed out.
          </p>
          <p className="max-w-full truncate font-mono text-[11px] text-faint-foreground">
            {state.phase === "unreachable" && state.message}
          </p>
          <Button variant="outline" size="sm" onClick={() => {
            setState({ phase: "checking" });
            check();
          }}>
            Retry
          </Button>
        </div>
      )}
    </div>
  );
}
