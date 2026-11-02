"use client";

import { ArrowRight } from "lucide-react";
import Link from "next/link";
import { plural, StatChip } from "@/components/stat-chip";
import { formatRelative } from "@/lib/events";
import { useApprovals, useSessions } from "@/lib/hooks";
import type { App } from "@/lib/types";
import { deploymentVersionLabel } from "@/lib/view";

/**
 * The project header's stat chips, in the Projects page's style: live
 * sessions pulse, approvals awaiting this project turn amber and link to
 * the inbox. Counts are of the loaded sessions (the latest 50), except the
 * total, which is the project's own count.
 */
export function ProjectStatChips({ app }: { app: App }) {
  const sessions = useSessions(app.id);
  const pending = useApprovals().filter((p) => p.session.app_id === app.id);
  const live = sessions.filter(
    (s) => s.status === "active" || s.status === "awaiting_approval",
  ).length;
  const total = Math.max(app.sessions_count, sessions.length);
  const d = app.latest_deployment;

  return (
    <div className="mt-3 flex flex-wrap items-center gap-2">
      <StatChip tone={live > 0 ? "live" : "muted"}>
        {live > 0 && <span className="size-1.5 rounded-full bg-success animate-pulse-dot" />}
        {plural(live, "live session")}
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
      <StatChip>{plural(total, "session")}</StatChip>
      <StatChip tone="muted">
        {deploymentVersionLabel(d)}
        {d.created_at && <> · deployed {formatRelative(d.created_at)}</>}
      </StatChip>
    </div>
  );
}
