"use client";

import { useState } from "react";
import { ArrowRight, ArrowUpRight, ChevronRight, MessagesSquare } from "lucide-react";
import { DeploymentStatusBadge } from "@/components/status-badges";
import Link from "next/link";
import { AppTabs } from "@/components/app-tabs";
import { deploymentVersionLabel } from "@/lib/view";
import { DeployDialog } from "@/components/deploy-dialog";
import { SessionTable } from "@/components/session-table";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatRelative } from "@/lib/events";
import { useSessionList } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import type { App, Deployment } from "@/lib/types";

/**
 * App overview tab body (agents + deployment history grid). CLIENT: the
 * tab bar's sessions count is a live read — see api.ts for why session
 * data is browser-fetched.
 */
export function AppDetailView({ app }: { app: App }) {
  const { sessions, loading } = useSessionList(app.id);
  // The Change column only appears once some deployment has a commit message.
  const showChange = app.deployments.some((d) => d.commit_message);
  // Server count is authoritative; the loaded page is a lower bound (the
  // dev gateway reports 0).
  const total = Math.max(app.sessions_count, sessions.length);
  // Counts are of the loaded sessions (the latest 50).
  const sessionsByRoute = new Map<string, number>();
  for (const s of sessions) sessionsByRoute.set(s.route, (sessionsByRoute.get(s.route) ?? 0) + 1);

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="overview" />

      <div className="grid gap-4 px-6 pt-6 lg:grid-cols-3">
        {/* Routes */}
        <Card className="lg:col-span-1">
          <CardHeader>
            <CardTitle className="text-lg tracking-tight">Agents</CardTitle>
            <CardDescription>Agents in this project</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {app.routes.map((r) => (
              <div
                key={r.path}
                className="flex items-center gap-2 rounded-md border border-border bg-background/50 px-3 py-2 transition-colors hover:border-brand/60 hover:bg-brand/[0.04]"
              >
                <div className="min-w-0 flex-1">
                  <div className="font-mono text-xs font-medium">{r.path}</div>
                  <div className="mt-0.5 truncate text-[11px] text-muted-foreground">
                    {r.description}
                  </div>
                </div>
                <span className="shrink-0 text-[11px] tabular-nums text-faint-foreground">
                  {sessionsByRoute.get(r.path) ?? 0}{" "}
                  {sessionsByRoute.get(r.path) === 1 ? "session" : "sessions"}
                </span>
                <Link
                  href={`/apps/${app.id}/playground?route=${encodeURIComponent(r.path)}`}
                  title={`Chat with ${r.path}`}
                  className="flex shrink-0 items-center gap-1 rounded-md border border-border bg-surface px-2.5 py-1 text-xs font-medium text-muted-foreground transition-colors hover:border-border-strong hover:bg-surface-raised hover:text-foreground"
                >
                  Chat
                  <ArrowUpRight className="size-3.5" />
                </Link>
              </div>
            ))}
          </CardContent>
        </Card>

        {/* Deployment history */}
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="text-lg tracking-tight">Deployments</CardTitle>
            <CardDescription>
              Sessions pin to the deployment they started on
            </CardDescription>
            <CardAction>
              <DeployDialog
                appName={app.name}
                nextVersion={app.latest_deployment.version + 1}
              />
            </CardAction>
          </CardHeader>
          <CardContent className="px-0 pb-2">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="pl-4">Version</TableHead>
                  <TableHead>Status</TableHead>
                  {showChange && <TableHead>Change</TableHead>}
                  <TableHead className="pr-4 text-right">Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {app.deployments.map((d) => (
                  <DeploymentRows key={d.id} d={d} showChange={showChange} />
                ))}
              </TableBody>
            </Table>
          </CardContent>
        </Card>

        {/* What people open a project for: the latest sessions. */}
        {!loading && sessions.length === 0 && (
          <Card className="items-center px-6 py-8 text-center lg:col-span-3">
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
          <Card className="pb-0 lg:col-span-3">
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
              <SessionTable sessions={sessions.slice(0, 5)} />
            </CardContent>
          </Card>
        )}
      </div>
    </>
  );
}

/** One deployment row; failed ones with a build error expand to show it. */
function DeploymentRows({ d, showChange }: { d: Deployment; showChange: boolean }) {
  const [open, setOpen] = useState(false);
  const expandable = d.status === "failed" && !!d.error;

  return (
    <>
      <TableRow
        className={cn(expandable && "cursor-pointer", d.active && "bg-brand/[0.04]")}
        onClick={expandable ? () => setOpen((o) => !o) : undefined}
        aria-expanded={expandable ? open : undefined}
      >
        {/* The current deployment gets a brand accent bar and tint. */}
        <TableCell className={cn("pl-4", d.active && "shadow-[inset_3px_0_0_0] shadow-brand")}>
          <span className="inline-flex items-center gap-1">
            {expandable && (
              <ChevronRight
                className={cn("size-3.5 text-muted-foreground transition-transform", open && "rotate-90")}
              />
            )}
            <span className="font-mono text-xs">{deploymentVersionLabel(d)}</span>
          </span>
        </TableCell>
        <TableCell>
          <DeploymentStatusBadge status={d.status} />
        </TableCell>
        {showChange && (
          <TableCell className="max-w-56 truncate text-xs text-muted-foreground">
            {d.commit_message ?? "—"}
          </TableCell>
        )}
        <TableCell className="pr-4 text-right text-xs text-muted-foreground">
          {d.created_at ? formatRelative(d.created_at) : "—"}
        </TableCell>
        {/* Rollback (activateDeployment) is not wired up yet.
        <TableCell className="pr-4 text-right">
          {!d.active && d.status === "active" && (
            <Button variant="ghost" size="icon-sm" title="Rollback to this deployment">
              <RotateCcw />
            </Button>
          )}
        </TableCell>
        */}
      </TableRow>
      {expandable && open && (
        <TableRow className="hover:bg-transparent">
          <TableCell colSpan={showChange ? 4 : 3} className="px-4 pb-3 pt-0">
            <pre className="overflow-x-auto whitespace-pre-wrap rounded-md border border-destructive/30 bg-destructive/5 p-3 font-mono text-xs text-destructive">
              {d.error}
            </pre>
          </TableCell>
        </TableRow>
      )}
    </>
  );
}
