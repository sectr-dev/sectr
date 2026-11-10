"use client";

import { ArrowRight, ArrowUpRight } from "lucide-react";
import Link from "next/link";
import { plural, StatChip } from "@/components/stat-chip";
import { BrandBackdrop } from "@/components/brand-backdrop";
import { NewProjectGhostCard, NewProjectMenu } from "@/components/new-project-card";
import { Welcome } from "@/components/welcome";
import { PageHeader } from "@/components/page-header";
import { RunsOnMark } from "@/components/platform-icons";
import { DeploymentStatusBadge } from "@/components/status-badges";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { providerLabel, runsOnFor } from "@/lib/connections";
import { CONSOLE_MODE } from "@/lib/console-mode";
import { FRAMEWORK_LABEL } from "@/lib/events";
import { deploymentVersionLabel } from "@/lib/view";
import { refreshApps, useApprovals, useApps, useAsync, useSessions } from "@/lib/hooks";
import * as api from "@/lib/api";

/** At most this many agent chips before "+N". */
const MAX_AGENT_CHIPS = 3;

/** The project's agents as one line of mono chips — never wraps: chips
 *  shrink and truncate, and past MAX_AGENT_CHIPS the rest fold into "+N"
 *  (the full list is in the tooltip). */
function AgentChips({ paths }: { paths: string[] }) {
  const shown = paths.slice(0, MAX_AGENT_CHIPS);
  const rest = paths.length - shown.length;
  return (
    <div
      className="mt-2.5 flex min-w-0 flex-nowrap items-center gap-1.5 overflow-hidden"
      title={paths.join("  ")}
    >
      {shown.map((p) => (
        <span
          key={p}
          className="min-w-0 shrink truncate rounded-md border border-border bg-surface px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground"
        >
          {p}
        </span>
      ))}
      {rest > 0 && (
        <span className="shrink-0 rounded-md px-1 py-0.5 text-[11px] text-faint-foreground">
          +{rest}
        </span>
      )}
    </div>
  );
}

/**
 * The overview body. CLIENT: sessions are browser-fetched (in dev-gateway
 * mode the edge can't reach localhost — see api.ts), and the live counts
 * interleave with the app cards. Apps come through the control-plane hooks.
 */
export function OverviewView() {
  // The welcome calls refreshApps() once the first project is live: the
  // refetch makes the page the grid (and the sidebar lists the project).
  const { apps, loading: appsLoading } = useApps();
  const sessions = useSessions();
  const pending = useApprovals();
  // The org name for the breadcrumb: "…" while loading, "Local dev" when
  // the backend has no org (the dev gateway has no auth or control plane).
  const { data: me, loading: meLoading } = useAsync(api.me, []);
  const orgLabel = me?.org.name ?? (meLoading ? "…" : "Local dev");

  const liveCount = sessions.filter(
    (s) => s.status === "active" || s.status === "awaiting_approval",
  ).length;

  // Nothing until the project list loads: an empty org gets the welcome,
  // anything else the grid, and showing either first would flash the other.
  if (appsLoading) return null;
  if (apps.length === 0)
    return <Welcome onFirstProject={refreshApps} />;

  return (
    <div className="relative isolate overflow-hidden pb-12">
      <BrandBackdrop strength="subtle" />
      <PageHeader
        title="Projects"
        breadcrumbs={[
          { label: orgLabel },
          { label: "Projects" },
        ]}
        actions={<NewProjectMenu />}
      >
        {/* Stat chips: live sessions pulse, pending approvals turn amber
            and link to the inbox — the line is something to act on. */}
        <div className="flex flex-wrap items-center gap-2">
          <StatChip>{plural(apps.length, "project")}</StatChip>
          <StatChip tone={liveCount > 0 ? "live" : "muted"}>
            {liveCount > 0 && (
              <span className="size-1.5 rounded-full bg-success animate-pulse-dot" />
            )}
            {plural(liveCount, "live session")}
          </StatChip>
          {pending.length > 0 ? (
            <Link href="/approvals" className="group/approvals inline-flex">
              <StatChip tone="attention">
                {pending.length} awaiting approval
                <ArrowRight className="size-3 transition-transform group-hover/approvals:translate-x-0.5" />
              </StatChip>
            </Link>
          ) : (
            <StatChip tone="muted">0 awaiting approval</StatChip>
          )}
        </div>
      </PageHeader>

      {/* App cards */}
      <div className="grid grid-cols-1 gap-4 px-6 pt-6 md:grid-cols-2">
        {apps.map((app) => {
          const live = sessions.filter(
            (s) => s.app_id === app.id && (s.status === "active" || s.status === "awaiting_approval"),
          ).length;
          const runsOn = runsOnFor(app, CONSOLE_MODE);
          const provider = providerLabel(runsOn.kind);
          return (
            <Link key={app.id} href={`/apps/${app.id}`} className="group rounded-xl outline-none">
              {/* Same orange hover/focus as the ghost card */}
              <Card className="h-full transition-colors group-hover:bg-brand/[0.04] group-hover:ring-brand/60 group-focus-visible:bg-brand/[0.04] group-focus-visible:ring-brand/60">
                <div className="flex items-start gap-3 px-4 pt-4">
                  {/* Same tinted tile as the welcome's icons */}
                  <span className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-lg border border-brand/25 bg-brand/10 text-[13px] font-semibold text-brand-foreground dark:text-brand">
                    {app.name[0].toUpperCase()}
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <h3 className="truncate text-sm font-semibold">{app.name}</h3>
                      <ArrowUpRight className="size-3.5 shrink-0 text-faint-foreground transition-colors group-hover:text-foreground" />
                    </div>
                    <div className="mt-0.5 flex items-center gap-1.5 font-mono text-[11px] text-muted-foreground">
                      {app.url ?? "local"}
                    </div>
                  </div>
                  <DeploymentStatusBadge status={app.latest_deployment.status} />
                </div>
                <div className="px-4 pt-3 pb-4">
                  <p className="truncate text-[15px] text-muted-foreground">
                    {app.description}
                  </p>
                  <AgentChips paths={app.routes.map((r) => r.path)} />
                  <div className="mt-3 flex items-center gap-2 border-t border-border pt-3 text-[11px] text-faint-foreground">
                    {app.framework && (
                      <Badge variant="muted">{FRAMEWORK_LABEL[app.framework]}</Badge>
                    )}
                    {provider && (
                      <Badge
                        variant="muted"
                        className="min-w-0 shrink"
                        title={runsOn.url ? `${provider} · ${runsOn.url}` : provider}
                      >
                        <RunsOnMark
                          kind={runsOn.kind}
                          bare
                          className="size-3.5 shrink-0 rounded-[3px] [&>img]:size-3.5"
                        />
                        <span className="truncate">{provider}</span>
                      </Badge>
                    )}
                    <Badge variant="muted" className="font-mono">
                      {deploymentVersionLabel(app.latest_deployment)}
                    </Badge>
                    <span className="ml-auto">
                      {live > 0 ? (
                        <span className="flex items-center gap-1.5 text-success">
                          <span className="size-1.5 rounded-full bg-success animate-pulse-dot" />
                          {live} live
                        </span>
                      ) : (
                        <span className="text-faint-foreground">idle</span>
                      )}
                    </span>
                  </div>
                </div>
              </Card>
            </Link>
          );
        })}
        {/* A project-card-sized ghost card that expands into the same
            options as the header's Create new. */}
        <NewProjectGhostCard />
      </div>
    </div>
  );
}
