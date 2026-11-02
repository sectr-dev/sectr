import type { EventType } from "./types";
import {
  BotMessageSquare,
  Braces,
  CircleAlert,
  CircleCheck,
  CircleX,
  Cog,
  CornerDownRight,
  FileWarning,
  Hammer,
  MessageSquareQuote,
  MessageSquareText,
  Minimize2,
  Play,
  ShieldAlert,
  ShieldCheck,
  ShieldX,
  Square,
  type LucideIcon,
} from "lucide-react";

/**
 * Display metadata per journal event type. Colors map to semantic tokens;
 * chrome stays monochrome, color carries meaning.
 */
export interface EventMeta {
  label: string;
  /** tailwind text color class */
  color: string;
  /** dot background class */
  dot: string;
  icon: LucideIcon;
  /** group used for filters + transcript rendering */
  group: "lifecycle" | "message" | "tool" | "approval" | "system";
}

export const EVENT_META: Record<EventType, EventMeta> = {
  SESSION_CREATED: {
    label: "Created",
    color: "text-muted-foreground",
    dot: "bg-muted-foreground",
    icon: Play,
    group: "lifecycle",
  },
  INVOCATION_STARTED: {
    label: "Invocation",
    color: "text-info",
    dot: "bg-info",
    icon: Play,
    group: "lifecycle",
  },
  MESSAGE_START: {
    label: "Message start",
    color: "text-foreground",
    dot: "bg-foreground",
    icon: MessageSquareQuote,
    group: "message",
  },
  MESSAGE_DELTA: {
    label: "Message Δ",
    color: "text-foreground",
    dot: "bg-foreground",
    icon: Braces,
    group: "message",
  },
  MESSAGE_END: {
    label: "Message end",
    color: "text-foreground",
    dot: "bg-foreground",
    icon: Square,
    group: "message",
  },
  TOOL_CALL: {
    label: "Tool call",
    color: "text-warning",
    dot: "bg-warning",
    icon: Hammer,
    group: "tool",
  },
  TOOL_RESULT: {
    label: "Tool result",
    // the quiet counterpart of the amber call — gray, so it never reads
    // as another Invocation started (also blue)
    color: "text-muted-foreground",
    dot: "bg-muted-foreground",
    icon: CornerDownRight,
    group: "tool",
  },
  TOOL_APPROVAL_REQUIRED: {
    label: "Approval req.",
    color: "text-warning",
    dot: "bg-warning",
    icon: ShieldAlert,
    group: "approval",
  },
  TOOL_APPROVAL_GRANTED: {
    label: "Approved",
    color: "text-success",
    dot: "bg-success",
    icon: ShieldCheck,
    group: "approval",
  },
  TOOL_APPROVAL_DENIED: {
    label: "Denied",
    color: "text-danger",
    dot: "bg-danger",
    icon: ShieldX,
    group: "approval",
  },
  TRANSCRIPT_COMPACTED: {
    label: "Compacted",
    color: "text-info",
    dot: "bg-info",
    icon: Minimize2,
    group: "system",
  },
  ERROR: {
    label: "Error",
    color: "text-danger",
    dot: "bg-danger",
    icon: CircleAlert,
    group: "system",
  },
  SESSION_ERROR: {
    label: "Session error",
    color: "text-danger",
    dot: "bg-danger",
    icon: FileWarning,
    group: "system",
  },
  NO_MORE_ACTIONS: {
    label: "Turn done",
    color: "text-success",
    dot: "bg-success",
    icon: CircleCheck,
    group: "lifecycle",
  },
};

// Re-exported so imports stay centralized.
export { BotMessageSquare, Cog, CircleX, MessageSquareText };

export const SESSION_STATUS_META = {
  active: {
    label: "Active",
    variant: "success" as const,
    dot: "bg-success animate-pulse-dot",
  },
  awaiting_approval: {
    label: "Awaiting approval",
    variant: "warning" as const,
    dot: "bg-warning animate-pulse-dot",
  },
  ended: {
    label: "Ended",
    variant: "muted" as const,
    dot: "bg-muted-foreground",
  },
  error: {
    label: "Error",
    variant: "danger" as const,
    dot: "bg-danger",
  },
};

// Registry vocabulary (data-model.sql — source of truth): building ->
// active | failed; active -> retired (superseded or via the activate
// endpoint).
export const DEPLOYMENT_STATUS_META = {
  building: { label: "Building", variant: "warning" as const },
  active: { label: "Active", variant: "success" as const },
  failed: { label: "Failed", variant: "danger" as const },
  retired: { label: "Retired", variant: "muted" as const },
};

export const FRAMEWORK_LABEL = {
  "openai-agents": "OpenAI Agents",
  langgraph: "LangGraph",
  "pydantic-ai": "Pydantic AI",
  "claude-code": "Claude Code",
} as const;

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

export function formatTime(iso: string): string {
  return new Date(iso).toLocaleTimeString("en-US", {
    hour12: false,
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatTimeMs(iso: string): string {
  const d = new Date(iso);
  return (
    formatTime(iso) +
    "." +
    String(d.getMilliseconds()).padStart(3, "0")
  );
}

export function formatRelative(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const mins = Math.floor(diff / 60_000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

export function formatDuration(fromIso: string, toIso: string): string {
  const ms = new Date(toIso).getTime() - new Date(fromIso).getTime();
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s - m * 60)}s`;
}

export function shortId(id: string): string {
  return id.slice(0, 8);
}

/**
 * The text of a MESSAGE_DELTA's delta under the contract shape
 * ({type, text}; `reasoning` kind renders nothing, like the CLI renderer).
 * All transcript renderers pull text through this — never `String(p.delta)`.
 */
export function deltaText(payload: unknown): string {
  const delta = (payload as { delta?: { type?: string; text?: string } }).delta;
  if (typeof delta === "string") return delta; // legacy fixtures
  return delta?.type === "text" ? (delta.text ?? "") : "";
}
