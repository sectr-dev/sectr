"use client";

import { toast } from "sonner";
import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

/**
 * <Soon> — wraps a post-MVP affordance so it reads as "designed, not shipped".
 *
 * Children render at reduced opacity with pointer events disabled (so real
 * buttons inside can never fire), the whole area carries a not-allowed
 * cursor, a small mono tag marks the corner, and any click raises a toast.
 * Interactive controls elsewhere on the page stay fully live — grayed means
 * preview, not broken page.
 */
export function Soon({
  children,
  message = "This ships after MVP — shown here as a preview.",
  className,
  tag = "soon",
}: {
  children: ReactNode;
  /** Toast copy shown on click; name the feature for flavor. */
  message?: string;
  className?: string;
  /** Corner tag text; pass null to omit. */
  tag?: string | null;
}) {
  return (
    <div
      title={message}
      aria-disabled
      onClick={() => toast(message, { id: "soon-preview" })}
      className={cn(
        "group/soon relative cursor-not-allowed select-none",
        className,
      )}
    >
      <div className="pointer-events-none opacity-55">{children}</div>
      {tag && (
        <span className="pointer-events-none absolute -top-1.5 right-2 z-10 rounded-full border border-border bg-surface px-1.5 py-px font-mono text-[9px] uppercase tracking-widest text-faint-foreground">
          {tag}
        </span>
      )}
    </div>
  );
}
