"use client";

import { useCallback, useLayoutEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowRight, ArrowUpRight, Bot, Globe } from "lucide-react";
import { aguiBase, ConnectFrontendDialog } from "@/components/connect-frontend-menu";
import { AgUiIcon } from "@/components/framework-icons";
import { LogoMark } from "@/components/logo";
import { RunsOnMark } from "@/components/platform-icons";
import {
  API_REFERENCE,
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
  The connections wire diagram, on a dotted canvas: where the agents run on
  the left, one node per agent in the middle, and the frontends (how users
  reach an agent) on the right, joined by orthogonal wires. Every agent runs
  on the project's active deployment, so the Runs on node is wired to all of
  them; one frontend serves exactly one agent.

  One grid row per agent holds the agent and its frontends, so each agent
  sits level with what reaches it; the Runs on node spans every row. Wires
  are SVG paths measured from the rendered boxes (ResizeObserver), so the
  diagram survives reflow. In a narrow container the nodes stack and the
  wires hide.
*/

const FRONTEND_ICON: Record<FrontendKind, React.ComponentType<{ className?: string }>> = {
  http: Globe,
  "ag-ui": AgUiIcon,
};

interface Edge {
  /** the node keys at either end, left then right */
  ends: [string, string];
  /** the agent this wire belongs to */
  route: string;
  d: string;
  /** where it meets its two nodes */
  ports: [[number, number], [number, number]];
}

const RUNS_ON_KEY = "runs-on";
const agentKey = (route: string) => `agent:${route}`;

/** Orthogonal elbow from (x1,y1) rightward to (x2,y2), turning halfway,
 *  with rounded corners. */
function elbow(x1: number, y1: number, x2: number, y2: number): string {
  if (Math.abs(y2 - y1) < 1) return `M ${x1} ${y1} H ${x2}`;
  const mid = (x1 + x2) / 2;
  const r = Math.min(8, Math.abs(y2 - y1) / 2, Math.abs(x2 - x1) / 2);
  const dir = y2 > y1 ? 1 : -1;
  return [
    `M ${x1} ${y1}`,
    `H ${mid - r}`,
    `Q ${mid} ${y1} ${mid} ${y1 + dir * r}`,
    `V ${y2 - dir * r}`,
    `Q ${mid} ${y2} ${mid + r} ${y2}`,
    `H ${x2}`,
  ].join(" ");
}

const NODE =
  "rounded-lg border bg-card px-3 py-2.5 shadow-sm transition-[border-color,box-shadow] duration-150";
/** A node's one action: a small bordered pill. */
const NODE_ACTION =
  "group/action flex shrink-0 items-center gap-1 rounded-md border border-border bg-surface px-2 py-0.5 text-[11px] font-medium text-muted-foreground transition-colors hover:border-border-strong hover:bg-surface-raised hover:text-foreground";

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
  // The route whose AG-UI node opened the connect modal, while it's open.
  const [connectRoute, setConnectRoute] = useState<string | null>(null);
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
    const wire = (ends: [string, string], route: string) => {
      const from = rectOf(ends[0]);
      const to = rectOf(ends[1]);
      if (!from || !to) return;
      next.push({
        ends,
        route,
        d: elbow(from.right, from.cy, to.left, to.cy),
        ports: [
          [from.right, from.cy],
          [to.left, to.cy],
        ],
      });
    };
    if (wired) for (const agent of agents) wire([RUNS_ON_KEY, agentKey(agent.route)], agent.route);
    for (const fe of frontends) wire([agentKey(fe.route), `fe:${fe.id}`], fe.route);
    setEdges(next);
  }, [frontends, agents, wired, nodes]);

  // Remeasure when the canvas or any node resizes (a node grows when its
  // health line loads). The observer fires once on observe, so this also
  // does the first draw.
  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver(measure);
    ro.observe(container);
    for (const el of nodes.values()) ro.observe(el);
    return () => ro.disconnect();
  }, [measure, nodes]);

  const setNodeRef = (key: string) => (el: HTMLElement | null) => {
    if (el) nodes.set(key, el);
    else nodes.delete(key);
  };
  const hoverProps = (key: string) => ({
    onMouseEnter: () => setHovered(key),
    onMouseLeave: () => setHovered(null),
  });

  // Hovering a node lights the whole path through it: a frontend's wire
  // and its agent's wire to the Runs on node, all of an agent's wires, or
  // everything for the Runs on node.
  const frontendRoute = new Map(frontends.map((f) => [`fe:${f.id}`, f.route]));
  const wireLit = (e: Edge) => {
    if (hovered == null) return false;
    if (hovered === RUNS_ON_KEY || hovered === agentKey(e.route)) return true;
    return frontendRoute.get(hovered) === e.route && (e.ends[1] === hovered || e.ends[0] === RUNS_ON_KEY);
  };
  const litNodes = new Set<string>(hovered ? [hovered] : []);
  for (const e of edges) if (wireLit(e)) e.ends.forEach((k) => litNodes.add(k));
  const nodeClass = (key: string) =>
    cn(NODE, litNodes.has(key) ? "border-brand/60" : "border-border-strong", hovered === key && "shadow-md");

  return (
    <div ref={containerRef} className="@container relative bg-dots px-6 py-8">
      {/* Wires and their ports, above the nodes so a port sits on its
          node's edge. Only in the three-column layout: stacked nodes have
          nothing to wire across. */}
      <svg className="pointer-events-none absolute inset-0 z-20 hidden size-full @3xl:block" aria-hidden>
        {[...edges]
          .sort((a, b) => Number(wireLit(a)) - Number(wireLit(b)))
          .map((e) => {
            const lit = wireLit(e);
            return (
              <g
                key={e.ends.join("->")}
                className={cn("transition-colors duration-150", lit ? "stroke-brand" : "stroke-border-strong")}
              >
                <path d={e.d} fill="none" strokeWidth={lit ? 2 : 1.5} strokeLinecap="round" />
                {e.ports.map(([x, y], i) => (
                  <circle key={i} cx={x} cy={y} r={3} strokeWidth={1.5} className="fill-card" />
                ))}
              </g>
            );
          })}
      </svg>

      <div
        className="relative z-10 mx-auto grid max-w-[62rem] gap-y-3 @3xl:grid-cols-[minmax(0,17rem)_minmax(0,19rem)_minmax(0,18rem)] @3xl:justify-center @3xl:gap-x-10 @3xl:gap-y-5 @4xl:gap-x-14"
        style={{ "--rows": Math.max(agents.length, 1) } as React.CSSProperties}
      >
        <ColumnLabel className="@3xl:col-start-1">Compute provider</ColumnLabel>

        {/* Left column — Runs on, level with the middle of the agents */}
        <div
          ref={setNodeRef(RUNS_ON_KEY)}
          {...hoverProps(RUNS_ON_KEY)}
          className={cn(
            nodeClass(RUNS_ON_KEY),
            "@3xl:col-start-1 @3xl:self-center @3xl:[grid-row:2/span_var(--rows)]",
            !wired && "border-dashed shadow-none",
          )}
        >
          <div className="flex items-center gap-2.5">
            <RunsOnIcon kind={runsOn.kind} />
            <div className="min-w-0 flex-1">
              <div className={cn("truncate text-[13px] font-medium leading-tight", !wired && "text-muted-foreground")}>
                {runsOn.title}
              </div>
              <div
                className={cn("mt-0.5 truncate text-[11px] text-muted-foreground", runsOn.url && "font-mono")}
                title={runsOn.detail}
              >
                {runsOn.url ? hostOf(runsOn.url) : runsOn.detail}
              </div>
            </div>
            {runsOn.version && (
              <span
                className="shrink-0 rounded-full border border-success/30 bg-success/10 px-1.5 py-px font-mono text-[10px] text-success"
                title="The active version: every agent runs on it"
              >
                {runsOn.version}
              </span>
            )}
          </div>
          {runsOn.healthDeploymentId && (
            <div className="mt-2.5 border-t border-border pt-2">
              <HealthText health={health} />
            </div>
          )}
        </div>

        <ColumnLabel className="mt-3 @3xl:col-start-2 @3xl:mt-0">Agents</ColumnLabel>
        <ColumnLabel className="hidden @3xl:col-start-3 @3xl:block">Frontend APIs</ColumnLabel>

        {agents.length === 0 && (
          <p className="text-xs text-muted-foreground @3xl:col-start-2 @3xl:row-start-2 @3xl:self-center">
            This project has no agents yet.
          </p>
        )}

        {agents.map((agent, i) => {
          const key = agentKey(agent.route);
          const count = sessionsByRoute.get(agent.route) ?? 0;
          const row = { "--row": i + 2 } as React.CSSProperties;
          return [
            /* Center column — the agent */
            <div
              key={key}
              ref={setNodeRef(key)}
              {...hoverProps(key)}
              style={row}
              className={cn(nodeClass(key), "@3xl:col-start-2 @3xl:self-center @3xl:[grid-row:var(--row)]")}
            >
              <div className="flex items-center gap-2.5">
                <NodeIcon tone="brand">
                  <Bot className="size-4" />
                </NodeIcon>
                <div className="min-w-0 flex-1">
                  <div className="truncate font-mono text-[13px] font-medium leading-tight">{agent.route}</div>
                  {agent.description && (
                    <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{agent.description}</div>
                  )}
                </div>
              </div>
              <div className="mt-2.5 flex items-center border-t border-border pt-2 text-[11px] text-muted-foreground">
                <span className="tabular-nums">
                  {count} {count === 1 ? "session" : "sessions"}
                </span>
                <Link
                  href={`/apps/${app.id}/playground?route=${encodeURIComponent(agent.route)}`}
                  title={`Chat with ${agent.route}`}
                  className={cn(NODE_ACTION, "ml-auto")}
                >
                  Chat
                  <ArrowRight className="size-3 transition-transform group-hover/action:translate-x-0.5" />
                </Link>
              </div>
            </div>,

            /* Right column — the agent's frontends */
            <div
              key={`fe:${agent.route}`}
              style={row}
              className="ml-5 flex flex-col gap-2.5 border-l border-border pl-4 @3xl:col-start-3 @3xl:ml-0 @3xl:self-center @3xl:border-l-0 @3xl:pl-0 @3xl:[grid-row:var(--row)]"
            >
              {frontends
                .filter((fe) => fe.route === agent.route)
                .map((fe) => {
                  const feKey = `fe:${fe.id}`;
                  const Icon = FRONTEND_ICON[fe.kind];
                  return (
                    <div
                      key={fe.id}
                      ref={setNodeRef(feKey)}
                      {...hoverProps(feKey)}
                      className={cn(nodeClass(feKey), "flex items-center gap-2.5")}
                    >
                      <NodeIcon>
                        <Icon className={fe.kind === "ag-ui" ? "size-4" : "size-3.5"} />
                      </NodeIcon>
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-[13px] font-medium leading-tight">{fe.label}</div>
                        <div
                          className={cn(
                            "mt-0.5 truncate text-[11px] text-muted-foreground",
                            fe.kind === "http" && "font-mono",
                          )}
                          title={fe.detail}
                        >
                          {fe.detail}
                        </div>
                      </div>
                      {fe.kind === "http" ? (
                        <a
                          href={API_REFERENCE}
                          target="_blank"
                          rel="noopener noreferrer"
                          title="The HTTP API reference"
                          className={NODE_ACTION}
                        >
                          Docs
                          <ArrowUpRight className="size-3" />
                        </a>
                      ) : (
                        <button
                          type="button"
                          title="Connect your frontend"
                          className={NODE_ACTION}
                          onClick={() => setConnectRoute(fe.route)}
                        >
                          Connect
                          <ArrowRight className="size-3 transition-transform group-hover/action:translate-x-0.5" />
                        </button>
                      )}
                    </div>
                  );
                })}
            </div>,
          ];
        })}
      </div>

      <ConnectFrontendDialog
        app={app}
        open={connectRoute != null}
        onOpenChange={(open) => !open && setConnectRoute(null)}
        route={connectRoute ?? undefined}
      />
    </div>
  );
}

