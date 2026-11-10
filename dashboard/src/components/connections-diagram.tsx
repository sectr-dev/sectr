"use client";

import { useCallback, useLayoutEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowUpRight, Cloud, Copy, Cpu, Globe, Laptop, Plug, Server } from "lucide-react";
import { toast } from "sonner";
import { aguiBase } from "@/components/connect-frontend-menu";
import {
  deriveConnections,
  hostOf,
  type ConsoleMode,
  type FrontendKind,
  type HealthLine,
  type RunsOnKind,
} from "@/lib/connections";
import type { App } from "@/lib/types";
import { cn } from "@/lib/utils";

/*
  The connections wire diagram: frontends (how users reach an agent) on the
  left, one node per agent in the middle, and where the agents run on the
  right, joined by orthogonal wires. One frontend serves exactly one agent;
  every agent runs on the project's active deployment, so the Runs on node
  is wired to all of them.

  Wires are SVG paths measured from the rendered boxes (ResizeObserver), so
  the diagram survives responsive reflow. Below `lg` the columns stack and
  the wires hide.
*/

const FRONTEND_ICON: Record<FrontendKind, typeof Globe> = {
  http: Globe,
  "ag-ui": Plug,
};

const RUNS_ON_ICON: Record<RunsOnKind, typeof Globe> = {
  lambda: Cloud,
  "cloud-run": Cloud,
  endpoint: Server,
  sectr: Cpu,
  local: Laptop,
  none: Server,
};

interface Edge {
  /** the node keys at either end */
  ends: [string, string];
  d: string;
}

const RUNS_ON_KEY = "runs-on";

/** Orthogonal elbow from (x1,y1) rightward to (x2,y2), rounded corners. */
function elbow(x1: number, y1: number, x2: number, y2: number): string {
  const r = Math.min(8, Math.abs(y2 - y1) / 2, Math.abs(x2 - x1) / 2);
  if (Math.abs(y2 - y1) < 1) return `M ${x1} ${y1} H ${x2}`;
  const dir = y2 > y1 ? 1 : -1;
  return [
    `M ${x1} ${y1}`,
    `H ${x2 - r - 12}`,
    `Q ${x2 - 12} ${y1} ${x2 - 12} ${y1 + dir * r}`,
    `V ${y2 - dir * r}`,
    `Q ${x2 - 12} ${y2} ${x2 - 12 + r} ${y2}`,
    `H ${x2}`,
  ].join(" ");
}

