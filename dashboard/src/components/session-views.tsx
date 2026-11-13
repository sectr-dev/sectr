"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  CircleAlert,
  CircleX,
  Hammer,
  Minimize2,
  ShieldAlert,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { isWakeFailure, wakeHint } from "@/lib/connections";
import { EVENT_META, formatTimeMs, shortId, deltaText } from "@/lib/events";
import type { EventMeta } from "@/lib/events";
import { toolCallStatuses, type ToolCallStatus } from "@/lib/tool-calls";
import type { JournalEvent } from "@/lib/transport/types";
import { failedTurnEnds } from "@/lib/turns";
import type { InvocationStartedPayload } from "@/lib/types";
import { cn } from "@/lib/utils";

/*
  Session projections — one journal, two views (see session-inspector.tsx):

  - ChatStream: the conversation. Transcript projection with clickable
    nodes — clicking anything selects the underlying journal event (the
    rail opens its detail view). Agent prose is serif record voice; machine
    events (tools, errors) are compact chips.
  - JournalLog: the event log — per-group filters, MESSAGE_DELTA
    coalescing (Grouped/Raw, toggled in the rail header), sticky header, tail status. Lives in the
    right-hand rail.
*/

// ---------------------------------------------------------------------------
// Shared summaries
// ---------------------------------------------------------------------------

function excerpt(s: string, n = 90): string {
  return s.length > n ? s.slice(0, n) + "…" : s;
}

function stringify(v: unknown): string {
  if (typeof v === "string") return v;
  return JSON.stringify(v);
}

function summarize(e: JournalEvent): { main: string; sub?: string } {
  const p = e.payload as Record<string, unknown>;
  switch (e.type) {
    case "SESSION_CREATED":
      return { main: String(p.route ?? "") };
    case "INVOCATION_STARTED": {
      const reason = p.reason as InvocationStartedPayload["reason"];
      if (reason.kind === "user_message" && reason.input) {
        return { main: `user_message · “${excerpt(reason.input, 110)}”` };
      }
      if (reason.kind === "approval_decision") {
        return {
          main: `approval_decision · ${reason.decision?.approved ? "approved" : "denied"}`,
        };
      }
      return { main: reason.kind };
    }
    case "MESSAGE_START":
      return { main: `role=${String(p.role)} · id=${String(p.message_id)}` };
    case "MESSAGE_DELTA": {
      const { kind, text } = deltaKindAndText(p);
      return { main: excerpt(kind === "reasoning" ? `reasoning · ${text}` : text) };
    }
    case "MESSAGE_END":
      return { main: `id=${String(p.message_id)}` };
    case "TOOL_CALL":
      return { main: String(p.name), sub: excerpt(JSON.stringify(p.arguments)) };
    case "TOOL_RESULT":
      return { main: excerpt(stringify(p.result), 140) };
    case "TOOL_APPROVAL_REQUIRED":
      return { main: String(p.name), sub: excerpt(String(p.reason ?? "")) };
    case "TOOL_APPROVAL_GRANTED":
    case "TOOL_APPROVAL_DENIED":
      return { main: `${String(p.tool_call_id)}${p.approver ? ` · ${String(p.approver)}` : ""}` };
    case "TRANSCRIPT_COMPACTED":
      return { main: `replaced seq ≤ ${String(p.replaces_up_to_seq)} with ${Array.isArray(p.messages) ? p.messages.length : "?"} messages` };
    case "ERROR":
      return { main: String(p.message), sub: p.code ? `code=${String(p.code)}${p.transient ? " · transient" : ""}` : undefined };
    case "SESSION_ERROR":
      return { main: String(p.message) };
    case "NO_MORE_ACTIONS":
      return { main: "turn complete — no pending actions" };
  }
}

// ---------------------------------------------------------------------------
// ChatStream — the conversation (transcript projection, clickable nodes)
// ---------------------------------------------------------------------------

