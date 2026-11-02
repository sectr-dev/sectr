"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import * as api from "@/lib/api";
import {
  BookOpen,
  Check,
  ChevronDown,
  Command,
  KeyRound,
  LayoutGrid,
  Monitor,
  Moon,
  PanelLeft,
  ShieldAlert,
  Sun,
} from "lucide-react";
import { BackendBadge } from "@/components/backend-badge";
import { Logo, LogoMark } from "@/components/logo";
import { getClerkBridge } from "@/lib/clerk-session";
import { useApprovals, useApps, useAsync } from "@/lib/hooks";
import { CommandPalette } from "@/components/command-palette";
import { DiscordIcon } from "@/components/discord-icon";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useTheme, type Theme } from "@/lib/theme";
import { cn } from "@/lib/utils";

const NAV = [
  { href: "/approvals", label: "Pending Approvals", icon: ShieldAlert },
  { href: "/settings", label: "Settings", icon: KeyRound, exact: false },
];

const FOOTER_LINKS = [
  { href: "https://docs.sectr.dev", label: "Docs", icon: BookOpen, newTab: false },
  { href: "https://sectr.dev/discord", label: "Discord Community", icon: DiscordIcon, newTab: true },
];

const THEME_OPTIONS: { value: Theme; label: string; icon: React.ComponentType<{ className?: string }> }[] = [
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
  { value: "system", label: "System", icon: Monitor },
];

