"use client";

import { useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import {
  FolderGit2,
  KeyRound,
  LayoutGrid,
  MessagesSquare,
  Search,
  ShieldAlert,
} from "lucide-react";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useApps, useSessions } from "@/lib/hooks";
import { cn } from "@/lib/utils";

/*
  CommandPalette — quick-nav behind ⌘K (shell v0.1). Searches apps,
  sessions, and console pages via api.ts hooks; keyboard driven
  (↑/↓/Enter, Esc). Phase 7 keeps this shape (api.ts swaps bodies).
*/

type Item = {
  label: string;
  sub?: string;
  href: string;
  kind: "Page" | "Project" | "Session";
  icon: React.ComponentType<{ className?: string }>;
};

const PAGES: Item[] = [
  { label: "Projects", href: "/", kind: "Page", icon: LayoutGrid },
  { label: "Pending Approvals", href: "/approvals", kind: "Page", icon: ShieldAlert },
  { label: "Settings", href: "/settings", kind: "Page", icon: KeyRound },
];

export function CommandPalette({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const router = useRouter();
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);

  // Reset search state on open, and snap the cursor back on query change —
  // via the adjust-state-during-render pattern (react.dev) rather than
  // effects, which would cascade renders.
  const [prevOpen, setPrevOpen] = useState(open);
  const [prevQuery, setPrevQuery] = useState(query);
  if (open !== prevOpen) {
    setPrevOpen(open);
    if (open) {
      setQuery("");
      setCursor(0);
    }
  }
  if (query !== prevQuery) {
    setPrevQuery(query);
    setCursor(0);
  }

  const { apps } = useApps();
  const sessions = useSessions();

  const items = useMemo<Item[]>(
    () => [
      ...PAGES,
      ...apps.map((a) => ({
        label: a.name,
        sub: a.description,
        href: `/apps/${a.id}`,
        kind: "Project" as const,
        icon: FolderGit2,
      })),
      ...sessions.map((s) => ({
        label: s.id,
        sub: `${s.app_name} · ${s.route}`,
        href: `/sessions/${s.id}`,
        kind: "Session" as const,
        icon: MessagesSquare,
      })),
    ],
    [apps, sessions],
  );

  const results = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return items;
    return items.filter((i) =>
      `${i.label} ${i.sub ?? ""}`.toLowerCase().includes(q),
    );
  }, [query, items]);

  const go = (href: string) => {
    onOpenChange(false);
    router.push(href);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setCursor((c) => Math.min(c + 1, results.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setCursor((c) => Math.max(c - 1, 0));
    } else if (e.key === "Enter" && results[cursor]) {
      e.preventDefault();
      go(results[cursor].href);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton={false}
        className="top-[18%] translate-y-0 gap-0 overflow-hidden p-0 sm:max-w-lg"
        onKeyDown={onKeyDown}
      >
        <DialogTitle className="sr-only">Search</DialogTitle>
        <div className="flex items-center gap-2.5 border-b border-border px-4">
          <Search className="size-4 shrink-0 text-faint-foreground" />
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search projects, sessions, pages…"
            className="h-12 w-full bg-transparent text-sm outline-none placeholder:text-faint-foreground"
          />
          <kbd className="rounded border border-border px-1 font-mono text-[10px] text-faint-foreground">
            esc
          </kbd>
        </div>
        <div className="max-h-80 overflow-y-auto p-1.5">
          {results.length === 0 && (
            <div className="px-3 py-8 text-center text-xs text-faint-foreground">
              No matches.
            </div>
          )}
          {results.map((item, i) => (
            <button
              key={`${item.href}-${item.label}`}
              type="button"
              onMouseEnter={() => setCursor(i)}
              onClick={() => go(item.href)}
              className={cn(
                "flex w-full items-center gap-2.5 rounded-md px-2.5 py-2 text-left text-[13px] transition-colors",
                i === cursor ? "bg-surface-raised" : "",
              )}
            >
              <item.icon className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate font-medium">{item.label}</span>
              {item.sub && (
                <span className="truncate text-[11px] text-faint-foreground">
                  {item.sub}
                </span>
              )}
              <span className="ml-auto shrink-0 rounded border border-border px-1 text-[10px] text-faint-foreground">
                {item.kind}
              </span>
            </button>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}
