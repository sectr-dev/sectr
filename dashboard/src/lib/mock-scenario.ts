/*
  Which demo dataset mock mode serves: the full fixtures, or an EMPTY
  account (no apps, sessions, API keys or secrets) for testing the UI's
  empty states. The "demo data" badge toggles it: a localStorage flag + full
  reload, like the gateway swap (lib/gateway-swap.ts) — the mock transports
  read it once at import. Server-side it is always the full dataset: only
  the mock session page server-renders fixtures, so a fixture session URL
  opened directly still renders (nothing links there when empty).
*/

const KEY = "sectr.mockData";

export const MOCK_EMPTY =
    typeof window !== "undefined" && window.localStorage.getItem(KEY) === "empty";

/** Swap full ⇄ empty demo data, then reload. Browser-only. */
export function toggleMockEmpty(): void {
    if (MOCK_EMPTY) window.localStorage.removeItem(KEY);
    else window.localStorage.setItem(KEY, "empty");
    window.location.reload();
}
