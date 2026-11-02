"use client";

import { Eye, EyeOff, KeyRound, Trash2 } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { TableSkeletonRows } from "@/components/table-skeleton";
import { Input } from "@/components/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import * as api from "@/lib/api";
import { formatRelative } from "@/lib/events";
import { useAsync } from "@/lib/hooks";
import type { App } from "@/lib/types";

/*
  A project's secrets manager (GET/PUT/DELETE /secrets?app=<slug>). Write-only
  by design: the UI never renders a stored value, and the
  eye toggle exists only while TYPING a new one.
*/

export function ProjectSecrets({ app }: { app: App }) {
  const slug = app.slug;
  const [reloadTick, setReloadTick] = useState(0);
  const reload = () => setReloadTick((t) => t + 1);
  const { data: secrets, loading } = useAsync(
    () => (slug ? api.listSecrets(slug) : Promise.resolve([])),
    [slug, reloadTick],
  );
  const [name, setName] = useState("");
  const replaces = !!name && (secrets ?? []).some((x) => x.name === name);
  const [value, setValue] = useState("");
  const [revealed, setRevealed] = useState(false); // for typing comfort only
  const [busy, setBusy] = useState(false);

  const set = async () => {
    const n = name.trim().toUpperCase().replaceAll(" ", "_");
    if (!n || !value || !slug || busy) return;
    setBusy(true);
    try {
      await api.setSecret(slug, n, value);
      reload();
      toast("Secret saved", { description: `${n} on ${app.name} — encrypted at rest.` });
      setName("");
      setValue("");
    } catch (e) {
      toast.error("Set failed", {
        description: e instanceof Error ? e.message : String(e),
      });
    } finally {
      setBusy(false);
    }
  };

  const remove = async (key: string) => {
    if (!slug) return;
    try {
      await api.deleteSecret(slug, key);
      reload();
      toast("Secret deleted", { description: key });
    } catch (e) {
      toast.error("Delete failed", {
        description: e instanceof Error ? e.message : String(e),
      });
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle>Secrets</CardTitle>
        <CardDescription>
          Injected into your agent processes at invoke time as environment
          variables. Write-only: values are encrypted at rest and can never be
          viewed again — only replaced or deleted. 
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex gap-2">
          <Input
            placeholder="SECRET_NAME"
            className="max-w-52 font-mono text-xs"
            value={name}
            onChange={(e) => setName(e.target.value.toUpperCase().replaceAll(" ", "_"))}
          />
          <div className="relative flex-1">
            <Input
              type={revealed ? "text" : "password"}
              placeholder="secret value"
              className="pr-9 font-mono text-xs"
              value={value}
              onChange={(e) => setValue(e.target.value)}
            />
            <button
              type="button"
              onClick={() => setRevealed((r) => !r)}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-faint-foreground hover:text-muted-foreground"
              aria-label={revealed ? "Hide value while typing" : "Show value while typing"}
            >
              {revealed ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
            </button>
          </div>
          <Button onClick={set} disabled={!name.trim() || !value || !slug || busy}>
            <KeyRound /> {replaces ? "Replace" : "Set"}
          </Button>
        </div>
        {replaces && (
          <p className="-mt-2 text-xs text-warning">
            Replaces the existing value of {name}.
          </p>
        )}

        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Value</TableHead>
              <TableHead>Updated</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {loading && <TableSkeletonRows widths={["w-32", "w-20", "w-14", ""]} />}
            {!loading && (secrets ?? []).length === 0 && (
              <TableRow>
                <TableCell colSpan={4} className="text-xs text-muted-foreground">
                  No secrets on {app.name}.
                </TableCell>
              </TableRow>
            )}
            {(secrets ?? []).map((s) => (
              <TableRow key={s.name}>
                <TableCell className="font-mono text-xs font-medium">
                  {s.name}
                </TableCell>
                <TableCell>
                  <Badge variant="muted" className="font-mono">
                    ••••••••
                  </Badge>
                </TableCell>
                <TableCell className="text-xs text-muted-foreground">
                  {formatRelative(s.updated_at ?? s.created_at)}
                </TableCell>
                <TableCell className="text-right">
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    title="Delete secret"
                    onClick={() => remove(s.name)}
                  >
                    <Trash2 className="text-muted-foreground" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  );
}