export function AppShell({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  const { theme, setTheme } = useTheme();
  const { apps } = useApps();
  const pending = useApprovals();
  const [collapsed, setCollapsed] = useState(false);
  const [appsOpen, setAppsOpen] = useState(true);

  // Portaled overlays (the step modals) center on the content area, not the
  // viewport: publish the sidebar's width (w-16 / w-56) for them to offset by.
  useEffect(() => {
    const root = document.documentElement;
    root.style.setProperty("--sidebar-w", collapsed ? "4rem" : "14rem");
    return () => {
      root.style.removeProperty("--sidebar-w");
    };
  }, [collapsed]);

  // Build-time inlined lockstep version (next.config.ts env) — the real
  // build on app.sectr.dev, replacing the old hardcoded "v0.1". Mock-mode
  // keeps its demo-data hint.
  const versionLabel = api.MOCK_MODE
    ? `shell v${process.env.NEXT_PUBLIC_SECTR_VERSION} — demo data`
    : `v${process.env.NEXT_PUBLIC_SECTR_VERSION}`;
  const [paletteOpen, setPaletteOpen] = useState(false);
  const identity = useIdentity();

  // A session page (/sessions/<id>) belongs to an app, but its URL doesn't
  // say which — resolve it so the sidebar can highlight that app.
  const sessionId = pathname.match(/^\/sessions\/([^/]+)/)?.[1];
  const { data: routedSession } = useAsync(
    () => (sessionId ? api.getSession(sessionId) : Promise.resolve(undefined)),
    [sessionId],
  );
  const sessionAppId = sessionId
    ? apps.find((a) => a.id === routedSession?.app_id || a.name === routedSession?.app_id)?.id
    : undefined;
  const inApps = pathname === "/" || pathname.startsWith("/apps") || !!sessionId;

  // Global ⌘K / Ctrl-K toggle.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="flex min-h-svh">
      {/* Sidebar */}
      <aside
        className={cn(
          "fixed inset-y-0 left-0 z-20 flex flex-col border-r border-border bg-sidebar transition-[width] duration-200",
          collapsed ? "w-16" : "w-56",
        )}
      >
        <div className={cn("flex h-14 items-center border-b border-border", collapsed ? "justify-center px-0" : "px-4")}>
          <Link href="/" className="flex items-center overflow-hidden">
            {collapsed ? <LogoMark /> : <Logo />}
          </Link>
          <Button
            variant="ghost"
            size="icon-sm"
            className={cn("text-faint-foreground", collapsed ? "" : "ml-auto")}
            onClick={() => setCollapsed(!collapsed)}
            aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
          >
            <PanelLeft className={cn(collapsed && "scale-x-[-1]")} />
          </Button>
        </div>

        <nav className="flex-1 space-y-0.5 px-2 py-3">
          {/* Apps — expandable group: label navigates to the overview,
              chevron toggles the sub-list */}
          <div
            className={cn(
              "relative flex h-8 items-center rounded-md transition-colors",
              collapsed && "justify-center px-0",
              inApps
                ? "bg-brand/[0.08] text-foreground"
                : "text-foreground hover:bg-brand/[0.06]",
            )}
          >
            {inApps && (
              <span
                aria-hidden
                className="absolute inset-y-1.5 left-0.5 w-[3px] rounded-full bg-brand shadow-[0_0_8px_rgb(240_154_82/60%)]"
              />
            )}
            <Link
              href="/"
              title="Projects"
              className={cn(
                "flex min-w-0 flex-1 items-center gap-2.5 px-2.5 text-[13px] font-medium",
                collapsed && "justify-center px-0",
              )}
            >
              <LayoutGrid className="size-4 shrink-0" />
              {!collapsed && <span>Projects</span>}
            </Link>
            {!collapsed && (
              <button
                type="button"
                onClick={() => setAppsOpen((o) => !o)}
                aria-label={appsOpen ? "Collapse Projects section" : "Expand Projects section"}
                aria-expanded={appsOpen}
                className="mr-1 flex size-6 items-center justify-center rounded text-faint-foreground transition-colors hover:text-foreground"
              >
                <ChevronDown
                  className={cn("size-3.5 transition-transform", !appsOpen && "-rotate-90")}
                />
              </button>
            )}
          </div>
          {appsOpen &&
            !collapsed &&
            apps.map((app) => {
              const active = pathname.startsWith(`/apps/${app.id}`) || app.id === sessionAppId;
              return (
                <Link
                  key={app.id}
                  href={`/apps/${app.id}`}
                  title={app.name}
                  className={cn(
                    "relative flex h-7 items-center gap-2.5 rounded-md pl-8 pr-2.5 text-[12px] transition-colors",
                    active
                      ? "bg-brand/[0.08] text-foreground"
                      : "text-foreground hover:bg-brand/[0.06]",
                  )}
                >
                  {active && (
                    <span
                      aria-hidden
                      className="absolute inset-y-1 left-6 w-[3px] rounded-full bg-brand shadow-[0_0_8px_rgb(240_154_82/60%)]"
                    />
                  )}
                  <span className="flex size-4 shrink-0 items-center justify-center rounded border border-border bg-surface-raised text-[9px] font-semibold">
                    {app.name[0].toUpperCase()}
                  </span>
                  <span className="truncate">{app.name}</span>
                </Link>
              );
            })}

          {NAV.map((item) => {
            const badge = item.href === "/approvals" ? pending.length : undefined;
            const active = pathname.startsWith(item.href);
            return (
              <Link
                key={item.href}
                href={item.href}
                title={item.label}
                className={cn(
                  "relative flex h-8 items-center gap-2.5 rounded-md px-2.5 text-[13px] font-semibold transition-colors",
                  collapsed && "justify-center px-0",
                  active
                    ? "bg-brand/[0.08] text-foreground"
                    : "text-foreground hover:bg-brand/[0.06]",
                )}
              >
                {active && (
                  <span
                    aria-hidden
                    className="absolute inset-y-1.5 left-0.5 w-[3px] rounded-full bg-brand shadow-[0_0_8px_rgb(240_154_82/60%)]"
                  />
                )}
                <item.icon className="size-4 shrink-0" />
                {!collapsed && (
                  <>
                    <span>{item.label}</span>
                    {badge ? (
                      <span className="ml-auto rounded-full bg-warning/15 px-1.5 py-px text-[10px] font-semibold text-warning">
                        {badge}
                      </span>
                    ) : null}
                  </>
                )}
                {collapsed && badge ? (
                  <span className="absolute right-1.5 top-1.5 size-1.5 rounded-full bg-warning" />
                ) : null}
              </Link>
            );
          })}
        </nav>

        <div className="space-y-0.5 border-t border-border px-2 py-3">
          {/* External links. Docs at docs.sectr.dev — console-origin /docs path retired. Community goes via
              sectr.dev/discord, never a raw invite, so the invite
              can rotate without a dashboard deploy. Icon-only when
              collapsed, like the nav items. */}
          {FOOTER_LINKS.map((link) => (
            <a
              key={link.href}
              href={link.href}
              title={link.label}
              {...(link.newTab ? { target: "_blank", rel: "noopener noreferrer" } : {})}
              className={cn(
                "flex h-8 items-center gap-2.5 rounded-md px-2.5 text-[13px] text-muted-foreground transition-colors hover:bg-surface-raised/60 hover:text-foreground",
                collapsed && "justify-center px-0",
              )}
            >
              <link.icon className="size-4 shrink-0" />
              {!collapsed && <span>{link.label}</span>}
            </a>
          ))}
          {collapsed && (
            <div className="px-2 pt-2 text-center text-[10px] text-faint-foreground">
              {versionLabel}
            </div>
          )}
          {!collapsed && (
            <div className="px-2.5 pt-2 text-[11px] text-faint-foreground">
              {versionLabel}
            </div>
          )}
        </div>
      </aside>

      {/* Main column */}
      <div className={cn("flex min-w-0 flex-1 flex-col transition-[padding] duration-200", collapsed ? "pl-16" : "pl-56")}>
        {/* Topbar */}
        <header className="sticky top-0 z-10 flex h-14 items-center gap-3 border-b border-border bg-background/80 px-6 backdrop-blur">
          <BackendBadge />
          <button
            className="absolute left-1/2 hidden h-7 w-64 -translate-x-1/2 items-center gap-2 rounded-md border border-border bg-surface px-2.5 text-xs text-muted-foreground transition-colors hover:border-border-strong md:flex"
            type="button"
            onClick={() => setPaletteOpen(true)}
          >
            <Command className="size-3.5" />
            Search projects, sessions…
            <kbd className="ml-auto rounded border border-border px-1 font-mono text-[10px]">⌘K</kbd>
          </button>

          <div className="ml-auto flex items-center gap-2">
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="ghost" size="icon" className="rounded-full border border-border transition-colors hover:border-brand/60 hover:bg-brand/[0.04]" aria-label="Account menu">
                  <span className="flex size-7 items-center justify-center rounded-full bg-surface-raised text-[11px] font-semibold">
                    {identity.initials}
                  </span>
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-52">
                <DropdownMenuLabel>
                  <div className="text-sm font-medium text-foreground">{identity.orgName}</div>
                  <div className="text-xs font-normal text-muted-foreground">{identity.subline}</div>
                </DropdownMenuLabel>
                <DropdownMenuSeparator />
                <DropdownMenuLabel className="text-xs text-muted-foreground">
                  Theme
                </DropdownMenuLabel>
                {THEME_OPTIONS.map(({ value, label, icon: Icon }) => (
                  <DropdownMenuItem key={value} onClick={() => setTheme(value)}>
                    <Icon />
                    {label}
                    {theme === value && <Check className="ml-auto size-3.5 text-brand" />}
                  </DropdownMenuItem>
                ))}
                <DropdownMenuSeparator />
                <DropdownMenuItem asChild>
                  <Link href="/settings">Settings</Link>
                </DropdownMenuItem>
                {/* Not implemented yet.
                <DropdownMenuItem>
                  <FolderGit2 /> New organization
                </DropdownMenuItem>
                */}
                <SignOutItem />
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </header>

        <main className="flex-1">{children}</main>
      </div>

      <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} />
    </div>
  );
}

