"use client";

import { AppTabs } from "@/components/app-tabs";
import { ProjectSecrets } from "@/components/project-secrets";
import { useSessions } from "@/lib/hooks";
import type { App } from "@/lib/types";

/** Project secrets tab body. */
export function AppSecretsView({ app }: { app: App }) {
  const sessions = useSessions(app.id);
  const total = Math.max(app.sessions_count, sessions.length);

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="secrets" />
      <div className="max-w-3xl px-6 pt-6">
        <ProjectSecrets app={app} />
      </div>
    </>
  );
}
