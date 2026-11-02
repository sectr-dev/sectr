"use client";

import { ArrowLeftRight } from "lucide-react";

import { PROD_AVAILABLE, swapToDev, swapToProd } from "@/lib/gateway-swap";
import { useBackend, type RememberedState } from "@/lib/hooks";
import { MOCK_EMPTY, toggleMockEmpty } from "@/lib/mock-scenario";
import { cn } from "@/lib/utils";

/**
 * Topbar pill: what backend is this console attached to? (Prominent by
 * design — the deployed console looks identical to the dev one, so the
 * badge is the only place the difference is VISIBLE.) Swapping dev <-> prod
 * is a URL-level action PERFORMED by the badge (`lib/gateway-swap.ts`:
 * localStorage pin move + full reload), not a runtime transport mutation —
 * api.ts resolves the backend once at import.
 *
 *   dev gateway ({project}:{port})  (amber — `sectr dev` attached; swaps to prod)
 *   gateway offline                 (red — pinned port not answering; swaps to prod)
 *   prod server                     (blue — /healthz answering)
 *   server offline                  (red — the prod server is down)
 *   demo data [· empty]             (gray — mock fixtures, no backend;
 *                                    swaps full ⇄ empty demo account)
 *
 * The prod pills are buttons (swap back to dev) only when a dev gateway was
 * remembered AND its bounded burst probe answered; a console that never
 * attached to a dev gateway gets plain status pills.
 */
export function BackendBadge() {
  const backend = useBackend();

  // Pre-probe skeleton: same footprint as a pill, no text to mismatch.
  if (backend === null) {
    return (
      <BadgePill tone="muted">
        <span className="w-10" aria-hidden />
      </BadgePill>
    );
  }

  if (backend.kind === "mock") {
    return (
      <BadgePill
        tone="muted"
        title={
          MOCK_EMPTY
            ? "Demo data, empty account: no projects, sessions, API keys or secrets — click for the full demo"
            : "No backend configured — pages render from demo fixtures. Click to switch to an empty account"
        }
        onSwap={toggleMockEmpty}
      >
        {MOCK_EMPTY ? "demo data · empty" : "demo data"}
      </BadgePill>
    );
  }
  if (backend.kind === "prod" || backend.kind === "prod-offline") {
    const base =
      backend.kind === "prod"
        ? "sectr-server is answering through the same-origin /api rewrite"
        : "sectr-server is not answering (checked via /healthz every 5s)";
    const swapBack = backend.remembered.state === "live";
    return (
      <BadgePill
        tone={backend.kind === "prod" ? "prod" : "offline"}
        title={`${base}${rememberedNote(backend.remembered)}`}
        onSwap={swapBack ? swapToDev : undefined}
      >
        {backend.kind === "prod" ? "prod server" : "server offline"}
      </BadgePill>
    );
  }
  if (backend.kind === "local-gateway-offline") {
    return (
      <BadgePill
        tone="offline"
        title={`pinned gateway ${backend.baseUrl} is not answering — ${
          PROD_AVAILABLE
            ? "click to switch to prod (the gateway is remembered), or restart `sectr dev`"
            : "restart `sectr dev`, or clear the pin with ?gateway="
        }`}
        onSwap={PROD_AVAILABLE ? () => swapToProd() : undefined}
      >
        gateway offline
      </BadgePill>
    );
  }
  const { status } = backend;
  return (
    <BadgePill
      tone="dev"
      title={
        PROD_AVAILABLE
          ? `dev gateway (${status.project}:${status.gateway_port}) — click to switch to prod; the gateway is remembered so you can switch back`
          : undefined // project/port are already visible
      }
      onSwap={PROD_AVAILABLE ? () => swapToProd(status.project) : undefined}
    >
      <span className="truncate">dev gateway ({status.project}</span>
      <span className="font-mono">:{status.gateway_port})</span>
    </BadgePill>
  );
}

function rememberedNote(remembered: RememberedState): string {
  if (remembered.state === "live") {
    const project = remembered.project ? ` (project ${remembered.project})` : "";
    return ` — click to switch back to the remembered dev gateway${project}`;
  }
  if (remembered.state === "stale") {
    const project = remembered.project ? ` (project ${remembered.project})` : "";
    return ` — remembered gateway${project} isn't running — start \`sectr dev\` and reopen its link to re-attach`;
  }
  return "";
}

function BadgePill({
  tone,
  title,
  onSwap,
  children,
}: {
  tone: "dev" | "offline" | "prod" | "muted";
  title?: string;
  /** Present → the pill is a button that performs the backend swap. */
  onSwap?: () => void;
  children: React.ReactNode;
}) {
  const dot = {
    dev: "bg-warning animate-pulse-dot",
    offline: "bg-danger",
    prod: "bg-info",
    muted: "bg-muted-foreground/40",
  }[tone];
  const text = {
    dev: "text-warning",
    offline: "text-danger",
    prod: "text-info",
    muted: "text-muted-foreground",
  }[tone];
  const interactive = {
    dev: "hover:border-warning/50 hover:bg-warning/10 focus-visible:border-warning/60 focus-visible:ring-warning/30",
    offline: "hover:border-danger/50 hover:bg-danger/10 focus-visible:border-danger/60 focus-visible:ring-danger/30",
    prod: "hover:border-info/50 hover:bg-info/10 focus-visible:border-info/60 focus-visible:ring-info/30",
    muted: "hover:bg-muted hover:text-foreground focus-visible:ring-ring/30",
  }[tone];
  const base = cn(
    "flex h-6 max-w-56 shrink-0 items-center gap-1.5 rounded-full border border-border bg-surface px-2.5 text-[11px] font-medium",
    text,
  );
  const inner = (
    <>
      <span className={cn("size-1.5 shrink-0 rounded-full", dot)} />
      {children}
    </>
  );
  if (!onSwap) {
    return (
      <span title={title} className={base}>
        {inner}
      </span>
    );
  }
  return (
    <button
      type="button"
      title={title}
      onClick={onSwap}
      className={cn(
        base,
        "group/swap cursor-pointer outline-none transition-colors focus-visible:ring-3",
        interactive,
      )}
    >
      {inner}
      <ArrowLeftRight
        className="size-3 shrink-0 opacity-50 transition-opacity group-hover/swap:opacity-100 group-focus-visible/swap:opacity-100"
        aria-hidden
      />
    </button>
  );
}
