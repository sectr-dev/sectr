/*
  A tiny bridge from Clerk's React hooks to non-React callers.

  `useAuth()` / `useClerk()` only work inside components, but the AuthGate's
  401 handler and the account menu's sign-out are plain async code that must
  work whether or not Clerk is on (hooks cannot be called conditionally).
  <ClerkBridge /> (mounted inside <ClerkProvider>, prod server only)
  publishes the current Clerk session here; everyone else reads it through
  these functions and sees `null` when Clerk is off.
*/

import * as api from "./api";

export interface ClerkSessionBridge {
    isLoaded: boolean;
    isSignedIn: boolean;
    getToken(): Promise<string | null>;
    /** Ends the Clerk session and navigates to `redirectUrl`. */
    signOut(redirectUrl: string): Promise<void>;
}

let bridge: ClerkSessionBridge | null = null;
const listeners = new Set<() => void>();

export function setClerkBridge(next: ClerkSessionBridge | null) {
    bridge = next;
    listeners.forEach((l) => l());
}

export function getClerkBridge(): ClerkSessionBridge | null {
    return bridge;
}

/** Resolves with the bridge once Clerk has loaded; null on timeout / Clerk off. */
function whenClerkLoaded(timeoutMs = 5000): Promise<ClerkSessionBridge | null> {
    if (bridge?.isLoaded) return Promise.resolve(bridge);
    return new Promise((resolve) => {
        const done = (value: ClerkSessionBridge | null) => {
            clearTimeout(timer);
            listeners.delete(onChange);
            resolve(value);
        };
        const onChange = () => {
            if (bridge?.isLoaded) done(bridge);
        };
        const timer = setTimeout(() => done(null), timeoutMs);
        listeners.add(onChange);
    });
}

/**
 * Silent re-exchange after a 401 on the stored key (the dashboard-session
 * key expired or was revoked): if a Clerk session is still active, swap it
 * for a fresh key. False = no Clerk session (or the exchange failed) — the
 * caller falls through to /login.
 */
export async function reexchangeFromClerk(): Promise<boolean> {
    const clerk = await whenClerkLoaded();
    if (!clerk?.isSignedIn) return false;
    try {
        const token = await clerk.getToken();
        if (!token) return false;
        const { key } = await api.exchangeClerkSession(token);
        await api.login(key);
        return true;
    } catch {
        return false;
    }
}

// The transports' 401 → refresh-once hook (transport/
// auth-refresh.ts). Registered at module load — this module is imported by
// the AuthGate, so it is in place before any console request. Mock and
// dev-gateway modes have no keys to refresh, and without Clerk a dead key is
// simply a sign-out.
if (api.CLERK_ENABLED) api.setSessionRefresher(reexchangeFromClerk);
