"use client";

import Link from "next/link";
import { SessionStatusBadge } from "@/components/status-badges";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { formatRelative, shortId } from "@/lib/events";
import type { Session } from "@/lib/types";

/** Clickable session table of one project's sessions.
 *  `deploymentLabel` maps a session's deployment_id to something
 *  human ("dev", "v3") — a raw registry UUID is bookkeeping, not copy. */
export function SessionTable({
  sessions,
  showOwner = false,
  showDeployment = false,
  deploymentLabel,
  emptyMessage = "No sessions.",
}: {
  sessions: Session[];
  /** The Owner column ("show me what user X saw"); on when any
   *  session in the project has an owner. */
  showOwner?: boolean;
  showDeployment?: boolean;
  deploymentLabel?: (deploymentId: string) => string;
  emptyMessage?: string;
}) {
  const hasOwners = showOwner;
  const visible = sessions;
  return (
    <>
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead>Session</TableHead>
          <TableHead>Agent</TableHead>
          {hasOwners && <TableHead>Owner</TableHead>}
          {showDeployment && <TableHead>Deployment</TableHead>}
          <TableHead className="w-px">Status</TableHead>
          <TableHead className="text-right">Events</TableHead>
          <TableHead className="text-right">Started</TableHead>
          <TableHead className="text-right">Last event</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {visible.length === 0 && (
          <TableRow className="hover:bg-transparent">
            <TableCell colSpan={9} className="py-8 text-center text-sm text-muted-foreground">
              {emptyMessage}
            </TableCell>
          </TableRow>
        )}
        {visible.map((s) => {
          const href = `/sessions/${s.id}`;
          return (
            <TableRow key={s.id} className="hover:bg-brand/[0.05] focus-within:bg-brand/[0.05]">
              <LinkCell href={href} first title={s.id} className="max-w-64">
                {s.title ? (
                  <span className="flex items-baseline gap-2">
                    <span className="truncate font-medium">{s.title}</span>
                    <span className="shrink-0 font-mono text-[11px] text-faint-foreground">
                      {shortId(s.id)}
                    </span>
                  </span>
                ) : (
                  <span className="font-mono">{shortId(s.id)}</span>
                )}
              </LinkCell>
              <LinkCell href={href}>{s.route}</LinkCell>
              {hasOwners && (
                <LinkCell href={href} title={s.external_user_id} className="max-w-40 font-mono">
                  {s.external_user_id ?? "—"}
                </LinkCell>
              )}
              {showDeployment && (
                <LinkCell href={href} title={s.deployment_id}>
                  {deploymentLabel ? deploymentLabel(s.deployment_id) : s.deployment_id}
                </LinkCell>
              )}
              <LinkCell href={href}>
                <SessionStatusBadge status={s.status} />
              </LinkCell>
              <LinkCell href={href} className="text-right">
                {s.event_count}
              </LinkCell>
              <LinkCell href={href} className="text-right" title={new Date(s.created_at).toLocaleString()}>
                {formatRelative(s.created_at)}
              </LinkCell>
              <LinkCell href={href} className="text-right">
                {formatRelative(s.last_event_at)}
              </LinkCell>
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
    </>
  );
}

/** A cell whose content is a link filling the whole cell, so the row reads
 *  as one link: cmd/middle-click, keyboard and "open in new tab" all work.
 *  Only the first cell is a tab stop; the rest repeat the same target. */
function LinkCell({
  href,
  first = false,
  title,
  className,
  children,
}: {
  href: string;
  first?: boolean;
  title?: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <TableCell className={cn("p-0 text-[13px] leading-5 text-foreground", className)} title={title}>
      <Link
        href={href}
        tabIndex={first ? undefined : -1}
        className="block truncate p-2 outline-none"
      >
        {children}
      </Link>
    </TableCell>
  );
}
