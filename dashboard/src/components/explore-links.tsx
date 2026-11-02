import { BookOpen, LayoutTemplate } from "lucide-react";
import { DropdownMenu as DropdownMenuPrimitive } from "radix-ui";
import { DiscordIcon } from "@/components/discord-icon";
import { cn } from "@/lib/utils";

export const DOCS_URL = "https://docs.sectr.dev";
export const SAMPLES_URL =
  "https://github.com/sectr-dev/sectr-copilotkit-starter";
export const DISCORD_URL = "https://sectr.dev/discord";
/** "How to deploy your first agent on Sectr" (~3 min, YouTube). */
export const WALKTHROUGH_URL = "https://www.youtube.com/watch?v=H-y3wynkKc0";

const LINKS = [
  { href: DOCS_URL, label: "Docs", icon: BookOpen },
  { href: SAMPLES_URL, label: "Samples", icon: LayoutTemplate },
  { href: DISCORD_URL, label: "Discord", icon: DiscordIcon },
];

/** The muted "Or explore [Docs] [Samples] [Discord]" line: the welcome's
 *  (`md`) and the ghost card's (`sm`, which drops the lead-in when the
 *  card or menu is narrow). `menuItems` makes each link an item of the
 *  enclosing dropdown menu, so arrow keys reach it (the Create new menu). */
export function ExploreLinks({
  size = "md",
  menuItems = false,
  className,
}: {
  size?: "sm" | "md";
  menuItems?: boolean;
  className?: string;
}) {
  const sm = size === "sm";
  return (
    <div
      className={cn(
        "flex flex-wrap items-center",
        sm ? "justify-center gap-1.5" : "gap-2",
        className,
      )}
    >
      <span
        className={cn(
          "text-faint-foreground",
          sm ? "mr-0.5 hidden text-xs @xs:inline" : "mr-1 text-sm",
        )}
      >
        Or explore
      </span>
      {LINKS.map(({ href, label, icon: Icon }) => {
        const link = (
          <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            className={cn(
              "inline-flex items-center rounded-full border border-border bg-card font-medium text-muted-foreground transition-colors outline-none hover:border-brand/60 hover:text-foreground focus-visible:border-brand/60 focus-visible:text-foreground data-highlighted:border-brand/60 data-highlighted:text-foreground",
              sm ? "gap-1 px-2.5 py-1 text-xs" : "gap-1.5 px-3 py-1.5 text-sm",
            )}
          >
            <Icon className={sm ? "size-3" : "size-3.5"} />
            {label}
          </a>
        );
        return menuItems ? (
          <DropdownMenuPrimitive.Item key={label} asChild>
            {link}
          </DropdownMenuPrimitive.Item>
        ) : (
          <span key={label} className="contents">
            {link}
          </span>
        );
      })}
    </div>
  );
}
