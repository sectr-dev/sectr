/**
 * Return-to after sign-in: `AuthGate` sends signed-out visitors to
 * `/login?next=<path>`; the login card remembers `next` across the OAuth
 * round trip (the SSO callback force-redirects to `/login` and drops query
 * params) and lands there instead of `/`.
 */

const STORAGE_KEY = "sectr.next";
/** Set by the SSO callback: the next `/login` load is an OAuth return. */
const OAUTH_RETURN_KEY = "sectr.oauthReturn";

/** Same-origin paths only: one leading `/`, never `//host` or `/\host`
 *  (protocol-relative / backslash tricks), no scheme. Anything else → null,
 *  so `next` can never be an open redirect. */
export function safeNext(raw: string | null | undefined): string | null {
  if (!raw || !raw.startsWith("/")) return null;
  if (raw.startsWith("//") || raw.startsWith("/\\")) return null;
  if (/[\u0000-\u001f]/.test(raw)) return null;
  return raw;
}

/** `/login` with the current page as `next` (omitted for the home page). */
export function loginUrlFor(path: string): string {
  return safeNext(path) && path !== "/"
    ? `/login?next=${encodeURIComponent(path)}`
    : "/login";
}

/** Called by the SSO callback so `/login` knows the missing `?next=` is
 *  just the OAuth round trip dropping it. */
export function markOauthReturn(): void {
  try {
    window.sessionStorage.setItem(OAUTH_RETURN_KEY, "1");
  } catch {
    // Storage blocked: return-to is best effort.
  }
}

/** Capture `?next=` from the current URL into sessionStorage. A `/login`
 *  load with no `next` that isn't an OAuth return drops any earlier capture
 *  (an abandoned sign-in must not redirect a later, unrelated one). */
export function rememberNext(): void {
  try {
    const next = safeNext(new URLSearchParams(window.location.search).get("next"));
    if (next) {
      window.sessionStorage.setItem(STORAGE_KEY, next);
    } else if (!window.sessionStorage.getItem(OAUTH_RETURN_KEY)) {
      window.sessionStorage.removeItem(STORAGE_KEY);
    }
  } catch {
    // Storage blocked: sign-in still works, it just lands on the home page.
  }
}

/** The remembered destination (consumed), or `/`. */
export function takeNext(): string {
  try {
    const next = safeNext(window.sessionStorage.getItem(STORAGE_KEY));
    window.sessionStorage.removeItem(STORAGE_KEY);
    window.sessionStorage.removeItem(OAUTH_RETURN_KEY);
    if (next) return next;
  } catch {
    // fall through
  }
  return "/";
}
