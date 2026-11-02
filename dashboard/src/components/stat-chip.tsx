import { cn } from "@/lib/utils";

/** The rounded status pill under a page title: live sessions pulse,
 *  approvals turn amber ("attention"). */
export function StatChip({
  tone = "default",
  children,
}: {
  tone?: "default" | "muted" | "live" | "attention";
  children: React.ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium transition-colors",
        tone === "default" && "border-border bg-card text-foreground",
        tone === "muted" && "border-border bg-card text-muted-foreground",
        tone === "live" && "border-success/30 bg-success/10 text-success",
        tone === "attention" &&
          "border-warning/40 bg-warning/10 text-warning hover:border-warning/70 hover:bg-warning/15",
      )}
    >
      {children}
    </span>
  );
}

/** "1 project" / "2 projects". */
export function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}