type ChatItem =
  | { kind: "divider"; key: string; icon?: React.ReactNode; text: string; failed?: boolean }
  | { kind: "user"; key: string; event: JournalEvent; text: string }
  | { kind: "agent"; key: string; event: JournalEvent; text: string }
  | {
      kind: "tool";
      key: string;
      event: JournalEvent;
      name: string;
      args?: unknown;
      result?: unknown;
      toolCallId: string;
      status: ToolCallStatus;
    }
  | {
      kind: "approval";
      key: string;
      event: JournalEvent;
      name: string;
      reason?: string;
      toolCallId: string;
      status: "awaiting" | "granted" | "denied";
      approver?: string;
    }
  | { kind: "error"; key: string; event: JournalEvent; message: string; code?: string };

function buildChatItems(events: JournalEvent[]): ChatItem[] {
  const items: ChatItem[] = [];
  const failedEnds = failedTurnEnds(events);
  // Messages still open (no MESSAGE_END yet) render as in-flight partial
  // text — that's what makes the chat appear to stream with the journal.
  const textByMessage = new Map<
    string,
    { text: string; lastEvent: JournalEvent }
  >();
  let turn = 0;
  const toolStatus = toolCallStatuses(events);

  for (const e of events) {
    const p = e.payload as Record<string, unknown>;
    switch (e.type) {
      case "INVOCATION_STARTED": {
        const reason = p.reason as InvocationStartedPayload["reason"];
        if (reason.kind === "user_message") {
          turn += 1;
          items.push({
            kind: "divider",
            key: `t-${e.seq}`,
            text: `turn ${turn}`,
          });
          if (reason.input) {
            items.push({
              kind: "user",
              key: `u-${e.seq}`,
              event: e,
              text: reason.input,
            });
          }
        } else if (reason.kind === "approval_decision") {
          items.push({
            kind: "divider",
            key: `r-${e.seq}`,
            icon: <Zap className="size-3" />,
            text: "resumed on approval decision",
          });
        }
        break;
      }
      case "MESSAGE_START":
        textByMessage.set(String(p.message_id), { text: "", lastEvent: e });
        break;
      case "MESSAGE_DELTA": {
        const id = String(p.message_id);
        const entry = textByMessage.get(id);
        if (entry) {
          entry.text += deltaText(p);
          entry.lastEvent = e;
        }
        break;
      }
      case "MESSAGE_END": {
        const id = String(p.message_id);
        const entry = textByMessage.get(id);
        // A message with no text (a tool-call-only response, or reasoning
        // only) draws no bubble; its events still show in the journal rail.
        if (entry?.text.trim()) {
          items.push({
            kind: "agent",
            key: `m-${e.seq}`,
            event: e,
            text: entry.text,
          });
        }
        textByMessage.delete(id);
        break;
      }
      case "TOOL_CALL":
        items.push({
          kind: "tool",
          key: `tc-${e.seq}`,
          event: e,
          name: String(p.name),
          args: p.arguments,
          toolCallId: String(p.tool_call_id),
          status: toolStatus.get(e.seq) ?? "running",
        });
        break;
      case "TOOL_RESULT": {
        const item = [...items]
          .reverse()
          .find((i) => i.kind === "tool" && i.toolCallId === String(p.tool_call_id));
        if (item && item.kind === "tool") {
          item.result = p.result;
        }
        break;
      }
      case "TOOL_APPROVAL_REQUIRED":
        items.push({
          kind: "approval",
          key: `ta-${e.seq}`,
          event: e,
          name: String(p.name),
          reason: p.reason ? String(p.reason) : undefined,
          toolCallId: String(p.tool_call_id),
          status: "awaiting",
        });
        break;
      case "TOOL_APPROVAL_GRANTED":
      case "TOOL_APPROVAL_DENIED": {
        const item = [...items]
          .reverse()
          .find((i) => i.kind === "approval" && i.toolCallId === String(p.tool_call_id));
        if (item && item.kind === "approval") {
          item.status = e.type === "TOOL_APPROVAL_GRANTED" ? "granted" : "denied";
          item.approver = p.approver ? String(p.approver) : undefined;
        }
        break;
      }
      case "TRANSCRIPT_COMPACTED":
        items.push({
          kind: "divider",
          key: `c-${e.seq}`,
          icon: <Minimize2 className="size-3" />,
          text: `compacted — seq ≤ ${String(p.replaces_up_to_seq)} replaced`,
        });
        break;
      case "ERROR":
      case "SESSION_ERROR":
        items.push({
          kind: "error",
          key: `e-${e.seq}`,
          event: e,
          message: String(p.message),
          code: p.code ? String(p.code) : undefined,
        });
        break;
      case "NO_MORE_ACTIONS":
        items.push(
          failedEnds.has(e.seq)
            ? { kind: "divider", key: `n-${e.seq}`, text: "turn failed", failed: true }
            : { kind: "divider", key: `n-${e.seq}`, text: "turn complete" },
        );
        break;
      default:
        break;
    }
  }
  // Still-open messages (deltas arrived, no END yet) render as partial
  // text, keyed off their last delta so selection tracks the tail.
  for (const [id, entry] of textByMessage) {
    if (!entry.text.trim()) continue;
    items.push({
      kind: "agent",
      key: `mp-${id}`,
      event: entry.lastEvent,
      text: entry.text,
    });
  }
  return items;
}

