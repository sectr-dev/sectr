/*
  Backend swap helpers (the BackendBadge's actions). Swapping is a URL-level
  action PERFORMED by the badge, never a runtime transport mutation: api.ts
  resolves API_URL once at import and builds the whole transport from it, so
  every swap here ends in a full reload.

  Storage: `sectr.gateway` is the pin api.ts reads (resolution untouched).
  `sectr.gateway.lastDev` (+ `.project`, tooltip copy only) is where swap-
  to-prod PARKS the gateway URL so swap-back works; resolveApiUrl never
  reads it. Browser-only — call from effects / event handlers.
*/

import { API_URL } from "@/lib/api";

const PIN_KEY = "sectr.gateway";
const LAST_DEV_KEY = "sectr.gateway.lastDev";
const LAST_DEV_PROJECT_KEY = "sectr.gateway.lastDev.project";

/**
 * A prod build exists to swap TO only when the build env is set (the
 * deploy script inlines `/api`). Without it, dropping the pin lands in
 * mock mode, which is not "prod".
 */
export const PROD_AVAILABLE = !!process.env.NEXT_PUBLIC_SECTR_API;

export interface RememberedGateway {
    url: string;
    project?: string;
}

export function rememberGateway(url: string, project?: string): void {
    window.localStorage.setItem(LAST_DEV_KEY, url);
    if (project) window.localStorage.setItem(LAST_DEV_PROJECT_KEY, project);
    else window.localStorage.removeItem(LAST_DEV_PROJECT_KEY);
}

/** The remembered gateway, left in place (probing reads it, doesn't consume it). */
export function peekRememberedGateway(): RememberedGateway | null {
    const url = window.localStorage.getItem(LAST_DEV_KEY);
    if (!url) return null;
    return {
        url,
        project: window.localStorage.getItem(LAST_DEV_PROJECT_KEY) ?? undefined,
    };
}

/** Read the remembered gateway AND clear it (it becomes the live pin). */
export function takeRememberedGateway(): RememberedGateway | null {
    const remembered = peekRememberedGateway();
    window.localStorage.removeItem(LAST_DEV_KEY);
    window.localStorage.removeItem(LAST_DEV_PROJECT_KEY);
    return remembered;
}

/**
 * Rewrite the address bar to the same path with `?gateway=` stripped, then
 * reload. replaceState + reload (not assign) so an identical URL still
 * reloads instead of degrading to a same-document fragment navigation.
 */
function reloadWithoutGatewayParam(): void {
    const url = new URL(window.location.href);
    url.searchParams.delete("gateway");
    window.history.replaceState(null, "", url);
    window.location.reload();
}

/** Dev → prod: park the current gateway, drop the pin, reload onto the build env. */
export function swapToProd(project?: string): void {
    rememberGateway(API_URL, project);
    window.localStorage.removeItem(PIN_KEY);
    reloadWithoutGatewayParam();
}

/** Prod → dev: restore the remembered gateway as the pin, reload. No-op if none. */
export function swapToDev(): void {
    const remembered = takeRememberedGateway();
    if (!remembered) return;
    window.localStorage.setItem(PIN_KEY, remembered.url);
    reloadWithoutGatewayParam();
}
