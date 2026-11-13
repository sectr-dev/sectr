"use client";

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { ArrowLeft, PanelRight, PanelRightClose } from "lucide-react";
import { ApprovalBanner } from "@/components/approval-banner";
import { ChatInput } from "@/components/chat-input";
import { EventPayloadInspector } from "@/components/event-payload-inspector";
import { ChatStream, JournalLog } from "@/components/session-views";
import { SessionStatusBadge } from "@/components/status-badges";
import { SessionBreadcrumb } from "@/components/session-breadcrumb";
import { formatRelative } from "@/lib/events";
import { useSessionEvents } from "@/lib/hooks";
import * as api from "@/lib/api";
import type { JournalEvent } from "@/lib/transport/types";
import type {
  EventEnvelope,
  Session,
  ToolApprovalRequiredPayload,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { lastTurnFailed } from "@/lib/session-status";

type Decision = "pending" | "approved" | "denied";

const NARROW_MQ = "(min-width: 1280px)";

// Terminal event types: after one of these, no output is in flight. (A
// turn ending at NO_MORE_ACTIONS leaves the session ACTIVE — open for
// follow-ups; the badge shows the session summary's status.)
const TERMINAL: ReadonlySet<JournalEvent["type"]> = new Set([
  "NO_MORE_ACTIONS",
  "ERROR",
  "SESSION_ERROR",
]);

// ---------------------------------------------------------------------------
// Journal-rail preferences — persisted per user (not per session) in
// localStorage, exposed via useSyncExternalStore so SSR hydration is clean.
// ---------------------------------------------------------------------------

interface RailPrefs {
  open: boolean;
  pct: number;
}

const DEFAULT_PREFS: RailPrefs = { open: true, pct: 0.42 };

function clampPct(pct: unknown): number {
  return typeof pct === "number" && pct >= 0.35 && pct <= 0.7 ? pct : DEFAULT_PREFS.pct;
}

let prefListeners: (() => void)[] = [];

const railPrefStore = {
  get(): string {
    return (typeof window !== "undefined" && localStorage.getItem(RAIL_PREF_KEY)) || "";
  },
  set(next: RailPrefs) {
    localStorage.setItem(RAIL_PREF_KEY, JSON.stringify(next));
    prefListeners.forEach((l) => l());
  },
  subscribe(cb: () => void) {
    prefListeners.push(cb);
    const onStorage = () => cb();
    window.addEventListener("storage", onStorage);
    return () => {
      prefListeners = prefListeners.filter((l) => l !== cb);
      window.removeEventListener("storage", onStorage);
    };
  },
};

function useRailPrefs() {
  const raw = useSyncExternalStore(railPrefStore.subscribe, railPrefStore.get, () => "");
  return useMemo(() => {
    try {
      const p = JSON.parse(raw || "{}") as Partial<RailPrefs>;
      return { open: p.open ?? DEFAULT_PREFS.open, pct: clampPct(p.pct) };
    } catch {
      return DEFAULT_PREFS;
    }
  }, [raw]);
}

function subscribeNarrow(cb: () => void) {
  const mq = window.matchMedia(NARROW_MQ);
  mq.addEventListener("change", cb);
  return () => mq.removeEventListener("change", cb);
}

/** Below xl the rail becomes an overlay drawer instead of a column. */
function useIsNarrow() {
  return useSyncExternalStore(
    subscribeNarrow,
    () => !window.matchMedia(NARROW_MQ).matches,
    () => false,
  );
}

const RAIL_PREF_KEY = "sectr.journalRail";

/**
 * The session surface: chat left, journal right.
 *
 * Left = ChatStream (the conversation; playground sessions get an input).
 * Right = the journal rail: the event log (Grouped | Raw), or — after
 * clicking an event — that event's payload with a Back button. IDE conventions throughout — draggable divider, collapse
 * (double-click divider, chevron, ⌘J, or the Journal button in the meta bar),
 * overlay drawer below xl. Preference persists per user, not per session.
 *
 * Data flow: everything comes from the journal tail subscription
 * (`useSessionEvents` → api.openSessionStream) — full replay, then live.
 * Sending = POST input; deciding = POST approval; both journal, and the
 * resumed turn's output arrives on the same tail (mock and real behave
 * identically from here).
 */
export function SessionInspector({ session }: { session: Session }) {
  const events = useSessionEvents(session.id);
  const last = events[events.length - 1];
  const lastRef = useRef<JournalEvent | null>(null);
  const [selected, setSelected] = useState<JournalEvent | null>(null);

  // Selection follows the tail while output is streaming (as the old
  // local simulation did); inspecting pins it until the next event.
  useEffect(() => {
    if (last && last !== lastRef.current) {
      lastRef.current = last;
      setSelected(last);
    }
  }, [last]);

  // --- journal rail state (persisted per user) ---
  const prefs = useRailPrefs();
  const railOpen = prefs.open;
  const railPct = prefs.pct;
  const setRailPrefs = useCallback(
    (patch: Partial<RailPrefs>) => railPrefStore.set({ open: railOpen, pct: railPct, ...patch }),
    [railOpen, railPct],
  );
  // The rail shows the event list, or one event's payload (reached by
  // clicking a row or a chat node; "Back" returns to the list).
  const [detail, setDetail] = useState(false);
  const [raw, setRaw] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const narrow = useIsNarrow();

  // ⌘J / Ctrl+J — toggle the journal rail (IDE convention, J = journal).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "j") {
        e.preventDefault();
        railPrefStore.set({ open: !railOpen, pct: railPct });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [railOpen, railPct]);

  const toggleRail = useCallback(() => setRailPrefs({ open: !railOpen }), [railOpen, setRailPrefs]);

  const lastApproval = useMemo(
    () =>
      [...events]
        .reverse()
        .find((e) => e.type === "TOOL_APPROVAL_REQUIRED") as
        | EventEnvelope<ToolApprovalRequiredPayload>
        | undefined,
    [events],
  );
  const decidedAfterApproval = useMemo(() => {
    if (!lastApproval) return undefined;
    return [...events]
      .reverse()
      .find(
        (e) =>
          e.seq > lastApproval.seq &&
          (e.type === "TOOL_APPROVAL_GRANTED" || e.type === "TOOL_APPROVAL_DENIED"),
      )?.type;
  }, [events, lastApproval]);

  const decision: Decision =
    decidedAfterApproval === "TOOL_APPROVAL_GRANTED"
      ? "approved"
      : decidedAfterApproval === "TOOL_APPROVAL_DENIED"
        ? "denied"
        : "pending";

  const approvalPending = !!lastApproval && decision === "pending";
  // Output in flight: the tail hasn't reached a terminal event. Paused on
  // an approval is NOT live — the banner owns that state.
  const live = !!last && !TERMINAL.has(last.type) && !approvalPending;

  const onDecide = useCallback(
    (approved: boolean) => {
      void api.decideApproval(session.id, { approved });
    },
    [session.id],
  );

  // Chat: a user_message invocation — the turn journals and its output
  // arrives on the tail.
  const onSend = useCallback(
    (text: string) => {
      void api.sendSessionInput(session.id, text);
    },
    [session.id],
  );

  /** Clicking any chat/journal node: select it and open its detail view. */
  const inspect = useCallback((e: JournalEvent) => {
    setSelected(e);
    setDetail(true);
    if (!railOpen) setRailPrefs({ open: true });
  }, [railOpen, setRailPrefs]);

  // Follows the tail, by the server's rules: the summary's status can be
  // older than the events.
  const status = approvalPending
    ? ("awaiting_approval" as const)
    : events.length === 0 || session.status === "ended"
      ? session.status
      : lastTurnFailed(events)
        ? ("error" as const)
        : ("active" as const);

  const railContent = (
    <>
      {/* Rail header: Grouped | Raw (or Back, in the detail view) + collapse */}
      <div className="flex h-9 shrink-0 items-center border-b border-border px-2">
        {detail ? (
          <button
            type="button"
            onClick={() => setDetail(false)}
            className="flex h-6 items-center gap-1 rounded px-1.5 text-[11px] font-medium text-muted-foreground transition-colors hover:text-foreground"
          >
            <ArrowLeft className="size-3.5" />
            Back
          </button>
        ) : (
          <div className="flex divide-x divide-border overflow-hidden rounded-md border border-border">
            {([false, true] as const).map((isRaw) => (
              <button
                key={String(isRaw)}
                type="button"
                onClick={() => setRaw(isRaw)}
                className={cn(
                  "h-6 px-2.5 text-[11px] font-medium transition-colors",
                  raw === isRaw
                    ? "bg-surface-raised text-foreground"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                {isRaw ? "Raw" : "Grouped"}
              </button>
            ))}
          </div>
        )}
        <button
          type="button"
          onClick={toggleRail}
          aria-label="Hide journal rail"
          title="Hide journal rail (⌘J)"
          className="ml-auto flex size-6 items-center justify-center rounded text-faint-foreground transition-colors hover:text-foreground"
        >
          <PanelRightClose className="size-3.5" />
        </button>
      </div>
      {!detail ? (
        <JournalLog
          className="min-h-0 flex-1"
          events={events}
          raw={raw}
          selected={selected}
          onSelect={inspect}
          live={live}
          entered={live}
        />
      ) : (
        <EventPayloadInspector className="min-h-0 flex-1" event={selected} />
      )}
    </>
  );

  return (
    <div className="flex h-[calc(100svh-3.5rem)] flex-col overflow-hidden">
      {/* Session meta bar */}
      {/* A container query, not a viewport one: the bar's own width (the
          sidebar eats some). Wide: one row. Below @3xl the identity block
          stacks (breadcrumb over status · age) with the Journal button
          beside it, vertically centered, so there is no mostly-empty
          second row. */}
      <div className="@container flex shrink-0 items-center gap-x-4 border-b border-border px-6 py-2 @3xl:py-3">
        <div className="flex min-w-0 flex-col gap-1.5 @3xl:flex-row @3xl:items-center @3xl:gap-x-4">
          {/* Breadcrumb: app › route › session id */}
          <SessionBreadcrumb session={session} />
          {/* Status and age read as one fact; the hover adds start time and
              the deployment the session is pinned to */}
          <div className="flex shrink-0 items-center gap-x-4">
            <span
              className="flex items-center gap-2"
              title={`Started ${new Date(session.created_at).toLocaleString()} · deployment ${session.deployment_id}`}
            >
              <SessionStatusBadge status={status} />
              <span className="text-xs text-faint-foreground">
                {formatRelative(session.created_at)}
              </span>
            </span>
            {session.external_user_id && (
              <span
                className="truncate rounded border px-1.5 font-mono text-[11px] text-muted-foreground"
                title="Session owner (external user id)"
              >
                {session.external_user_id}
              </span>
            )}
          </div>
        </div>
        {/* Journal toggle. The event count says there is something worth
            opening. */}
        <button
          type="button"
          onClick={toggleRail}
          aria-pressed={railOpen}
          aria-label={railOpen ? "Hide journal" : "Show journal"}
          title={`${railOpen ? "Hide" : "Show"} journal (⌘J)`}
          className={cn(
            "ml-auto flex shrink-0 items-center gap-2.5 rounded-md border px-3 py-1.5 text-left transition-colors",
            railOpen
              ? "border-brand/40 bg-brand/10 text-brand"
              : "border-border-strong bg-surface text-foreground hover:bg-surface-raised",
          )}
        >
          <PanelRight className="size-4 shrink-0" />
          <span className="flex flex-col gap-0.5">
            <span className="flex items-center gap-2 text-xs font-medium leading-none">
              Journal
            </span>
            <span className="flex items-center gap-1.5 text-[11px] leading-none text-muted-foreground">
              {events.length} events
            </span>
          </span>
          <kbd className="hidden items-center gap-0.5 rounded border border-current/20 px-1.5 py-0.5 font-mono text-xs font-normal leading-none opacity-60 @3xl:inline-flex">
            <span className="text-sm leading-none">⌘</span>
            <span>J</span>
          </kbd>
        </button>
      </div>

      <ApprovalBanner
        event={lastApproval ?? null}
        decision={decision}
        onDecide={onDecide}
      />

      {/* Chat + journal rail */}
      <div ref={containerRef} className="relative flex min-h-0 flex-1">
        {/* Chat column */}
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="min-h-0 flex-1">
            <ChatStream
              className="h-full"
              events={events}
              selected={selected}
              onSelect={inspect}
              live={live}
              entered={live}
            />
          </div>
          {session.origin === "playground" && (
            <ChatInput
              onSend={onSend}
              disabled={live || approvalPending}
              disabledHint={
                live
                  ? "Agent is responding…"
                  : approvalPending
                    ? "Paused on approval — decide above"
                    : undefined
              }
            />
          )}
        </div>

        {/* Divider + column rail (xl+) */}
        {railOpen && !narrow && (
          <>
            <div
              role="separator"
              aria-orientation="vertical"
              aria-label="Resize journal rail"
              onDoubleClick={toggleRail}
              onPointerDown={(e) => {
                e.preventDefault();
                const container = containerRef.current;
                if (!container) return;
                const rect = container.getBoundingClientRect();
                const onMove = (ev: PointerEvent) => {
                  const pct = Math.min(
                    0.7,
                    Math.max(0.35, (rect.right - ev.clientX) / rect.width),
                  );
                  railPrefStore.set({ open: true, pct });
                };
                const onUp = () => {
                  window.removeEventListener("pointermove", onMove);
                };
                window.addEventListener("pointermove", onMove);
                window.addEventListener("pointerup", onUp, { once: true });
              }}
              className="group relative z-10 flex w-px shrink-0 cursor-col-resize items-center justify-center bg-border transition-colors hover:bg-brand/40"
            >
              {/* wide hit area */}
              <div className="absolute inset-y-0 -left-1.5 -right-1.5" />
              <PanelRightClose className="size-3 text-faint-foreground opacity-0 transition-opacity group-hover:opacity-100" />
            </div>
            <div
              className="flex min-h-0 flex-col border-l border-border"
              style={{ width: `${railPct * 100}%` }}
            >
              {railContent}
            </div>
          </>
        )}

        {/* Overlay drawer (<xl) */}
        {railOpen && narrow && (
          <>
            <div
              className="fixed inset-0 z-20 bg-background/60"
              onClick={toggleRail}
              aria-hidden
            />
            <div className="fixed bottom-0 right-0 top-14 z-30 flex w-[min(420px,85vw)] flex-col border-l border-border bg-background shadow-2xl">
              {railContent}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
