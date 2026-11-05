"use client";

import { MessagesSquare, Search } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { AppTabs } from "@/components/app-tabs";
import { CommandBlock } from "@/components/new-project-card";
import { SessionTable } from "@/components/session-table";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useSessionList } from "@/lib/hooks";
import type { App, Session } from "@/lib/types";
import { cn } from "@/lib/utils";
import { deploymentVersionLabel } from "@/lib/view";
import { publicServerUrl } from "@/lib/runtime-config";

type Filter = "all" | "live" | "awaiting_approval" | "ended" | "error";

const FILTERS: {
  key: Filter;
  label: string;
  match: (s: Session) => boolean;
}[] = [
  { key: "all", label: "All", match: () => true },
  {
    key: "live",
    label: "Live",
    match: (s) => s.status === "active" || s.status === "awaiting_approval",
  },
  {
    key: "awaiting_approval",
    label: "Awaiting approval",
    match: (s) => s.status === "awaiting_approval",
  },
  { key: "ended", label: "Ended", match: (s) => s.status === "ended" },
  { key: "error", label: "Errored", match: (s) => s.status === "error" },
];

/** App sessions tab body. CLIENT: session data is browser-fetched (see api.ts). */
export function AppSessionsView({ app }: { app: App }) {
  const { sessions, loading } = useSessionList(app.id);
  // Server count is authoritative; the loaded page is a lower bound (the
  // dev gateway reports 0).
  const total = Math.max(app.sessions_count, sessions.length);

  const [filter, setFilter] = useState<Filter>("all");
  const [owner, setOwner] = useState("");
  const hasOwners = sessions.some((s) => s.external_user_id);
  const needle = owner.trim().toLowerCase();
  const active = FILTERS.find((f) => f.key === filter)!;
  const visible = sessions.filter(
    (s) =>
      active.match(s) &&
      (!needle || s.external_user_id?.toLowerCase().includes(needle)),
  );

  return (
    <>
      <AppTabs appId={app.id} sessionsCount={total} active="sessions" />

      {!loading && sessions.length === 0 ? (
        <EmptySessions app={app} />
      ) : (
        <div className="px-6 pt-6">
          {/* Filters apply to the loaded sessions (the latest 50); the server
            can't page yet, so counts are of what's loaded. */}
          <div className="mb-3 flex flex-wrap items-center gap-x-4 gap-y-2">
            <div
              className="flex flex-wrap items-center gap-1.5"
              role="group"
              aria-label="Filter by status"
            >
              {FILTERS.map((f) => {
                const count = sessions.filter(f.match).length;
                const on = filter === f.key;
                return (
                  <button
                    key={f.key}
                    type="button"
                    aria-pressed={on}
                    onClick={() => setFilter(f.key)}
                    className={cn(
                      "flex items-center gap-1.5 rounded-full border px-3 py-1 text-xs font-medium transition-colors",
                      on
                        ? "border-brand/50 bg-brand/10 text-brand-foreground dark:text-brand"
                        : "border-border bg-surface text-muted-foreground hover:border-border-strong hover:text-foreground",
                    )}
                  >
                    {f.label}
                    <span
                      className={cn(
                        "tabular-nums",
                        on ? "opacity-80" : "text-faint-foreground",
                      )}
                    >
                      {count}
                    </span>
                  </button>
                );
              })}
            </div>
            {hasOwners && (
              <div className="relative ml-auto w-full max-w-64 sm:w-64">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-faint-foreground" />
                <Input
                  value={owner}
                  onChange={(e) => setOwner(e.target.value)}
                  placeholder="Search by owner"
                  aria-label="Search by owner (external user id)"
                  className="h-8 pl-8 text-xs"
                />
              </div>
            )}
          </div>
          <Card className="py-0">
            <SessionTable
              sessions={visible}
              showOwner={hasOwners}
              showDeployment
              emptyMessage={
                sessions.length === 0
                  ? "No sessions yet."
                  : "No sessions match."
              }
              deploymentLabel={(id) => {
                // Older sessions pin to earlier deployments; resolve from the
                // history. "—" only if the deployment isn't in it at all.
                const d = app.deployments.find((x) => x.id === id);
                return d ? deploymentVersionLabel(d) : "—";
              }}
            />
          </Card>
        </div>
      )}
    </>
  );
}

/** A project with no sessions yet: point at the two ways to make one. */
function EmptySessions({ app }: { app: App }) {
  const route = app.routes[0]?.path ?? "/chat";
  const curl =
    `curl -X POST ${publicServerUrl()}/sessions ` +
    `-H "Authorization: Bearer $SECTR_INVOCATION_KEY" ` +
    `-H "Content-Type: application/json" ` +
    `-d '{"app": "${app.slug}", "route": "${route}", "input": "hi"}'`;
  return (
    <div className="px-6 pt-6">
      <Card className="items-center px-6 py-10 text-center">
        <MessagesSquare className="size-6 text-faint-foreground" />
        <div>
          <p className="text-sm font-medium">No sessions yet</p>
          <p className="mt-1 text-xs text-muted-foreground">
            Sessions appear here as soon as someone talks to {app.name}.
          </p>
        </div>
        <Button asChild size="sm">
          <Link href={`/apps/${app.id}/playground`}>
            Try it in the Playground
          </Link>
        </Button>
        <div className="mt-2 w-full max-w-xl space-y-1.5 text-left">
          <p className="text-xs text-muted-foreground">
            Or start one from your backend:
          </p>
          <CommandBlock command={curl} />
        </div>
      </Card>
    </div>
  );
}
