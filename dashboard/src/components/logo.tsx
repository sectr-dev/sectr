import { cn } from "@/lib/utils";

/**
 * Sectr wordmark. The mark is the logo graphic (public/logo.png) framed
 * exactly like our standard Button theme — default variant, size sm:
 * cream (bg-primary), rounded like the sm radius, with the primary
 * foreground. Not interactive, so no hover/focus affordances.
 *
 * The wordmark text is set in Varino Normal (--font-varino), with mono
 * fallbacks; Varino ships a single weight, so no font weight is applied.
 * Varino's metrics put the glyphs' visual center ~10% of the em above the
 * line-box midpoint (cap 700, baseline at 75% of a leading-none box), so
 * the translate-y compensates to keep the wordmark truly middle-aligned
 * with the mark at any font size.
 *
 * `size="lg"` is the hero wordmark for the pages the console greets a
 * human on (login); the default fits the sidebar.
 */
type LogoSize = "default" | "lg";

const MARK_SIZE: Record<LogoSize, { frame: string; img: string }> = {
    default: { frame: "size-5 rounded-[min(var(--radius-md),12px)]", img: "size-4" },
    lg: { frame: "size-10 rounded-lg", img: "size-8" },
};

export function LogoMark({ className, size = "default" }: { className?: string; size?: LogoSize }) {
    return (
        <span
            className={cn(
                "flex shrink-0 items-center justify-center bg-white ring-1 ring-foreground/10 dark:bg-[#f4f4f5] dark:ring-border",
                MARK_SIZE[size].frame,
                className,
            )}
        >
            {/* eslint-disable-next-line @next/next/no-img-element -- static asset, no optimization needed */}
            <img src="/logo.png" alt="" className={MARK_SIZE[size].img} aria-hidden />
        </span>
    );
}

export function Logo({ className, size = "default" }: { className?: string; size?: LogoSize }) {
    return (
        <span className={cn("inline-flex items-center", size === "lg" ? "gap-3.5" : "gap-2", className)}>
            <LogoMark size={size} />
            <span
                className={cn(
                    "translate-y-[0.1em] font-[family-name:var(--font-varino)] leading-none tracking-tight",
                    size === "lg" ? "text-[34px]" : "text-[15px]",
                )}
            >
                SECTR
            </span>
        </span>
    );
}