export function ConnectionsDiagram({
  app,
  mode,
  sessionsByRoute,
  health,
}: {
  app: App;
  mode: ConsoleMode;
  /** Sessions per route, of the loaded sessions. */
  sessionsByRoute: Map<string, number>;
  /** The endpoint's health line, once fetched (endpoint deployments only). */
  health: HealthLine | null;
}) {
  const base = aguiBase();
  const { frontends, agents, runsOn } = useMemo(
    () => deriveConnections(app, base, mode),
    [app, base, mode],
  );
  const wired = runsOn.kind !== "none";
  const [hovered, setHovered] = useState<string | null>(null);
  const [edges, setEdges] = useState<Edge[]>([]);
  const containerRef = useRef<HTMLDivElement>(null);
  // The measured boxes, by node key: one mutable map for the component's
  // life (filled by callback refs, read when measuring).
  const [nodes] = useState(() => new Map<string, HTMLElement>());

  const measure = useCallback(() => {
    const container = containerRef.current;
    if (!container) return;
    const cRect = container.getBoundingClientRect();
    const rectOf = (key: string) => {
      const el = nodes.get(key);
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return {
        left: r.left - cRect.left,
        right: r.right - cRect.left,
        cy: r.top - cRect.top + r.height / 2,
      };
    };
    const next: Edge[] = [];
    for (const fe of frontends) {
      const ends: [string, string] = [`fe:${fe.id}`, `agent:${fe.route}`];
      const from = rectOf(ends[0]);
      const to = rectOf(ends[1]);
      if (from && to) next.push({ ends, d: elbow(from.right, from.cy, to.left, to.cy) });
    }
    if (wired) {
      const target = rectOf(RUNS_ON_KEY);
      for (const agent of agents) {
        const ends: [string, string] = [`agent:${agent.route}`, RUNS_ON_KEY];
        const from = rectOf(ends[0]);
        if (from && target) next.push({ ends, d: elbow(from.right, from.cy, target.left, target.cy) });
      }
    }
    setEdges(next);
  }, [frontends, agents, wired, nodes]);

  // The observer fires once on observe, so this also does the first draw.
  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver(measure);
    ro.observe(container);
    return () => ro.disconnect();
  }, [measure]);

  const setNodeRef = (key: string) => (el: HTMLElement | null) => {
    if (el) nodes.set(key, el);
    else nodes.delete(key);
  };
  const hoverProps = (key: string) => ({
    onMouseEnter: () => setHovered(key),
    onMouseLeave: () => setHovered(null),
  });
  const wireActive = (e: Edge) => hovered != null && e.ends.includes(hovered);
  const RunsOnIcon = RUNS_ON_ICON[runsOn.kind];

  return (
    <div
      ref={containerRef}
      className="relative grid gap-10 px-2 py-4 lg:grid-cols-[minmax(190px,240px)_minmax(260px,1fr)_minmax(200px,260px)]"
    >
      {/* Wires — only in the three-column layout; stacked columns have
          nothing to wire across. */}
      <svg className="pointer-events-none absolute inset-0 hidden size-full lg:block" aria-hidden>
        {edges.map((e) => (
          <path
            key={e.ends.join("->")}
            d={e.d}
            fill="none"
            strokeWidth={wireActive(e) ? 2 : 1.25}
            className={cn(
              "stroke-brand transition-opacity duration-150",
              wireActive(e) ? "opacity-100" : "opacity-40",
            )}
          />
        ))}
      </svg>

      {/* Left column — Frontends */}
      <div className="relative z-10 flex flex-col gap-4">
        <ColumnLabel>Frontends</ColumnLabel>
        {frontends.map((fe) => {
          const key = `fe:${fe.id}`;
          const Icon = FRONTEND_ICON[fe.kind];
          return (
            <div
              key={fe.id}
              ref={setNodeRef(key)}
              {...hoverProps(key)}
              className={cn(
                "rounded-lg border border-border-strong bg-card px-3 py-2.5 shadow-sm transition-shadow",
                hovered === key && "shadow-md",
              )}
            >
              <div className="flex items-center gap-2">
                <Icon className="size-3.5 shrink-0 text-brand" />
                <span className="truncate text-[13px] font-medium">{fe.label}</span>
              </div>
              <div className="mt-1 truncate text-[11px] text-muted-foreground" title={fe.detail}>
                {fe.detail}
              </div>
            </div>
          );
        })}
      </div>

      {/* Center column — Agents */}
      <div className="relative z-10 flex flex-col justify-center gap-8 py-2">
        <ColumnLabel>Agents</ColumnLabel>
        {agents.length === 0 && (
          <p className="text-center text-xs text-muted-foreground">This project has no agents yet.</p>
        )}
        {agents.map((agent) => {
          const key = `agent:${agent.route}`;
          const count = sessionsByRoute.get(agent.route) ?? 0;
          const curl =
            `curl -X POST ${base}/sessions ` +
            `-H "Authorization: Bearer $SECTR_INVOCATION_KEY" ` +
            `-H "Content-Type: application/json" ` +
            `-d '{"app": "${app.slug}", "route": "${agent.route}", "input": "hi"}'`;
          return (
            <div
              key={agent.route}
              ref={setNodeRef(key)}
              {...hoverProps(key)}
              className={cn(
                "rounded-xl border bg-card px-4 py-3 transition-shadow",
                hovered === key ? "border-brand/50 shadow-md" : "border-border-strong shadow-sm",
              )}
            >
              <div className="flex items-center gap-2">
                <span className="flex size-6 shrink-0 items-center justify-center rounded-md border border-brand/25 bg-brand/10 text-[11px] font-semibold text-brand-foreground dark:text-brand">
                  {app.name[0]?.toUpperCase()}
                </span>
                <span className="min-w-0">
                  <span className="block truncate font-mono text-[13px] font-medium leading-tight">
                    {agent.route}
                  </span>
                  {agent.description && (
                    <span className="block truncate text-[11px] text-muted-foreground">
                      {agent.description}
                    </span>
                  )}
                </span>
                {runsOn.version && (
                  <span className="ml-auto shrink-0 rounded-full border border-success/30 bg-success/10 px-1.5 py-px font-mono text-[10px] text-success">
                    {runsOn.version}
                  </span>
                )}
              </div>
              <div className="mt-2 flex items-center gap-2 text-[11px] text-muted-foreground">
                <span className="tabular-nums">
                  {count} {count === 1 ? "session" : "sessions"}
                </span>
                <Link
                  href={`/apps/${app.id}/playground?route=${encodeURIComponent(agent.route)}`}
                  title={`Chat with ${agent.route}`}
                  className="ml-auto flex shrink-0 items-center gap-1 rounded-md border border-border bg-surface px-2 py-0.5 font-medium transition-colors hover:border-border-strong hover:bg-surface-raised hover:text-foreground"
                >
                  Chat
                  <ArrowUpRight className="size-3" />
                </Link>
              </div>
              <button
                type="button"
                onClick={() => {
                  void navigator.clipboard.writeText(curl);
                  toast("Invoke command copied", { description: `POST ${base}/sessions · ${agent.route}` });
                }}
                className="mt-2 flex w-full items-center gap-1.5 rounded border border-border bg-background/60 px-2 py-1 font-mono text-[10px] text-muted-foreground transition-colors hover:text-foreground"
                title="Copy a curl that starts a session with this agent"
              >
                <span className="truncate">POST {base}/sessions</span>
                <Copy className="ml-auto size-3 shrink-0" />
              </button>
            </div>
          );
        })}
      </div>

      {/* Right column — Runs on */}
      <div className="relative z-10 flex flex-col justify-center gap-4">
        <ColumnLabel>Runs on</ColumnLabel>
        <div
          ref={setNodeRef(RUNS_ON_KEY)}
          {...hoverProps(RUNS_ON_KEY)}
          className={cn(
            "block rounded-lg border bg-card px-3 py-2.5 transition-shadow",
            wired ? "border-border-strong shadow-sm" : "border-dashed border-border",
            hovered === RUNS_ON_KEY && "shadow-md",
          )}
        >
          <div className="flex items-center gap-2">
            <RunsOnIcon className={cn("size-3.5 shrink-0", wired ? "text-brand" : "text-muted-foreground")} />
            <span className={cn("truncate text-[13px] font-medium", !wired && "text-muted-foreground")}>
              {runsOn.title}
            </span>
            {runsOn.version && runsOn.url && (
              <span className="ml-auto shrink-0 font-mono text-[10px] text-faint-foreground">
                {runsOn.version}
              </span>
            )}
          </div>
          <div
            className={cn("mt-1 truncate text-[11px] text-muted-foreground", runsOn.url && "font-mono")}
            title={runsOn.detail}
          >
            {runsOn.url ? hostOf(runsOn.url) : runsOn.detail}
          </div>
          {runsOn.healthDeploymentId && <HealthText health={health} />}
          <Link
            href={`/apps/${app.id}/deployments`}
            className="mt-2 inline-flex items-center gap-0.5 text-[11px] text-muted-foreground transition-colors hover:text-foreground"
          >
            Deployments <ArrowUpRight className="size-3" />
          </Link>
        </div>
      </div>
    </div>
  );
}

function HealthText({ health }: { health: HealthLine | null }) {
  if (!health) {
    return <div className="mt-1.5 h-4 w-28 animate-pulse rounded bg-muted" aria-label="Loading wakes" />;
  }
  return (
    <div className="mt-1.5 space-y-0.5 text-[11px]">
      <div
        className={cn(
          health.tone === "ok" && "text-success",
          health.tone === "warn" && "text-warning",
          health.tone === "muted" && "text-faint-foreground",
        )}
      >
        {health.tone === "ok" ? "✓ " : ""}
        {health.text}
      </div>
      {health.failure && (
        <Link
          href={`/sessions/${health.failure.session_id}`}
          className="line-clamp-2 text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
          title={health.failure.message}
        >
          Latest: {health.failure.message}
        </Link>
      )}
    </div>
  );
}

function ColumnLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="text-center text-[10px] font-medium uppercase tracking-widest text-faint-foreground">
      {children}
    </div>
  );
}
