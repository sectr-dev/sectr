"use client";

import { Check, ChevronRight, Copy } from "lucide-react";
import Link from "next/link";
import { useState } from "react";
import { toast } from "sonner";
import { shortId } from "@/lib/events";
import type { Session } from "@/lib/types";

/**
 * `project › agent › id` for a session: links to the project and its
 * sessions, and the short id is a chip that copies the full one. Shared by
 * the session page's meta bar and the approval cards.
 */
export function SessionBreadcrumb({
  session,
}: {
  session: Pick<Session, "id" | "app_id" | "app_name" | "route">;
}) {
  const [copied, setCopied] = useState(false);
  const copyId = () => {
    void navigator.clipboard.writeText(session.id);
    setCopied(true);
    toast("Session ID copied", { description: session.id });
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <nav aria-label="Session" className="flex min-w-0 items-center gap-1.5 text-sm">
      <Link
        href={`/apps/${session.app_id}`}
        className="truncate font-mono text-[13px] text-muted-foreground transition-colors hover:text-foreground"
      >
        {session.app_name}
      </Link>
      <ChevronRight className="size-3.5 shrink-0 text-faint-foreground" aria-hidden />
      <Link
        href={`/apps/${session.app_id}/sessions`}
        title={`All ${session.app_name} sessions`}
        className="truncate font-mono text-[13px] text-muted-foreground transition-colors hover:text-foreground"
      >
        {session.route}
      </Link>
      <ChevronRight className="size-3.5 shrink-0 text-faint-foreground" aria-hidden />
      <button
        type="button"
        onClick={copyId}
        aria-label={`Copy session ID ${session.id}`}
        title={`Copy session ID · ${session.id}`}
        className="group flex shrink-0 items-center gap-2 rounded-md border border-border bg-surface px-3 py-1 font-mono text-[13px] font-medium transition-colors hover:border-border-strong hover:bg-surface-raised"
      >
        {shortId(session.id)}
        {copied ? (
          <Check className="size-3.5 text-success" />
        ) : (
          <Copy className="size-3.5 text-faint-foreground transition-colors group-hover:text-foreground" />
        )}
      </button>
    </nav>
  );
}
