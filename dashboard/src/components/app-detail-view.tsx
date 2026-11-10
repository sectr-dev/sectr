"use client";

import { ArrowRight, MessagesSquare } from "lucide-react";
import Link from "next/link";
import { AppTabs } from "@/components/app-tabs";
import { ConnectionsDiagram } from "@/components/connections-diagram";
import { DeploymentsCard } from "@/components/deployments-card";
import { SessionTable } from "@/components/session-table";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { healthFor, runsOnFor } from "@/lib/connections";
import { CONSOLE_MODE as MODE } from "@/lib/console-mode";
import { useDeploymentWakes, useSessionList } from "@/lib/hooks";
import type { App } from "@/lib/types";

/**
 * App overview tab body: the connections diagram (where the agents run, and
 * how users reach them; see lib/connections.ts) across the top, then the
 * latest sessions beside the deployment history. CLIENT: the tab bar's
 * sessions count is a live read — see api.ts for why session data is
 * browser-fetched.
 */
export function AppDetailView({ app }: { app: App }) {
  const { sessions, loading } = useSessionList(app.id);
  // Server count is authoritative; the loaded page is a lower bound (the
  // dev gateway reports 0).
  const total = Math.max(app.sessions_count, sessions.length);
  // Counts are of the loaded sessions (the latest 50).
  const sessionsByRoute = new Map<string, number>();
  for (const s of sessions) sessionsByRoute.set(s.route, (sessionsByRoute.get(s.route) ?? 0) + 1);
  // For an endpoint deployment, the Runs on node carries how its recent
  // turns woke.
  const wakes = useDeploymentWakes(runsOnFor(app, MODE).healthDeploymentId);

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="overview" />

      <div className="grid items-start gap-4 px-6 pt-6 xl:grid-cols-5">
        {/* The diagram's canvas runs to the card's edges. */}
        <Card className="gap-0 pb-0 xl:col-span-5">
          <CardHeader className="border-b border-border">
            <CardTitle className="text-lg tracking-tight">Connections</CardTitle>
            <CardDescription>Where your agents run, and how users reach them</CardDescription>
          </CardHeader>
          <ConnectionsDiagram
            app={app}
            mode={MODE}
            sessionsByRoute={sessionsByRoute}
            health={healthFor(wakes)}
          />
        </Card>

        {/* What people open a project for: the latest sessions. */}
        {!loading && sessions.length === 0 && (
          <Card className="items-center px-6 py-8 text-center xl:col-span-3">
            <MessagesSquare className="size-5 text-faint-foreground" />
            <div>
              <p className="text-sm font-medium">No sessions yet</p>
              <p className="mt-1 text-xs text-muted-foreground">
                Chat with {app.name} to start the first one.
              </p>
            </div>
            <Button asChild size="sm">
              <Link href={`/apps/${app.id}/playground`}>Try it in the Playground</Link>
            </Button>
          </Card>
        )}
        {sessions.length > 0 && (
          <Card className="pb-0 xl:col-span-3">
            <CardHeader>
              <CardTitle className="text-lg tracking-tight">Recent sessions</CardTitle>
              <CardAction>
                <Link
                  href={`/apps/${app.id}/sessions`}
                  className="flex items-center gap-1 text-sm text-muted-foreground transition-colors hover:text-foreground"
                >
                  View all <ArrowRight className="size-3.5" />
                </Link>
              </CardAction>
            </CardHeader>
            <CardContent className="px-0">
              <SessionTable sessions={sessions.slice(0, 5)} compact />
            </CardContent>
          </Card>
        )}
        {/* While sessions load, the history keeps its column. */}
        <DeploymentsCard app={app} className="xl:col-span-2 xl:col-start-4" />
      </div>
    </>
  );
}
