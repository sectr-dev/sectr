"use client";

import { AppTabs } from "@/components/app-tabs";
import { ConnectionsDiagram } from "@/components/connections-diagram";
import { Card, CardContent } from "@/components/ui/card";
import * as api from "@/lib/api";
import { healthLine, runsOnFor, type ConsoleMode } from "@/lib/connections";
import { useDeploymentWakes, useSessionList } from "@/lib/hooks";
import type { App } from "@/lib/types";

/** The dev gateway (`sectr dev`) runs the agent on this machine. */
const MODE: ConsoleMode = !api.MOCK_MODE && !api.PROD_MODE ? "dev" : "server";

/**
 * The project's first tab: how users reach its agents, and where they run
 * (see lib/connections.ts). For an endpoint deployment, the Runs on node
 * carries how its recent turns woke.
 */
export function AppConnectionsView({ app }: { app: App }) {
  const { sessions } = useSessionList(app.id);
  // Server count is authoritative; the loaded page is a lower bound.
  const total = Math.max(app.sessions_count, sessions.length);
  // Counts are of the loaded sessions (the latest 50).
  const sessionsByRoute = new Map<string, number>();
  for (const s of sessions) sessionsByRoute.set(s.route, (sessionsByRoute.get(s.route) ?? 0) + 1);
  const wakes = useDeploymentWakes(runsOnFor(app, MODE).healthDeploymentId);

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="connections" />
      <div className="mx-auto max-w-5xl px-6 pt-6">
        <p className="mb-3 text-sm text-muted-foreground">
          How users reach your agents, and where your agents run.
        </p>
        <Card>
          <CardContent>
            <ConnectionsDiagram
              app={app}
              mode={MODE}
              sessionsByRoute={sessionsByRoute}
              health={wakes ? healthLine(wakes) : null}
            />
          </CardContent>
        </Card>
      </div>
    </>
  );
}
