import Link from "next/link";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/**
 * Tab bar shared by the app pages. Overview (routes + deployments), Sessions,
 * Playground and Secrets live on separate routes but present as one tabbed page —
 * sessions are the most-used surface and shouldn't hide behind a button.
 */
export function AppTabs({
  appId,
  sessionsCount,
  active,
}: {
  appId: string;
  sessionsCount: number;
  active: "overview" | "sessions" | "playground" | "secrets";
}) {
  const tabs = [
    { key: "overview" as const, label: "Overview", href: `/apps/${appId}`, count: null },
    { key: "sessions" as const, label: "Sessions", href: `/apps/${appId}/sessions`, count: sessionsCount },
    { key: "playground" as const, label: "Playground", href: `/apps/${appId}/playground`, count: null },
    { key: "secrets" as const, label: "Secrets", href: `/apps/${appId}/secrets`, count: null },
  ];
  return (
    <div className="flex shrink-0 gap-1 border-b border-border px-6">
      {tabs.map((t) => (
        <Link
          key={t.key}
          href={t.href}
          className={cn(
            "relative flex h-9 items-center gap-1.5 px-3 text-sm font-medium transition-colors",
            active === t.key
              ? "text-foreground"
              : "text-foreground/70 hover:text-foreground",
          )}
        >
          {t.label}
          {t.count != null && (
            <Badge variant="muted" className="border-brand/40 bg-brand/20 px-1.5 text-[10px] text-brand-foreground dark:border-brand/50 dark:bg-brand/25 dark:text-brand">
              {t.count}
            </Badge>
          )}
          {active === t.key && (
            <span className="absolute inset-x-2 bottom-0 h-0.5 rounded-full bg-foreground" />
          )}
        </Link>
      ))}
    </div>
  );
}