const TOOL_PILLS: Record<ToolCallStatus, { label: string; cls: string; title?: string }> = {
  running: { label: "running", cls: "border-warning/30 bg-warning/10 text-warning" },
  done: { label: "done", cls: "border-success/30 bg-success/10 text-success" },
  interrupted: {
    label: "interrupted",
    cls: "border-danger/30 bg-danger/10 text-danger",
    title: "The turn ended before this tool call finished, so no result was recorded.",
  },
  not_run: {
    label: "not run",
    cls: "border-border bg-muted text-muted-foreground",
    title: "Its approval was denied or never decided, so it didn't run.",
  },
};

function ChatNode({
  item,
  selected,
  onSelect,
  entered,
}: {
  item: ChatItem;
  selected: boolean;
  onSelect?: (e: JournalEvent) => void;
  entered?: boolean;
}) {
  const anim = entered ? "animate-event-enter" : undefined;

  if (item.kind === "divider") {
    return (
      <div className={cn("flex items-center gap-3", anim)}>
        <span className="h-px flex-1 bg-border" />
        <span
          className={cn(
            "flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wider",
            item.failed ? "text-danger" : "text-faint-foreground",
          )}
        >
          {item.icon}
          {item.text}
        </span>
        <span className="h-px flex-1 bg-border" />
      </div>
    );
  }

  const clickable = !!onSelect;
  const selectProps = clickable
    ? {
        type: "button" as const,
        onClick: () => onSelect?.(item.event),
        title: `Inspect in journal · seq ${item.event.seq}`,
      }
    : {};

  const wrapperCls = cn(
    clickable && "group cursor-pointer text-left",
    selected && "ring-1 ring-brand/40 rounded-lg",
  );

  const seqBadge = (
    <span className="ml-auto shrink-0 self-start pt-0.5 font-mono text-[10px] text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100">
      seq {item.event.seq}
    </span>
  );

  switch (item.kind) {
    case "user":
      return (
        <div className={cn("flex justify-end", anim)}>
          <button {...selectProps} className={cn(wrapperCls, "max-w-[80%] rounded-lg rounded-br-sm bg-surface-raised px-3.5 py-2 text-sm")}>
            {item.text}
          </button>
        </div>
      );
    case "agent":
      return (
        <div className={cn("flex", anim)}>
          {/* The agent's words are the record — serif record voice */}
          <button
            {...selectProps}
            className={cn(
              wrapperCls,
              "flex min-w-0 max-w-[85%] items-start gap-2 rounded-lg rounded-tl-sm border-l-2 border-l-brand/60 bg-surface px-4 py-2.5 text-left text-[15px] leading-relaxed",
            )}
          >
            <span className="min-w-0">{item.text}</span>
            {seqBadge}
          </button>
        </div>
      );
    case "tool": {
      const pill = TOOL_PILLS[item.status];
      return (
        <div className={cn("pl-8", anim)}>
          <button
            {...selectProps}
            className={cn(
              wrapperCls,
              "flex w-full items-center gap-2 rounded-md border border-border bg-background/60 px-3 py-1.5 text-xs",
            )}
          >
            <Hammer className="size-3 shrink-0 text-faint-foreground" />
            <span className="font-mono font-medium">{item.name}</span>
            {item.args != null && Object.keys(item.args as object).length > 0 && (
              <span className="min-w-0 truncate font-mono text-[11px] text-faint-foreground">
                {excerpt(JSON.stringify(item.args), 60)}
              </span>
            )}
            <span className="ml-auto flex shrink-0 items-center gap-2">
              <span className="font-mono text-[10px] text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100">
                seq {item.event.seq}
              </span>
              <span
                className={cn("rounded-full border px-1.5 py-px text-[10px]", pill.cls)}
                title={pill.title}
              >
                {pill.label}
              </span>
            </span>
          </button>
          {item.status === "done" && (
            <div className="mt-1 pl-4 font-mono text-[11px] text-muted-foreground">
              <span className="text-faint-foreground">→ </span>
              {excerpt(stringify(item.result), 160)}
            </div>
          )}
        </div>
      );
    }
    case "approval": {
      const pill = {
        awaiting: { label: "awaiting approval", cls: "border-warning/30 bg-warning/10 text-warning" },
        granted: { label: item.approver ? `approved by ${item.approver}` : "approved", cls: "border-success/30 bg-success/10 text-success" },
        denied: { label: "denied", cls: "border-danger/30 bg-danger/10 text-danger" },
      }[item.status];
      return (
        <div className={cn("pl-8", anim)}>
          <button
            {...selectProps}
            className={cn(
              wrapperCls,
              "flex w-full items-center gap-2 rounded-lg border border-warning/30 bg-warning/[0.07] px-3 py-2 text-xs",
            )}
          >
            <ShieldAlert className="size-3.5 shrink-0 text-warning" />
            <span className="font-mono font-medium text-warning">{item.name}</span>
            {item.reason && (
              <span className="min-w-0 truncate text-warning/80">{item.reason}</span>
            )}
            <span className="ml-auto flex shrink-0 items-center gap-2">
              <span className="font-mono text-[10px] text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100">
                seq {item.event.seq}
              </span>
              <span className={cn("rounded-full border px-1.5 py-px text-[10px]", pill.cls)}>
                {pill.label}
              </span>
            </span>
          </button>
        </div>
      );
    }
    case "error": {
      if (isWakeFailure(item.message)) {
        const hint = wakeHint(item.message);
        return (
          <div className={cn("pl-8", anim)}>
            <button
              {...selectProps}
              className={cn(
                wrapperCls,
                "block w-full rounded-lg border border-danger/30 bg-danger/[0.07] px-3 py-2 text-left text-xs",
              )}
            >
              <span className="flex items-center gap-2 font-medium text-danger">
                <CircleAlert className="size-3.5 shrink-0" />
                Your compute endpoint didn&apos;t start this turn.
                <span className="ml-auto shrink-0 font-mono text-[10px] font-normal text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100">
                  seq {item.event.seq}
                </span>
              </span>
              <span className="mt-1 block break-all font-mono text-muted-foreground">{item.message}</span>
              {hint && <span className="mt-1 block italic text-foreground/80">{hint}</span>}
            </button>
          </div>
        );
      }
      return (
        <div className={cn("pl-8", anim)}>
          <button
            {...selectProps}
            className={cn(
              wrapperCls,
              "flex w-full items-center gap-2 rounded-lg border border-danger/30 bg-danger/[0.07] px-3 py-2 text-xs text-danger",
            )}
          >
            <CircleAlert className="size-3.5 shrink-0" />
            <span className="font-mono">{item.message}</span>
            {item.code && <span className="text-faint-foreground">{item.code}</span>}
            <span className="ml-auto shrink-0 font-mono text-[10px] text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100">
              seq {item.event.seq}
            </span>
          </button>
        </div>
      );
    }
  }
}

