"use client";

import { Check, Copy } from "lucide-react";
import { useState } from "react";
import { JsonView } from "@/components/json-view";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { EVENT_META, formatTimeMs } from "@/lib/events";
import type { JournalEvent } from "@/lib/transport/types";
import { cn } from "@/lib/utils";

/**
 * EventPayloadInspector — detail panel for a selected journal event.
 * Shows envelope metadata + syntax-highlighted payload.
 */
export function EventPayloadInspector({
  event,
  className,
}: {
  event: JournalEvent | null;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  if (!event) {
    return (
      <div
        className={cn(
          "flex flex-col items-center justify-center gap-2 p-8 text-center",
          className,
        )}
      >
        <p className="text-sm text-muted-foreground">Select an event</p>
        <p className="max-w-48 text-xs text-faint-foreground">
          Click a row in the event log to inspect its journal payload.
        </p>
      </div>
    );
  }

  const meta = EVENT_META[event.type];

  const copy = () => {
    void navigator.clipboard.writeText(JSON.stringify(event, null, 2));
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className={cn("flex min-h-0 flex-col", className)}>
      <div className="flex items-center gap-2 border-b border-border px-4 py-2.5">
        <span className={cn("font-mono text-xs font-semibold", meta.color)}>
          {event.type}
        </span>
        <span className="text-[11px] text-faint-foreground">seq {event.seq}</span>
        <Button
          variant="ghost"
          size="icon-sm"
          className="ml-auto"
          onClick={copy}
          aria-label="Copy event JSON"
        >
          {copied ? <Check className="text-success" /> : <Copy />}
        </Button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto px-4 py-3">
        <table className="w-full text-xs">
          <tbody className="[&_th]:w-28 [&_th]:py-1 [&_th]:text-left [&_th]:align-top [&_th]:font-normal [&_th]:text-faint-foreground [&_td]:py-1 [&_td]:font-mono text-foreground">
            <tr>
              <th>id</th>
              <td className="break-all">{event.id}</td>
            </tr>
            <tr>
              <th>created_at</th>
              <td>{formatTimeMs(event.created_at)}</td>
            </tr>
            <tr>
              <th>deployment</th>
              <td className="break-all">{event.deployment_id}</td>
            </tr>
          </tbody>
        </table>
        <Separator className="my-3" />
        <div className="mb-1.5 text-[11px] font-medium text-faint-foreground uppercase tracking-wide">
          payload
        </div>
        <JsonView data={event.payload} />
      </div>
    </div>
  );
}
