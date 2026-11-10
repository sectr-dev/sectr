"use client";

import { useState } from "react";
import { ChevronRight, Server } from "lucide-react";
import { DeploymentStatusBadge } from "@/components/status-badges";
import { AppTabs } from "@/components/app-tabs";
import { deploymentVersionLabel } from "@/lib/view";
import { DeployDialog } from "@/components/deploy-dialog";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { hostOf } from "@/lib/connections";
import { formatRelative } from "@/lib/events";
import { useSessionList } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import type { App, Deployment } from "@/lib/types";

/**
 * The project's Deployments tab: its version history. CLIENT: the tab
 * bar's sessions count is a live read — see api.ts for why session data is
 * browser-fetched.
 */
export function AppDeploymentsView({ app }: { app: App }) {
  const { sessions } = useSessionList(app.id);
  // The Change column only appears once some deployment has a commit message.
  const showChange = app.deployments.some((d) => d.commit_message);
  // Server count is authoritative; the loaded page is a lower bound (the
  // dev gateway reports 0).
  const total = Math.max(app.sessions_count, sessions.length);
  const current = app.deployments.find((d) => d.active) ?? app.latest_deployment;

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="deployments" />

      <div className="px-6 pt-6">
        <Card>
          <CardHeader>
            <CardTitle className="text-lg tracking-tight">Deployments</CardTitle>
            <CardDescription>
              Sessions pin to the deployment they started on
            </CardDescription>
            <CardAction>
              <DeployDialog
                appName={app.name}
                nextVersion={app.latest_deployment.version + 1}
                endpoint={!!current.endpoint_url}
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
      </div>
    </>
  );
}

/** One deployment row. Failed ones with an error expand to show it; endpoint
 *  ones expand to their URL and the rollback caveat. */
function DeploymentRows({ d, showChange }: { d: Deployment; showChange: boolean }) {
  const [open, setOpen] = useState(false);
  const failed = d.status === "failed" && !!d.error;
  const endpoint = d.endpoint_url ?? null;
  const expandable = failed || !!endpoint;

  return (
    <>
      <TableRow
        className={cn(expandable && "cursor-pointer", d.active && "bg-brand/[0.04]")}
        onClick={expandable ? () => setOpen((o) => !o) : undefined}
        aria-expanded={expandable ? open : undefined}
      >
        {/* The current deployment gets a brand accent bar and tint. */}
        <TableCell className={cn("pl-4", d.active && "shadow-[inset_3px_0_0_0] shadow-brand")}>
          <span className="inline-flex items-center gap-1.5">
            {expandable && (
              <ChevronRight
                className={cn("size-3.5 text-muted-foreground transition-transform", open && "rotate-90")}
              />
            )}
            <span className="font-mono text-xs">{deploymentVersionLabel(d)}</span>
            {endpoint && (
              <span
                className="inline-flex max-w-56 items-center gap-1 rounded-full border border-border bg-surface px-1.5 py-px text-[10px] text-muted-foreground"
                title={endpoint}
              >
                <Server className="size-3 shrink-0" />
                Endpoint
                <span className="truncate font-mono text-faint-foreground">{hostOf(endpoint)}</span>
              </span>
            )}
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
          <TableCell colSpan={showChange ? 4 : 3} className="space-y-2 px-4 pb-3 pt-0">
            {endpoint && (
              <div className="rounded-md border border-border bg-background/60 p-3 text-xs">
                <div className="font-mono break-all">{endpoint}</div>
                <p className="mt-1.5 text-muted-foreground">
                  Runs on your own endpoint. Activating an older version only rolls back if
                  its URL still serves that version&apos;s image.
                </p>
              </div>
            )}
            {failed && (
              <pre className="overflow-x-auto whitespace-pre-wrap rounded-md border border-destructive/30 bg-destructive/5 p-3 font-mono text-xs text-destructive">
                {d.error}
              </pre>
            )}
          </TableCell>
        </TableRow>
      )}
    </>
  );
}
