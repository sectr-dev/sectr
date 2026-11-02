"use client";

import { Suspense } from "react";
import { AppTabs } from "@/components/app-tabs";
import { PlaygroundClient } from "@/components/playground-client";
import { useSessions } from "@/lib/hooks";
import type { App } from "@/lib/types";

/** Playground tab body. CLIENT: session data is browser-fetched (see api.ts). */
export function PlaygroundView({ app }: { app: App }) {
  const sessions = useSessions(app.id);
  // Server count is authoritative; the loaded page is a lower bound (the
  // dev gateway reports 0).
  const total = Math.max(app.sessions_count, sessions.length);

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="playground" />
      {/* useSearchParams in the playground needs a Suspense boundary to
          stay statically prerenderable */}
      <Suspense>
        <PlaygroundClient app={app} />
      </Suspense>
    </>
  );
}