/** A node's leading icon, in a small framed square. */
function NodeIcon({ tone = "plain", children }: { tone?: "plain" | "brand"; children: React.ReactNode }) {
  return (
    <span
      className={cn(
        "flex size-7 shrink-0 items-center justify-center rounded-md border",
        tone === "brand"
          ? "border-brand/25 bg-brand/10 text-brand-foreground dark:text-brand"
          : "border-border bg-surface text-muted-foreground",
      )}
    >
      {children}
    </span>
  );
}

/** Where the agents run: the platform's mark in the node's frame (Sectr
 *  Run's logo is its own framed tile). */
function RunsOnIcon({ kind }: { kind: RunsOnKind }) {
  if (kind === "sectr") return <LogoMark className="size-7 rounded-md" />;
  return (
    <NodeIcon>
      <RunsOnMark kind={kind} className="size-4" />
    </NodeIcon>
  );
}

function HealthText({ health }: { health: HealthLine | null }) {
  if (!health) {
    return <div className="h-4 w-28 animate-pulse rounded bg-muted" aria-label="Loading wakes" />;
  }
  return (
    <div className="space-y-1 text-[11px]">
      <div className="flex items-center gap-1.5">
        <span
          className={cn(
            "size-1.5 shrink-0 rounded-full",
            health.tone === "ok" && "bg-success",
            health.tone === "warn" && "bg-warning",
            health.tone === "muted" && "bg-faint-foreground/60",
          )}
        />
        <span
          className={cn(
            health.tone === "ok" && "text-success",
            health.tone === "warn" && "text-warning",
            health.tone === "muted" && "text-muted-foreground",
          )}
        >
          {health.text}
        </span>
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

function ColumnLabel({ className, children }: { className?: string; children: React.ReactNode }) {
  return (
    <div
      className={cn(
        "text-xs font-semibold uppercase tracking-wider text-muted-foreground @3xl:row-start-1",
        className,
      )}
    >
      {children}
    </div>
  );
}
