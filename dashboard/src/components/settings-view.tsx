"use client";

import { Check, Plus, Server, Terminal, Trash2, type LucideIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { TableSkeletonRows } from "@/components/table-skeleton";
import { CommandBlock } from "@/components/new-project-card";
import { cn } from "@/lib/utils";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import * as api from "@/lib/api";
import type { App } from "@/lib/types";
import type { ApiKeyMeta } from "@/lib/transport/types";
import { useAsync, useApps } from "@/lib/hooks";
import { formatRelative } from "@/lib/events";

/*
  Settings — the org-wide API keys manager, against the REAL control plane
  (GET/POST/DELETE /api-keys). Secrets are per project and live on the
  project's Secrets tab (project-secrets.tsx).

  API keys are shown as kind-derived masks — the store keeps only SHA-256
  hashes, so no key prefix exists to display; a new key's value is shown
  exactly once, at creation time.
*/

/** Reload tick for useAsync — bumps to re-fetch after mutations. */
function useReload() {
  const [tick, setTick] = useState(0);
  return [tick, () => setTick((t) => t + 1)] as const;
}

const KIND_OPTIONS: {
  value: "platform" | "invocation";
  icon: LucideIcon;
  title: string;
  description: string;
}[] = [
  {
    value: "platform",
    icon: Terminal,
    title: "Platform",
    description: "For the CLI and dashboard. Works across your whole organization.",
  },
  {
    value: "invocation",
    icon: Server,
    title: "Invocation",
    description: "For your backend, pinned to one project. Never use it in a browser.",
  },
];

function KeyMask({ kind }: { kind: "platform" | "invocation" }) {
  return (
    <span className="flex items-center gap-2 font-mono text-xs text-muted-foreground">
      {kind === "platform" ? "skp_" : "ski_"}••••••••
    </span>
  );
}

function AppSelect({
  apps,
  value,
  onChange,
  id,
}: {
  apps: App[];
  value: string;
  onChange: (id: string) => void;
  id: string;
}) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger id={id} className="w-56">
        <SelectValue placeholder="Pick a project" />
      </SelectTrigger>
      <SelectContent>
        {apps.map((a) => (
          <SelectItem key={a.id} value={a.id}>
            {a.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function ApiKeysCard() {
  const [reloadTick, reload] = useReload();
  const { data: keys, loading } = useAsync(() => api.listApiKeys(), [reloadTick]);
  const { apps } = useApps();
  const [newName, setNewName] = useState("");
  const [kind, setKind] = useState<"platform" | "invocation">("platform");
  const [appId, setAppId] = useState<string>("");
  const [created, setCreated] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [showSessions, setShowSessions] = useState(false);
  const [revoking, setRevoking] = useState<{ id: string; name: string; expires_at?: string | null } | null>(null);
  const [now] = useState(() => Date.now());

  const createKey = async () => {
    setBusy(true);
    try {
      const created = await api.createApiKey(
        newName.trim() || "untitled",
        kind,
        kind === "invocation" ? appId || undefined : undefined,
      );
      setCreated(created.key);
      setNewName("");
      reload();
    } catch (e) {
      toast.error("Key creation failed", {
        description: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setBusy(false);
    }
  };

  const revoke = async () => {
    if (!revoking) return;
    const k = revoking;
    setRevoking(null);
    try {
      await api.revokeApiKey(k.id);
      reload();
      toast("Key revoked", { description: k.name });
    } catch (e) {
      toast.error("Revoke failed", {
        description: e instanceof Error ? e.message : String(e),
      });
    }
  };

  // Expired == revoked (the server treats them alike). Of the live keys,
  // those with an expires_at are dashboard sessions (one per login):
  // hidden behind a toggle so they don't bury the keys people made.
  const live = (keys ?? []).filter(
    (k) => !k.revoked_at && !(k.expires_at && Date.parse(k.expires_at) <= now),
  );
  const sessions = live.filter((k) => k.expires_at);
  const visible = showSessions ? live : live.filter((k) => !k.expires_at);

  return (
    <Card>
      <CardHeader>
        <CardTitle>API keys</CardTitle>
        <CardDescription>
          Authenticate the CLI, SDK, and your backend against Sectr.
        </CardDescription>
        <CardAction>
          <Dialog open={open} onOpenChange={(o) => { setOpen(o); if (!o) setCreated(null); }}>
            <DialogTrigger asChild>
              <Button size="sm">
                <Plus /> Create key
              </Button>
            </DialogTrigger>
          <DialogContent>
            {created ? (
              <>
                <DialogHeader>
                  <DialogTitle>Copy your key now</DialogTitle>
                  <DialogDescription>
                    This is the only time the full key is shown. Sectr stores
                    only its hash. Store it somewhere safe.
                  </DialogDescription>
                </DialogHeader>
                <CommandBlock command={created} kind="prompt" />
                <DialogFooter>
                  <Button variant="secondary" onClick={() => { setOpen(false); setCreated(null); }}>
                    Done
                  </Button>
                </DialogFooter>
              </>
            ) : (
              <>
                <DialogHeader>
                  <DialogTitle>Create API key</DialogTitle>
                  <DialogDescription>
                    Give the key a name so you can recognize it later.
                  </DialogDescription>
                </DialogHeader>
                <div className="space-y-5">
                  <div className="space-y-2">
                    <Label htmlFor="key-name">Key name</Label>
                    <Input
                      id="key-name"
                      placeholder="e.g. production"
                      value={newName}
                      onChange={(e) => setNewName(e.target.value)}
                    />
                  </div>
                  <div className="space-y-2">
                    <Label id="key-kind-label">Kind</Label>
                    <div role="radiogroup" aria-labelledby="key-kind-label" className="grid gap-2">
                      {KIND_OPTIONS.map((o) => {
                        const selected = kind === o.value;
                        return (
                          <button
                            key={o.value}
                            type="button"
                            role="radio"
                            aria-checked={selected}
                            onClick={() => setKind(o.value)}
                            className={cn(
                              "flex items-center gap-3 rounded-lg border px-3 py-2.5 text-left shadow-xs outline-none transition-colors focus-visible:border-brand/60",
                              selected
                                ? "border-brand/60 bg-brand/[0.06]"
                                : "border-border bg-card hover:border-brand/40 hover:bg-brand/[0.03]",
                            )}
                          >
                            <span className="flex size-8 shrink-0 items-center justify-center rounded-md border border-brand/25 bg-brand/10 text-brand-foreground dark:text-brand">
                              <o.icon className="size-4" />
                            </span>
                            <span className="min-w-0 flex-1">
                              <span className="block text-sm font-semibold">{o.title}</span>
                              <span className="mt-0.5 block text-xs leading-snug text-muted-foreground">
                                {o.description}
                              </span>
                            </span>
                            {selected && <Check className="size-4 shrink-0 text-brand" />}
                          </button>
                        );
                      })}
                    </div>
                  </div>
                  {kind === "invocation" && (
                    <div className="space-y-2">
                      <Label htmlFor="key-app">Project</Label>
                      {apps.length > 0 ? (
                        <AppSelect apps={apps} value={appId} onChange={setAppId} id="key-app" />
                      ) : (
                        <p className="text-xs text-faint-foreground">
                          No projects yet — deploy one first.
                        </p>
                      )}
                      <p className="text-xs text-muted-foreground">
                        Your server uses an invocation key to mint short-lived
                        per-user browser tokens (<code className="font-mono">POST /tokens</code>).
                        Never put one in a browser for a multi-user project.
                      </p>
                    </div>
                  )}
                </div>
                <DialogFooter>
                  <Button
                    onClick={createKey}
                    disabled={busy || !newName.trim() || (kind === "invocation" && !appId)}
                  >
                    Create
                  </Button>
                </DialogFooter>
              </>
            )}
          </DialogContent>
          </Dialog>
        </CardAction>
      </CardHeader>
      <CardContent className="px-0 pb-2">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="pl-4">Name</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead>Key</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Last used</TableHead>
              <TableHead className="pr-4" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {loading && (
              <TableSkeletonRows
                widths={["w-28", "w-16", "w-24", "w-14", "w-14", ""]}
                firstCellClassName="pl-4"
              />
            )}
            {!loading && visible.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="pl-4 text-xs text-muted-foreground">
                  No keys yet.
                </TableCell>
              </TableRow>
            )}
            {visible.map((k: ApiKeyMeta) => {
              const app = apps.find((a) => a.id === k.app_id);
              return (
                <TableRow key={k.id}>
                  <TableCell className="pl-4 text-sm font-medium">{k.name}</TableCell>
                  <TableCell>
                    <Badge variant={k.kind === "platform" ? "info" : "muted"}>{k.kind}</Badge>
                    {app && (
                      <span className="ml-2 text-xs text-muted-foreground">{app.name}</span>
                    )}
                  </TableCell>
                  <TableCell>
                    <KeyMask kind={k.kind} />
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {formatRelative(k.created_at)}
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {k.last_used_at ? formatRelative(k.last_used_at) : "never"}
                  </TableCell>
                  <TableCell className="pr-4 text-right">
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      title="Revoke key"
                      onClick={() => setRevoking(k)}
                    >
                      <Trash2 className="text-muted-foreground" />
                    </Button>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
        <Dialog open={revoking !== null} onOpenChange={(o) => !o && setRevoking(null)}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Revoke {revoking?.name}?</DialogTitle>
              <DialogDescription>
                {revoking?.expires_at
                  ? "This signs that browser session out. "
                  : "Anything using this key stops working immediately. "}
                This can&apos;t be undone.
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setRevoking(null)}>
                Cancel
              </Button>
              <Button variant="destructive" onClick={revoke}>
                Revoke key
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
        {sessions.length > 0 && (
          <div className="px-4 pt-3">
            <button
              type="button"
              className="text-xs text-faint-foreground transition-colors hover:text-muted-foreground"
              onClick={() => setShowSessions((v) => !v)}
            >
              {showSessions
                ? "Hide browser sessions"
                : `Show browser sessions (${sessions.length})`}
            </button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function SettingsView() {
  return <ApiKeysCard />;
}
