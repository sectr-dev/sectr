import { CircleAlert, Hammer, ShieldAlert, ShieldX } from "lucide-react";
import { deltaText } from "@/lib/events";
import { cn } from "@/lib/utils";
import type { JournalEvent } from "@/lib/transport/types";
import type { InvocationStartedPayload } from "@/lib/types";

/**
 * MiniTranscript — compact, read-only projection of a session's recent
 * events, for inline previews (approvals inbox) where you want just enough
 * context to act without navigating away. Shares the transcript's visual
 * language (record voice for agent prose) but renders nothing interactive.
 */
export function MiniTranscript({
  events,
  limit = 4,
  className,
}: {
  events: JournalEvent[];
  limit?: number;
  className?: string;
}) {
  const nodes: React.ReactNode[] = [];
  const textByMessage = new Map<string, string>();

  for (const e of events) {
    const p = e.payload as Record<string, unknown>;
    switch (e.type) {
      case "INVOCATION_STARTED": {
        const reason = p.reason as InvocationStartedPayload["reason"];
        if (reason.kind === "user_message" && reason.input) {
          nodes.push(
            <div key={`u-${e.seq}`} className="flex justify-end">
              <div className="max-w-[85%] rounded-lg rounded-br-sm bg-surface-raised px-3 py-1.5 text-xs">
                {reason.input}
              </div>
            </div>,
          );
        }
        break;
      }
      case "MESSAGE_START":
        textByMessage.set(String(p.message_id), "");
        break;
      case "MESSAGE_DELTA": {
        const id = String(p.message_id);
        textByMessage.set(id, (textByMessage.get(id) ?? "") + deltaText(p));
        break;
      }
      case "MESSAGE_END": {
        const id = String(p.message_id);
        const text = textByMessage.get(id) ?? "";
        textByMessage.delete(id);
        if (!text.trim()) break;
        nodes.push(
          <div key={`m-${e.seq}`}>
            <div className="max-w-[85%] rounded-lg rounded-tl-sm border-l-2 border-l-brand/60 bg-surface px-3 py-1.5 text-[13px] leading-relaxed">
              {text}
            </div>
          </div>,
        );
        break;
      }
      case "TOOL_CALL":
        nodes.push(
          <div key={e.seq} className="flex items-center gap-2 pl-2 text-xs text-muted-foreground">
            <Hammer className="size-3 shrink-0 text-faint-foreground" />
            <span className="font-mono font-medium text-foreground">{String(p.name)}</span>
            <span className="min-w-0 truncate font-mono text-[11px] text-faint-foreground">
              {JSON.stringify(p.arguments)}
            </span>
          </div>,
        );
        break;
      case "TOOL_APPROVAL_REQUIRED":
        nodes.push(
          <div
            key={e.seq}
            className="flex items-center gap-2 rounded-md border border-warning/30 bg-warning/[0.07] px-2.5 py-1.5 text-xs text-warning"
          >
            <ShieldAlert className="size-3.5 shrink-0" />
            <span className="font-mono font-medium">{String(p.name)}</span>
            {p.reason ? <span className="min-w-0 truncate text-warning/80">{String(p.reason)}</span> : null}
          </div>,
        );
        break;
      case "TOOL_APPROVAL_DENIED":
        nodes.push(
          <div key={e.seq} className="flex items-center gap-1.5 text-[11px] text-danger">
            <ShieldX className="size-3" /> denied
          </div>,
        );
        break;
      case "ERROR":
      case "SESSION_ERROR":
        nodes.push(
          <div
            key={e.seq}
            className="flex items-center gap-2 rounded-md border border-danger/30 bg-danger/[0.07] px-2.5 py-1.5 text-xs text-danger"
          >
            <CircleAlert className="size-3.5 shrink-0" />
            <span className="font-mono">{String(p.message)}</span>
          </div>,
        );
        break;
      default:
        break;
    }
  }

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      {nodes.slice(-limit)}
    </div>
  );
}
