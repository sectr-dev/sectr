import { cn } from "@/lib/utils";

/**
 * The brand dusk behind a page's top: a faint grid fading out from the top
 * edge, under an orange glow, like the marketing hero. `hero` is the
 * empty-org welcome's full-strength version; `subtle` is for everyday pages
 * (the project grid), where it only sets the mood. Place inside a
 * `relative isolate overflow-hidden` container.
 */
export function BrandBackdrop({ strength }: { strength: "hero" | "subtle" }) {
  const hero = strength === "hero";
  return (
    <>
      <div
        aria-hidden
        className={cn(
          "bg-grid pointer-events-none absolute inset-x-0 top-0 -z-10 [mask-image:radial-gradient(ellipse_70%_60%_at_50%_0%,black,transparent)]",
          hero ? "h-[36rem]" : "h-[22rem] opacity-70",
        )}
      />
      <div
        aria-hidden
        className={cn(
          "pointer-events-none absolute left-1/2 -z-10 max-w-[140%] -translate-x-1/2 rounded-full blur-3xl",
          hero
            ? "-top-56 h-[32rem] w-[60rem] bg-brand/20"
            : "-top-48 h-[22rem] w-[52rem] bg-brand/[0.09]",
        )}
      />
    </>
  );
}
