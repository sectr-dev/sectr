/*
  The dashboard's PostHog analytics —
  in-app product analytics + frontend error capture (PostHog error
  tracking replaces Sentry; the Rust platform stays in the OTel/Grafana
  plane — the boundary is Rust = Grafana, JS = PostHog).

  Identity: identify by ORG, using the org id from the login key's /me
  response — the SAME distinct_id the server-side track() calls use
  (crates/sectr-server/src/analytics.rs), so server and client events
  correlate in PostHog. Never a user email (the PII rule; the MVP
  has no user records anyway — the org IS the identity).

  Gating: NEXT_PUBLIC_POSTHOG_KEY unset (local dev, demo) → posthog-js is
  never initialized and every helper below is a no-op. Local dev without
  a key therefore reports nothing; setting the key opts that browser in.
*/

import posthog, { type PostHogConfig } from "posthog-js";

export const POSTHOG_KEY = process.env.NEXT_PUBLIC_POSTHOG_KEY ?? "";
export const POSTHOG_HOST =
  process.env.NEXT_PUBLIC_POSTHOG_HOST ?? "https://us.i.posthog.com";

/** The provider initializes the SDK only when a key is configured. */
export const POSTHOG_ENABLED = POSTHOG_KEY.length > 0;

/** posthog-js init options (kept to what we actually set; the rest is the
 *  SDK's defaults — autocapture on, which is the in-app analytics leg). */
export const POSTHOG_OPTIONS = {
  api_host: POSTHOG_HOST,
  // Frontend exceptions (uncaught errors + unhandled rejections) route to
  // PostHog error tracking — the deliberate no-Sentry decision.
  capture_exceptions: true,
  // No cookie / localStorage identifiers, so the privacy policy can
  // disclose analytics without a consent banner.
  persistence: "memory",
} satisfies Partial<PostHogConfig>;

/**
 * Identify the signed-in org. Called from AuthGate right after the
 * successful GET /me (the console's only choke point) — the org id and
 * org name are the only person properties we set.
 */
export function identifyOrg(orgId: string, orgName: string): void {
  if (!POSTHOG_ENABLED) return;
  posthog.identify(orgId, { org_name: orgName });
}