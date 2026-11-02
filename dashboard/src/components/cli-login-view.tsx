"use client";

/**
 * The approval page for `sectr auth login`: the CLI printed a
 * link with a code; a signed-in user (this renders inside the console's
 * AuthGate) confirms the code matches their terminal and clicks Approve.
 * Never auto-approves — the click is the defense against someone sending
 * you their own login link.
 */

import { CircleCheck, CircleX, LoaderCircle, Terminal, TriangleAlert } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import * as api from "@/lib/api";
import type { CliLoginRequest } from "@/lib/transport/types";

type View =
  | { kind: "loading" }
  | { kind: "request"; request: CliLoginRequest }
  | { kind: "gone" }
  | { kind: "error"; message: string };

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex min-h-[60svh] items-center justify-center p-4">
      <div className="w-full max-w-md space-y-5 rounded-xl border border-border bg-surface p-7 text-center shadow-[0_8px_30px_var(--shadow-card)]">
        {children}
      </div>
    </div>
  );
}

function ago(iso: string, now: number): string {
  const secs = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000));
  if (secs < 60) return `${secs} s ago`;
  return `${Math.round(secs / 60)} min ago`;
}

/** `/cli` with no code: type the one the terminal printed. */
export function CliCodeEntry() {
  const router = useRouter();
  const [code, setCode] = useState("");
  const normalized = code.replace(/[^a-zA-Z0-9]/g, "").toUpperCase();
  return (
    <Shell>
      <Terminal className="mx-auto size-6 text-muted-foreground" />
      <h1 className="text-xl font-medium">Sign in the Sectr CLI</h1>
      <p className="text-sm text-muted-foreground">
        Enter the code <code className="font-mono">sectr auth login</code> printed in your terminal.
      </p>
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (normalized.length === 8) router.push(`/cli/${normalized}`);
        }}
      >
        <Input
          value={code}
          onChange={(e) => setCode(e.target.value)}
          placeholder="WDJB-MJHT"
          className="font-mono uppercase"
          autoFocus
          aria-label="Code from your terminal"
        />
        <Button type="submit" disabled={normalized.length !== 8}>
          Continue
        </Button>
      </form>
    </Shell>
  );
}

export function CliLoginView({ code }: { code: string }) {
  const [view, setView] = useState<View>({ kind: "loading" });
  const [busy, setBusy] = useState<"approve" | "deny" | null>(null);
  const [done, setDone] = useState<"approved" | "denied" | null>(null);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let cancelled = false;
    api
      .getCliLogin(code)
      .then((request) => !cancelled && setView({ kind: "request", request }))
      .catch((err: unknown) => {
        if (cancelled) return;
        if (err instanceof api.ApiError && err.status === 404) setView({ kind: "gone" });
        else
          setView({
            kind: "error",
            message: err instanceof Error ? err.message : "Couldn't load the request.",
          });
      });
    return () => {
      cancelled = true;
    };
  }, [code]);

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  async function decide(action: "approve" | "deny") {
    setBusy(action);
    try {
      if (action === "approve") await api.approveCliLogin(code);
      else await api.denyCliLogin(code);
      setDone(action === "approve" ? "approved" : "denied");
    } catch (err) {
      if (err instanceof api.ApiError && err.status === 409) {
        // Already decided elsewhere (another tab): show the current state.
        try {
          const request = await api.getCliLogin(code);
          setView({ kind: "request", request });
        } catch {
          setView({ kind: "gone" });
        }
      } else if (err instanceof api.ApiError && err.status === 404) {
        setView({ kind: "gone" });
      } else {
        setView({
          kind: "error",
          message: err instanceof Error ? err.message : "Something went wrong.",
        });
      }
    } finally {
      setBusy(null);
    }
  }

  if (done === "approved") {
    return (
      <Shell>
        <CircleCheck className="mx-auto size-7 text-success" />
        <h1 className="text-xl font-medium">You&apos;re signed in</h1>
        <p className="text-sm text-muted-foreground">Go back to your terminal.</p>
      </Shell>
    );
  }
  if (done === "denied") {
    return (
      <Shell>
        <CircleX className="mx-auto size-7 text-muted-foreground" />
        <h1 className="text-xl font-medium">Request denied</h1>
        <p className="text-sm text-muted-foreground">The CLI was not signed in.</p>
      </Shell>
    );
  }

  if (view.kind === "loading") {
    return (
      <Shell>
        <LoaderCircle className="mx-auto size-5 animate-spin text-muted-foreground" />
      </Shell>
    );
  }
  if (view.kind === "gone") {
    return (
      <Shell>
        <TriangleAlert className="mx-auto size-6 text-warning" />
        <h1 className="text-xl font-medium">Request not found</h1>
        <p className="text-sm text-muted-foreground">
          This login request expired or was already used. Run{" "}
          <code className="font-mono">sectr auth login</code> again.
        </p>
      </Shell>
    );
  }
  if (view.kind === "error") {
    return (
      <Shell>
        <TriangleAlert className="mx-auto size-6 text-danger" />
        <p className="text-sm text-danger">{view.message}</p>
        <Button variant="outline" onClick={() => window.location.reload()}>
          Retry
        </Button>
      </Shell>
    );
  }

  const { request } = view;
  if (request.status !== "pending") {
    return (
      <Shell>
        <h1 className="text-xl font-medium">
          {request.status === "denied" ? "Request denied" : "Already approved"}
        </h1>
        <p className="text-sm text-muted-foreground">
          This request was already {request.status === "denied" ? "denied" : "approved"}. Run{" "}
          <code className="font-mono">sectr auth login</code> again if you need a new one.
        </p>
        <Link href="/" className="text-sm underline underline-offset-2">
          Back to the console
        </Link>
      </Shell>
    );
  }

  return (
    <Shell>
      <Terminal className="mx-auto size-6 text-muted-foreground" />
      <h1 className="text-xl font-medium">Sign in the Sectr CLI?</h1>
      <p className="text-sm text-muted-foreground">
        <span className="font-mono text-foreground">{request.client_name}</span> is asking to sign in
        to your account · requested {ago(request.created_at, now)}
      </p>
      <div>
        <div className="inline-block rounded-lg border border-border bg-background px-5 py-3 font-mono text-2xl tracking-widest">
          {request.user_code}
        </div>
        <p className="mt-2 text-xs text-muted-foreground">
          Make sure this matches the code in your terminal.
        </p>
      </div>
      <div className="flex gap-2">
        <Button
          variant="outline"
          className="flex-1"
          disabled={busy !== null}
          onClick={() => void decide("deny")}
        >
          Deny
        </Button>
        <Button className="flex-1" disabled={busy !== null} onClick={() => void decide("approve")}>
          {busy === "approve" && <LoaderCircle className="animate-spin" />}
          Approve
        </Button>
      </div>
      <p className="text-xs text-faint-foreground">
        Only approve if you just ran <code className="font-mono">sectr auth login</code>.
      </p>
    </Shell>
  );
}
