"use client";

import { Check, ShieldAlert, ShieldCheck, ShieldX } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import type { EventEnvelope, ToolApprovalRequiredPayload } from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * ApprovalBanner — pending `TOOL_APPROVAL_REQUIRED` action surface.
 * `onDecide` is called with the decision; `decision` reflects the local
 * (optimistic, shell-only) state — server-side idempotency lands in Phase 7.
 */
export function ApprovalBanner({
  event,
  decision,
  onDecide,
  className,
}: {
  event: EventEnvelope<ToolApprovalRequiredPayload> | null;
  decision: "pending" | "approved" | "denied";
  onDecide?: (approved: boolean) => void;
  className?: string;
}) {
  const [submitting, setSubmitting] = useState<null | boolean>(null);

  if (!event) return null;
  const p = event.payload;

  if (decision !== "pending") {
    const approved = decision === "approved";
    return (
      <div
        className={cn(
          "flex items-center gap-2.5 border-b px-6 py-3 text-sm",
          approved
            ? "border-success/25 bg-success/[0.07] text-success"
            : "border-danger/25 bg-danger/[0.07] text-danger",
          className,
        )}
      >
        {approved ? <ShieldCheck className="size-4" /> : <ShieldX className="size-4" />}
        <span>
          <span className="font-mono font-semibold">{p.name}</span>{" "}
          {approved ? "approved" : "denied"} — resuming session with a fresh
          invocation…
        </span>
        <Check className="ml-auto size-4" />
      </div>
    );
  }

  const decide = (approved: boolean) => {
    if (submitting !== null) return; // double-click safe
    setSubmitting(approved);
    // Simulate a request round-trip so the interaction feels real.
    setTimeout(() => {
      setSubmitting(null);
      onDecide?.(approved);
    }, 450);
  };

  return (
    <div className={cn("border-b border-warning/30 bg-warning/[0.06] px-6 py-3", className)}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <div className="flex items-center gap-2 text-sm font-medium text-warning">
          <ShieldAlert className="size-4" />
          Approval required
        </div>
        <span className="font-mono text-xs text-foreground">{p.name}</span>
        <span className="truncate font-mono text-[11px] text-muted-foreground">
          {JSON.stringify(p.arguments)}
        </span>
        {p.reason && (
          <span className="text-xs text-muted-foreground">— {p.reason}</span>
        )}
        <div className="ml-auto flex items-center gap-2">
          <Button
            variant="destructive"
            size="sm"
            onClick={() => decide(false)}
            disabled={submitting !== null}
          >
            <ShieldX /> Deny
          </Button>
          <Button
            variant="success"
            size="sm"
            onClick={() => decide(true)}
            disabled={submitting !== null}
          >
            <ShieldCheck /> Approve
          </Button>
        </div>
      </div>
    </div>
  );
}