/** The signed-in identity for the shell (org switcher + account menu).
 *  GET /me — the MVP has no user records, so the org IS the identity and
 *  the account subline describes the auth mode instead of a person:
 *  prod = the stored platform key's tail, dev/mock = their mode. */
function useIdentity() {
  const [orgName, setOrgName] = useState("…");
  const [subline, setSubline] = useState("");
  useEffect(() => {
    let alive = true;
    void api.me().then(
      (me) => {
        if (!alive) return;
        setOrgName(me.org.name);
        setSubline(
          api.PROD_MODE
            ? `platform key ····${(api.getPlatformKey() ?? "").slice(-4)}`
            : api.MOCK_MODE
              ? "demo data"
              : "dev gateway — no auth",
        );
      },
      () => {
        if (alive) setOrgName(api.PROD_MODE ? "sectr" : "Local dev");
      },
    );
    return () => {
      alive = false;
    };
  }, []);
  const initials =
    orgName
      .split(/\s+/)
      .slice(0, 2)
      .map((w) => w[0]?.toUpperCase() ?? "")
      .join("") || "S";
  return { orgName, subline, initials };
}

/** Sign-out: revoke the dashboard-session key
 *  (`POST /auth/logout`, best effort), clear the stored key, then end the Clerk session if one
 *  is active (Clerk navigates to /login itself). The gateway pin is NOT
 *  touched — it belongs to `sectr dev` sessions (clear via ?gateway=). */
function SignOutItem() {
  const router = useRouter();
  return (
    <DropdownMenuItem
      onClick={() => {
        void (async () => {
          // In-flight requests that 401 on the key being revoked must not
          // re-exchange the still-live Clerk session for a new one.
          api.markSignedOut(api.getPlatformKey());
          if (api.PROD_MODE) await api.logoutRemote();
          await api.logout();
          const clerk = getClerkBridge();
          if (clerk?.isSignedIn) {
            await clerk.signOut("/login");
          } else {
            router.push("/login");
          }
        })();
      }}
    >
      Sign out
    </DropdownMenuItem>
  );
}