export function ChatStream({
  events,
  selected,
  onSelect,
  live,
  entered,
  className,
}: {
  events: JournalEvent[];
  selected: JournalEvent | null;
  onSelect?: (e: JournalEvent) => void;
  live?: boolean;
  entered?: boolean;
  className?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const items = useMemo(() => buildChatItems(events), [events]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [events.length]);

  return (
    <div ref={scrollRef} className={cn("min-h-0 flex-1 overflow-auto", className)}>
      <div className="flex flex-col gap-3 px-4 py-4">
        {items.map((item) => (
          <ChatNode
            key={item.key}
            item={item}
            selected={"event" in item && selected?.seq === item.event.seq}
            onSelect={onSelect}
            entered={entered}
          />
        ))}
        {items.length === 0 && (
          <div className="px-4 py-10 text-center text-xs text-faint-foreground">
            No conversation yet.
          </div>
        )}
        {/* streaming status — the only chrome the chat pane carries */}
        <div className="flex h-5 items-center gap-2 px-2 text-[11px]">
          {live ? (
            <>
              <span className="size-1.5 rounded-full bg-success animate-pulse-dot" />
              <span className="text-success">agent is responding…</span>
              {/* CSS-drawn caret — see note in mock-data.ts: an animated
                  U+2590 glyph in Geist Mono crashes headless chromium. */}
              <span className="animate-caret-blink inline-block h-3 w-[2px] bg-current" />
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// JournalLog — the event log (rail projection)
// ---------------------------------------------------------------------------

type EventGroup = EventMeta["group"];

const FILTERS: { key: EventGroup; label: string }[] = [
  { key: "lifecycle", label: "Lifecycle" },
  { key: "message", label: "Messages" },
  { key: "tool", label: "Tools" },
  { key: "approval", label: "Approvals" },
  { key: "system", label: "System" },
];

type Row =
  | { kind: "turn"; n: number }
  | { kind: "event"; event: JournalEvent }
  | {
      kind: "messageGroup";
      start: JournalEvent;
      deltas: JournalEvent[];
      end?: JournalEvent;
      reasoning?: boolean;
    };

/** Kind + text of a MESSAGE_DELTA payload (contract shape: {type, text}). */
function deltaKindAndText(payload: unknown): { kind: "text" | "reasoning"; text: string } {
  const delta = (payload as { delta?: { type?: string; text?: string } }).delta;
  if (typeof delta === "string") return { kind: "text", text: delta }; // legacy fixtures
  if (delta?.type === "reasoning") return { kind: "reasoning", text: delta.text ?? "" };
  return { kind: "text", text: delta?.text ?? "" };
}

function groupDeltas(events: JournalEvent[]): Row[] {
  const rows: Row[] = [];
  let start: JournalEvent | null = null; // MESSAGE_START of the open message
  let deltas: JournalEvent[] = [];
  let openId: string | null = null;
  let startEmitted = false; // the MESSAGE_START row renders once, ever
  let turn = 0;

  // Emit the accumulated deltas as segment rows. A message's delta stream
  // can be interrupted by tool calls (the SDK runs non-gated tools
  // mid-message), so a flush here does NOT close the message — the next
  // delta with the same message_id continues it as a new segment.
  const flush = () => {
    if (start && deltas.length > 0) {
      let i = 0;
      while (i < deltas.length) {
        const kind = deltaKindAndText(deltas[i].payload).kind;
        let j = i;
        while (j < deltas.length && deltaKindAndText(deltas[j].payload).kind === kind) j++;
        rows.push({
          kind: "messageGroup",
          start,
          deltas: deltas.slice(i, j),
          reasoning: kind === "reasoning",
        });
        i = j;
      }
      // the segment(s) represent this start — a bare start row would both
      // duplicate and misorder
      startEmitted = true;
    } else if (start && !startEmitted) {
      // A start with no deltas yet (or after a flush) is only emitted
      // once per message — resume flushes must not re-push it.
      rows.push({ kind: "event", event: start });
      startEmitted = true;
    }
    deltas = [];
  };

  for (const e of events) {
    const p = e.payload as Record<string, unknown>;
    if (e.type === "INVOCATION_STARTED") {
      flush();
      start = null;
      openId = null;
      rows.push({ kind: "turn", n: ++turn });
      // the divider is decoration; the event itself still belongs in the log
      rows.push({ kind: "event", event: e });
    } else if (e.type === "MESSAGE_START") {
      flush();
      start = null;
      start = e;
      startEmitted = false;
      openId = String(p.message_id);
    } else if (e.type === "MESSAGE_DELTA") {
      const id = String((p as { message_id?: unknown }).message_id);
      if (start && id === openId) {
        deltas.push(e);
      } else {
        // delta for a message we never saw start (filtered?) — raw row
        flush();
        start = null;
        openId = null;
        rows.push({ kind: "event", event: e });
      }
    } else if (e.type === "MESSAGE_END" && start && String(p.message_id) === openId) {
      flush();
      const last = rows[rows.length - 1];
      if (last.kind === "messageGroup") last.end = e;
      else if (!startEmitted) rows.push({ kind: "event", event: start });
      start = null;
      openId = null;
    } else {
      flush();
      rows.push({ kind: "event", event: e });
    }
  }
  flush();
  return rows;
}

/** A `NO_MORE_ACTIONS` row that ends a failed turn. */
const FAILED_TURN_END = {
  labelOverride: "Turn failed",
  colorOverride: "text-danger",
  iconOverride: CircleX,
  summaryOverride: { main: "turn ended by an error" },
};

function LogRow({
  event,
  labelOverride,
  colorOverride,
  iconOverride,
  summaryOverride,
  selected,
  onSelect,
  entered,
}: {
  event: JournalEvent;
  labelOverride?: string;
  colorOverride?: string;
  iconOverride?: LucideIcon;
  summaryOverride?: { main: string; sub?: string };
  selected: boolean;
  onSelect?: (e: JournalEvent) => void;
  entered?: boolean;
}) {
  const meta = EVENT_META[event.type];
  const s = summaryOverride ?? summarize(event);
  const Icon = iconOverride ?? meta.icon;
  return (
    <button
      type="button"
      onClick={() => onSelect?.(event)}
      className={cn(
        "flex w-full items-center gap-2 px-3 py-1.5 text-left transition-colors",
        onSelect && "hover:bg-surface-raised/70",
        selected && "bg-brand/[0.07] hover:bg-brand/[0.09]",
        entered && "animate-event-enter",
      )}
    >
      <span className="w-7 shrink-0 text-right font-mono text-[11px] text-faint-foreground">
        {event.seq}
      </span>
      <span className="flex w-[112px] shrink-0 items-center gap-1.5">
        <Icon className={cn("size-3.5 shrink-0", colorOverride ?? meta.color)} />
        <span className={cn("truncate text-xs font-medium", colorOverride ?? meta.color)}>
          {labelOverride ?? meta.label}
        </span>
      </span>
      <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
        {s.main}
        {s.sub && (
          <span className="ml-2 font-mono text-[11px] text-faint-foreground">{s.sub}</span>
        )}
      </span>
      <span className="shrink-0 font-mono text-[10px] text-faint-foreground">
        {formatTimeMs(event.created_at)}
      </span>
    </button>
  );
}

export function JournalLog({
  events,
  raw,
  selected,
  onSelect,
  live,
  entered,
  className,
}: {
  events: JournalEvent[];
  /** Show every event as-is instead of coalescing MESSAGE_DELTAs. */
  raw: boolean;
  selected: JournalEvent | null;
  onSelect?: (e: JournalEvent) => void;
  live?: boolean;
  entered?: boolean;
  className?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [activeFilters, setActiveFilters] = useState<Set<EventGroup>>(
    () => new Set(FILTERS.map((f) => f.key)),
  );

  // From every event, so a filter that hides the error still shows the
  // turn as failed.
  const failedEnds = useMemo(() => failedTurnEnds(events), [events]);
  const rows = useMemo(() => {
    const filtered = events.filter((e) => activeFilters.has(EVENT_META[e.type].group));
    return raw ? filtered.map((e) => ({ kind: "event" as const, event: e })) : groupDeltas(filtered);
  }, [events, activeFilters, raw]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [rows.length]);

  const toggleFilter = (key: EventGroup) => {
    setActiveFilters((prev) => {
      const next = new Set(prev);
      if (next.has(key)) {
        if (next.size > 1) next.delete(key); // never allow all-off
      } else {
        next.add(key);
      }
      return next;
    });
  };

  return (
    <div className={cn("flex min-h-0 flex-col", className)}>
      {/* Group filters */}
      <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border px-2.5">
        <div className="flex items-center gap-1 overflow-x-auto">
          {FILTERS.map((f) => (
            <button
              key={f.key}
              type="button"
              onClick={() => toggleFilter(f.key)}
              className={cn(
                "whitespace-nowrap rounded-full border px-2 py-0.5 text-[11px] transition-colors",
                activeFilters.has(f.key)
                  ? "border-brand/40 bg-brand/10 text-brand"
                  : "border-border text-faint-foreground hover:text-muted-foreground",
              )}
            >
              {f.label}
            </button>
          ))}
        </div>
      </div>

      {/* Rows */}
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto pb-1.5">
        {rows.length > 0 && (
          <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-border bg-background/95 px-3 py-1 text-[10px] font-medium uppercase tracking-wider text-faint-foreground backdrop-blur">
            <span className="w-7 shrink-0 text-right">seq</span>
            <span className="w-[112px] shrink-0">event</span>
            <span className="min-w-0 flex-1">summary</span>
            <span className="shrink-0">time</span>
          </div>
        )}
        {rows.map((row) =>
          row.kind === "turn" ? (
            <div key={`t-${row.n}`} className="my-1 flex items-center gap-2 px-3">
              <span className="h-px flex-1 bg-border" />
              <span className="text-[9px] font-medium uppercase tracking-wider text-faint-foreground">
                turn {row.n}
              </span>
              <span className="h-px flex-1 bg-border" />
            </div>
          ) : row.kind === "event" ? (
            <LogRow
              key={row.event.seq}
              event={row.event}
              {...(failedEnds.has(row.event.seq) && FAILED_TURN_END)}
              selected={selected?.seq === row.event.seq}
              onSelect={onSelect}
              entered={entered}
            />
          ) : (
            <LogRow
              key={`g-${row.start.seq}-${row.deltas[0].seq}`}
              event={row.start}
              labelOverride={row.reasoning ? "Thinking Δ" : "Message Δ"}
              colorOverride={row.reasoning ? "text-muted-foreground" : undefined}
              summaryOverride={{
                main: row.deltas
                  .map((d) => deltaKindAndText(d.payload).text)
                  .join(""),
              }}
              selected={selected?.seq === row.start.seq}
              onSelect={onSelect}
              entered={entered}
            />
          ),
        )}
        {rows.length === 0 && (
          <div className="px-4 py-10 text-center text-xs text-faint-foreground">
            No events match the active filters.
          </div>
        )}
      </div>

      {/* Tail status */}
      <div className="flex h-7 shrink-0 items-center gap-2 border-t border-border px-3 text-[11px] text-faint-foreground">
        {live ? (
          <>
            <span className="size-1.5 rounded-full bg-success animate-pulse-dot" />
            <span className="text-success">streaming…</span>
            <span className="ml-1">seq {events[events.length - 1]?.seq ?? 0}</span>
          </>
        ) : (
          <>
            <span className="size-1.5 rounded-full bg-muted-foreground/50" />
            tail idle · {events.length} events
            <span className="ml-auto font-mono">
              Last-Event-ID: {shortId(events[events.length - 1]?.id ?? "—")}
            </span>
          </>
        )}
      </div>
    </div>
  );
}
