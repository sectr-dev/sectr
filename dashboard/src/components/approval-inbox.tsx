"use client";

import { ChevronDown, ExternalLink, ShieldCheck, ShieldX } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useMemo, useState } from "react";
import { toast } from "sonner";
import { JsonView } from "@/components/json-view";
import { SessionBreadcrumb } from "@/components/session-breadcrumb";
import { DOCS_URL } from "@/components/explore-links";
import { MiniTranscript } from "@/components/mini-transcript";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { formatRelative } from "@/lib/events";
import { useApprovals, useSessionEvents } from "@/lib/hooks";
import * as api from "@/lib/api";
import type { PendingApproval } from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * Approval inbox — cross-app list of sessions whose last journal event is
 * TOOL_APPROVAL_REQUIRED. Decisions POST .../approval (server enforces
 * idempotency; the client just avoids double-clicks).
 */
export function ApprovalInbox() {
  const router = useRouter();
  const [decided, setDecided] = useState<Record<string, boolean>>({});
  const [submitting, setSubmitting] = useState<string | null>(null);
  const pending = useApprovals();

  // Longest-waiting first; projects (once there's more than one) ordered by
  // their longest-waiting approval.
  const groups = useMemo(() => {
    const sorted = [...pending].sort(
      (a, b) => Date.parse(a.event.created_at) - Date.parse(b.event.created_at),
    );
    const byApp = new Map<string, { name: string; items: typeof sorted }>();
    for (const p of sorted) {
      const g = byApp.get(p.session.app_id) ?? { name: p.session.app_name, items: [] };
      g.items.push(p);
      byApp.set(p.session.app_id, g);
    }
    return [...byApp.values()];
  }, [pending]);

  if (pending.length === 0) {
    return (
      <Card className="flex flex-col items-center gap-1.5 px-6 py-12 text-center">
        <ShieldCheck className="size-6 text-success" />
        <p className="text-sm font-medium">You&apos;re all caught up</p>
        <p className="max-w-sm text-xs text-muted-foreground">
          When an agent calls a tool that needs approval, the session pauses
          here until you approve or deny it.
        </p>
        <a
          href={`${DOCS_URL}/guides/approvals/`}
          target="_blank"
          rel="noreferrer"
          className="mt-2 flex items-center gap-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          Gating tools behind approval <ExternalLink className="size-3" />
        </a>
      </Card>
    );
  }

  const decide = (sessionId: string, approved: boolean) => {
    if (submitting) return; // double-click safe
    setSubmitting(sessionId);
    api
      .decideApproval(sessionId, { approved })
      .then(() => {
        setDecided((d) => ({ ...d, [sessionId]: approved }));
        toast(approved ? "Approval granted" : "Approval denied", {
          description: `Session ${sessionId}: the agent continues from where it paused.`,
        });
      })
      .catch((e: unknown) => {
        toast.error(approved ? "Couldn't approve" : "Couldn't deny", {
          description: e instanceof Error ? e.message : "The request failed. Try again.",
        });
      })
      .finally(() => setSubmitting(null));
  };

  return (
    <div className="space-y-6">
      {groups.map((g) => (
        <section key={g.name} className="space-y-3">
          {groups.length > 1 && (
            <h2 className="flex items-center gap-2 text-sm font-medium">
              {g.name}
              <span className="text-xs font-normal text-muted-foreground">{g.items.length}</span>
            </h2>
          )}
          {g.items.map((p) => (
            <ApprovalCard
              key={p.session.id}
              approval={p}
              isDecided={decided[p.session.id] !== undefined}
              approved={decided[p.session.id]}
              submitting={submitting === p.session.id}
              onDecide={decide}
              onOpen={() => router.push(`/sessions/${p.session.id}`)}
            />
          ))}
        </section>
      ))}
    </div>
  );
}

/** One inbox row. Owns its inline transcript subscription (Inspect). */
function ApprovalCard({
  approval,
  isDecided,
  approved,
  submitting,
  onDecide,
  onOpen,
}: {
  approval: PendingApproval;
  isDecided: boolean;
  approved: boolean | undefined;
  submitting: boolean;
  onDecide: (sessionId: string, approved: boolean) => void;
  onOpen: () => void;
}) {
  const { session, event } = approval;
  // Inline context: expand the recent transcript right here instead of
  // navigating away, so the decision keeps the inbox in view.
  const [expanded, setExpanded] = useState(false);
  const events = useSessionEvents(session.id);
  const recent = events.slice(-4);

  return (
    <Card
      className={cn(
        "px-4 py-3.5 transition-opacity",
        isDecided && "opacity-60",
      )}
    >
      <div className="flex flex-wrap items-start gap-x-4 gap-y-2">
        <span className="flex size-8 shrink-0 items-center justify-center rounded-lg border border-brand/25 bg-brand/10 text-[13px] font-semibold text-brand-foreground dark:text-brand">
          {session.app_name[0].toUpperCase()}
        </span>
        <div className="min-w-0 flex-1">
          <button
            type="button"
            onClick={onOpen}
            className={cn(
              "block max-w-full truncate text-left text-sm font-medium underline-offset-4 hover:underline",
              !session.title && "text-muted-foreground",
            )}
          >
            {session.title ?? "Untitled session"}
          </button>
          <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <SessionBreadcrumb session={session} />
            <span className="shrink-0 whitespace-nowrap text-[11px] text-faint-foreground">
              paused {formatRelative(event.created_at)}
            </span>
          </div>
          <div className="mt-2.5 flex items-baseline gap-2 text-sm">
            <span className="font-mono font-medium text-warning">
              {event.payload.name}
            </span>
          </div>
          {event.payload.reason && (
            <div className="mt-0.5 text-xs text-muted-foreground">
              {event.payload.reason}
            </div>
          )}
          <JsonView
            data={event.payload.arguments}
            className="mt-2 max-h-64 overflow-auto rounded-md border border-border bg-background/50 px-3 py-2"
          />
        </div>

        {isDecided ? (
          <span
            className={cn(
              "flex items-center gap-1.5 text-sm font-medium",
              approved ? "text-success" : "text-danger",
            )}
          >
            {approved ? (
              <ShieldCheck className="size-4" />
            ) : (
              <ShieldX className="size-4" />
            )}
            {approved ? "Approved" : "Denied"}
          </span>
        ) : (
          <div className="flex items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setExpanded((x) => !x)}
              aria-expanded={expanded}
            >
              Inspect
              <ChevronDown
                className={cn(
                  "transition-transform",
                  expanded && "rotate-180",
                )}
              />
            </Button>
            <Button
              variant="destructive"
              size="sm"
              onClick={() => onDecide(session.id, false)}
              disabled={submitting}
            >
              <ShieldX /> Deny
            </Button>
            <Button
              variant="success"
              size="sm"
              onClick={() => onDecide(session.id, true)}
              disabled={submitting}
            >
              <ShieldCheck /> Approve
            </Button>
          </div>
        )}
      </div>

      {expanded && !isDecided && (
        <div className="mt-3 border-t border-border pt-3">
          <MiniTranscript events={recent} limit={4} />
          <div className="mt-3 flex items-center justify-between">
            <span className="text-[11px] text-faint-foreground">
              last {Math.min(4, events.length)} events · your decision lets the agent continue from where it paused
            </span>
            <Link
              href={`/sessions/${session.id}`}
              className="flex items-center gap-1 text-[11px] text-muted-foreground transition-colors hover:text-foreground"
            >
              Open full session <ExternalLink className="size-3" />
            </Link>
          </div>
        </div>
      )}
    </Card>
  );
}
