import { cn } from "@/lib/utils";

/**
 * One numbered step on a vertical timeline: a glowing brand number, a rail
 * down to the next step, then the title, optional description and content.
 * Wrap the steps in an `<ol>`. `lg` is the empty-org welcome's; `sm` the
 * step modals'. One component, so the two can't drift apart.
 */
export function TimelineStep({
  n,
  title,
  description,
  last = false,
  size = "lg",
  children,
}: {
  n: number;
  title: string;
  description?: string;
  last?: boolean;
  size?: "lg" | "sm";
  children?: React.ReactNode;
}) {
  const lg = size === "lg";
  return (
    <li
      className={cn(
        "grid",
        lg
          ? "grid-cols-[2.25rem_1fr] gap-x-5"
          : "grid-cols-[1.75rem_1fr] gap-x-3.5",
      )}
    >
      <div className="flex flex-col items-center">
        <span
          className={cn(
            "flex shrink-0 items-center justify-center rounded-full border border-brand/50 bg-background font-semibold text-brand-foreground dark:text-brand",
            lg
              ? "size-9 text-base shadow-[0_0_16px_rgb(240_154_82/30%)]"
              : "size-7 text-xs shadow-[0_0_12px_rgb(240_154_82/30%)]",
          )}
        >
          {n}
        </span>
        {!last && (
          <span
            className={cn(
              "w-px flex-1 bg-gradient-to-b from-brand/50 to-border",
              lg ? "mt-2" : "mt-1.5",
            )}
          />
        )}
      </div>
      <div
        className={cn(
          "min-w-0",
          lg ? "pt-0.5" : "pt-1",
          !last && (lg ? "pb-12" : "pb-5"),
        )}
      >
        {lg ? (
          <h2 className="text-xl font-semibold tracking-tight">{title}</h2>
        ) : (
          <div className="text-sm font-medium">{title}</div>
        )}
        {description && (
          <p
            className={cn(
              "mt-1 text-muted-foreground",
              lg ? "text-[15px]" : "text-xs",
            )}
          >
            {description}
          </p>
        )}
        {children && <div className={lg ? "mt-4" : "mt-2"}>{children}</div>}
      </div>
    </li>
  );
}
