"use client";

/**
 * The PostHog provider — the dashboard's in-app analytics +
 * frontend error capture. A client component wrapping the whole console
 * in the root layout: posthog-js is browser-only, so it must never run
 * during SSR (guarded by POSTHOG_ENABLED + the client boundary).
 *
 * The provider is a no-op wrapper when NEXT_PUBLIC_POSTHOG_KEY is unset
 * (local dev, demo mode) — nothing loads, nothing reports.
 */

import { PostHogProvider as PostHogJsProvider } from "posthog-js/react";

import { POSTHOG_ENABLED, POSTHOG_KEY, POSTHOG_OPTIONS } from "@/lib/analytics";

export function PostHogProvider({ children }: { children: React.ReactNode }) {
  if (!POSTHOG_ENABLED) {
    return <>{children}</>;
  }
  return (
    <PostHogJsProvider apiKey={POSTHOG_KEY} options={POSTHOG_OPTIONS}>
      {children}
    </PostHogJsProvider>
  );
}