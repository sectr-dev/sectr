"use client";

import { useState } from "react";
import { ChevronRight } from "lucide-react";
import { EndpointPlatformIcon } from "@/components/platform-icons";
import { DeploymentStatusBadge } from "@/components/status-badges";
import { Card, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { hostOf } from "@/lib/connections";
import { formatRelative } from "@/lib/events";
import { cn } from "@/lib/utils";
import type { App, Deployment } from "@/lib/types";
import { deploymentVersionLabel } from "@/lib/view";

/** How many versions show before "Show all". */
const LIMIT = 6;

/** The project's version history, on its Overview: a compact list, newest
 *  first. (Deploying is the page header's Deploy button.) */
export function DeploymentsCard({ app, className }: { app: App; className?: string }) {
  const [all, setAll] = useState(false);
  const activeUrl = app.deployments.find((d) => d.active)?.endpoint_url ?? null;
  const shown = all ? app.deployments : app.deployments.slice(0, LIMIT);
  // Rows without a chevron keep its space when some row has one.
  const anyExpandable = app.deployments.some(expandable);

  return (
    <Card className={cn("gap-0 pb-0", className)}>
      <CardHeader className="border-b border-border">
        <CardTitle className="text-lg tracking-tight">Deployments</CardTitle>
        <CardDescription>Sessions pin to the deployment they started on</CardDescription>
      </CardHeader>
      <ul className="divide-y divide-border">
        {shown.map((d) => (
          <DeploymentRow
            key={d.id}
            d={d}
            // An endpoint version names its host when it's the live one, or
            // when it ran somewhere other than the live one.
            showHost={!!d.endpoint_url && (d.active || d.endpoint_url !== activeUrl)}
            indent={anyExpandable}
          />
        ))}
      </ul>
      {app.deployments.length > LIMIT && (
        <button
          type="button"
          onClick={() => setAll((a) => !a)}
          className="border-t border-border px-4 py-2 text-left text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          {all ? "Show fewer" : `Show all ${app.deployments.length}`}
        </button>
      )}
    </Card>
  );
}

/** Failed versions with an error expand to show it; endpoint ones expand
 *  to their URL and the rollback caveat. */
function expandable(d: Deployment): boolean {
  return (d.status === "failed" && !!d.error) || !!d.endpoint_url;
}

function DeploymentRow({ d, showHost, indent }: { d: Deployment; showHost: boolean; indent: boolean }) {
  const [open, setOpen] = useState(false);
  const failed = d.status === "failed" && !!d.error;
  const endpoint = d.endpoint_url ?? null;
  const canOpen = expandable(d);

  const summary = (
    <>
      {indent && (
        <ChevronRight
          className={cn(
            "mt-0.5 size-3.5 shrink-0 text-muted-foreground transition-transform",
            open && "rotate-90",
            !canOpen && "invisible",
          )}
        />
      )}
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2">
          <span className="font-mono text-xs font-medium">{deploymentVersionLabel(d)}</span>
          <DeploymentStatusBadge status={d.status} />
          <span className="ml-auto shrink-0 text-xs text-muted-foreground">
            {d.created_at ? formatRelative(d.created_at) : "—"}
          </span>
        </span>
        {d.commit_message && (
          <span className="mt-1 block truncate text-xs text-muted-foreground" title={d.commit_message}>
            {d.commit_message}
          </span>
        )}
        {endpoint && showHost && (
          <span className="mt-1 flex items-center gap-1.5 text-[11px] text-muted-foreground" title={endpoint}>
            <EndpointPlatformIcon url={endpoint} className="size-3 shrink-0" />
            <span className="truncate font-mono">{hostOf(endpoint)}</span>
          </span>
        )}
      </span>
    </>
  );
  // The current deployment gets a brand accent bar and tint.
  const rowClass = cn(
    "flex w-full items-start gap-2 px-4 py-2.5 text-left",
    d.active && "bg-brand/[0.04] shadow-[inset_3px_0_0_0] shadow-brand",
  );

  return (
    <li>
      {canOpen ? (
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
          className={cn(rowClass, "transition-colors hover:bg-muted/50", d.active && "hover:bg-brand/[0.07]")}
        >
          {summary}
        </button>
      ) : (
        <div className={rowClass}>{summary}</div>
      )}
      {/* Rollback (activateDeployment) is not wired up yet. */}
      {canOpen && open && (
        <div className="space-y-2 px-4 pb-3">
          {endpoint && (
            <div className="rounded-md border border-border bg-background/60 p-3 text-xs">
              <div className="font-mono break-all">{endpoint}</div>
              <p className="mt-1.5 text-muted-foreground">
                Runs on your own compute endpoint. Activating an older version only rolls back if
                its URL still serves that version&apos;s image.
              </p>
            </div>
          )}
          {failed && (
            <pre className="overflow-x-auto whitespace-pre-wrap rounded-md border border-destructive/30 bg-destructive/5 p-3 font-mono text-xs text-destructive">
              {d.error}
            </pre>
          )}
        </div>
      )}
    </li>
  );
}
