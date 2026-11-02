"use client";

import { deploymentVersionLabel } from "@/lib/view";
import { useEffect, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { ArrowUp, Info } from "lucide-react";
import type { App } from "@/lib/types";
import { createSession } from "@/lib/api";
import { cn } from "@/lib/utils";

/**
 * Playground — chat with a freshly created session on a chosen agent.
 * First message creates the session (mock: client-side runtime registry)
 * and navigates to /sessions/[id]; the URL *is* the shareable link.
 */
export function PlaygroundClient({ app }: { app: App }) {
  const router = useRouter();
  const searchParams = useSearchParams();
  const routeParam = searchParams.get("route");
  const [route, setRoute] = useState(
    () =>
      (routeParam && app.routes.some((r) => r.path === routeParam)
        ? routeParam
        : app.routes[0]?.path) ?? "",
  );
  const [prevParam, setPrevParam] = useState(routeParam);
  // Focus the message box on open — the playground's whole purpose is
  // typing a first message.
  const inputRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    inputRef.current?.focus();
  }, []);
  // Arriving via an Agents-card chat link (?route=/x) — including clicking a
  // different agent's chat button while already on the playground. Adjusted
  // during render per the React "derive state from props" pattern.
  if (routeParam !== prevParam) {
    setPrevParam(routeParam);
    if (routeParam && app.routes.some((r) => r.path === routeParam)) {
      setRoute(routeParam);
    }
  }
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const deployment = app.latest_deployment;
  const deploymentReady = deployment.status === "active";

  const send = () => {
    const message = text.trim();
    if (!message || sending) return;
    if (!deploymentReady) return;
    setSending(true);
    // First message creates the session (POST /sessions); its first turn
    // starts immediately and the session page streams the output.
    void createSession({ app: app.id, route, input: message }).then((created) => {
      // Brief beat so the send reads as intentional, then the session page
      // takes over and streams the assistant turn.
      timer.current = setTimeout(() => router.push(`/sessions/${created.session_id}`), 350);
    });
  };

  return (
    // One centered column, ChatGPT-style: greeting, input, then the quiet
    // extras. Sent messages move to the session page, where the input sits
    // at the bottom under the conversation. The bottom padding lifts the
    // block a little above true center, where the eye expects it.
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center overflow-y-auto px-6 pb-[10vh] pt-10">
      <div className="w-full max-w-3xl">
        <div className="mb-8 text-center">
          <h1 className="text-3xl font-medium tracking-tight">
            Chat with{" "}
            <span className="font-mono text-[0.92em] text-brand">{route || "…"}</span>
          </h1>
        </div>

        {!deploymentReady && (
          <p className="mb-3 flex items-center justify-center gap-1.5 rounded-md border border-warning/30 bg-warning/[0.07] px-2.5 py-1.5 text-xs text-warning">
            <Info className="size-3.5" />
            No ready deployment yet — ship one to chat.
          </p>
        )}

        <div className="flex items-end gap-3 rounded-2xl border border-border bg-surface px-4 py-3 shadow-[0_2px_16px_var(--shadow-card)] transition-colors focus-within:border-border-strong">
          <textarea
            ref={inputRef}
            rows={1}
            value={text}
            placeholder={
              deploymentReady
                ? `Message ${route}…`
                : "No ready deployment — ship one first"
            }
            disabled={!deploymentReady}
            onChange={(e) => {
              setText(e.target.value);
              e.target.style.height = "auto";
              e.target.style.height = `${Math.min(e.target.scrollHeight, 200)}px`;
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                send();
              }
            }}
            className="max-h-[200px] min-h-9 flex-1 resize-none bg-transparent py-1.5 text-base leading-6 text-foreground outline-none placeholder:text-faint-foreground"
          />
          <button
            type="button"
            onClick={send}
            disabled={!deploymentReady || !text.trim() || sending}
            aria-label="Send and create session"
            className={cn(
              "flex size-9 shrink-0 items-center justify-center rounded-full transition-colors",
              deploymentReady && text.trim() && !sending
                ? "bg-brand text-background hover:bg-brand/90"
                : "bg-muted text-faint-foreground",
            )}
          >
            <ArrowUp className="size-4.5" />
          </button>
        </div>

        {app.routes.length > 1 && (
          <div className="mt-3 flex flex-wrap items-center justify-center gap-1.5">
            <span className="text-xs text-faint-foreground">agent</span>
            {app.routes.map((r) => (
              <button
                key={r.path}
                type="button"
                onClick={() => setRoute(r.path)}
                className={cn(
                  "flex items-center gap-1.5 rounded-full border px-3 py-1 font-mono text-xs transition-colors",
                  route === r.path
                    ? "border-brand/60 bg-brand/10 text-foreground"
                    : "border-border text-faint-foreground hover:text-muted-foreground",
                )}
              >
                <span
                  className={cn(
                    "size-1.5 rounded-full",
                    route === r.path ? "bg-brand" : "bg-muted-foreground/30",
                  )}
                />
                {r.path}
              </button>
            ))}
          </div>
        )}

        <div className="mt-4 text-center text-[11px] text-faint-foreground">
          Pinned to <span className="font-mono">{deploymentVersionLabel(deployment)}</span> · sessions pin to the deployment they started on
        </div>
      </div>
    </div>
  );
}
