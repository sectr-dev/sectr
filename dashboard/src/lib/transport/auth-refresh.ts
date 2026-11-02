/*
  Recover from a dead platform key MID-SESSION.

  A Clerk sign-in stores an EXPIRING dashboard-session key (default 7
  days). It can die while a tab is open: it expires, the user revokes it
  in Settings, or signs out in another tab. Every prod transport call
  goes through `fetchWithAuthRetry`: on a 401 it refreshes the session
  ONCE (single-flight — N concurrent 401s cause one exchange) and resends
  with the new key. Resending is safe for every method: the server's auth
  middleware rejects before any handler runs, so the 401'd request did
  nothing.

  If the refresh fails (no live Clerk session), the auth-lost listeners fire; the AuthGate purges the
  key and routes to /login.

  This module has NO imports: the transports depend on it, and the
  refresher (Clerk) is registered from outside (lib/clerk-session.ts), so
  there is no transport → Clerk → api → transport cycle.
*/

type Refresher = () => Promise<boolean>;

let refresher: Refresher | null = null;
let inFlight: Promise<boolean> | null = null;
/** The key a sign-out just revoked: its 401s must NOT trigger a refresh
 *  (the Clerk session is still live until the sign-out finishes, and a
 *  refresh would mint a fresh key behind the user's back). */
let signedOutKey: string | null = null;
const lostListeners = new Set<() => void>();

/** Registered by lib/clerk-session.ts when Clerk is on; null = no way to
 *  refresh (every dead key is a sign-out). */
export function setSessionRefresher(fn: Refresher | null): void {
  refresher = fn;
}

/** Called at the START of a sign-out, with the key being signed out. */
export function markSignedOut(key: string | null): void {
  signedOutKey = key;
}

/** Fires when a dead key could not be replaced. Returns an unsubscribe. */
export function onAuthLost(listener: () => void): () => void {
  lostListeners.add(listener);
  return () => {
    lostListeners.delete(listener);
  };
}

function refreshSession(): Promise<boolean> {
  inFlight ??= (async () => {
    const ok = refresher ? await refresher().catch(() => false) : false;
    if (!ok) lostListeners.forEach((listener) => listener());
    return ok;
  })().finally(() => {
    inFlight = null;
  });
  return inFlight;
}

/**
 * Send an authenticated request; on a 401, refresh the session once and
 * resend. `send` must read the CURRENT credentials on every call (the
 * resend picks up the new key). `currentKey` reads the stored key.
 */
export async function fetchWithAuthRetry(
  currentKey: () => string | null,
  send: () => Promise<Response>,
): Promise<Response> {
  const keyUsed = currentKey();
  const res = await send();
  if (res.status !== 401 || keyUsed === null || keyUsed === signedOutKey) {
    return res;
  }
  // If another caller already swapped a new key in, just resend with it.
  if (currentKey() === keyUsed && !(await refreshSession())) return res;
  void res.body?.cancel().catch(() => {});
  return send();
}
