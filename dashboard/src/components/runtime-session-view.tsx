"use client";

import { SearchX } from "lucide-react";
import { SessionInspector } from "@/components/session-inspector";
import { toUiSession } from "@/lib/view";
import { useAsync, useApps } from "@/lib/hooks";
import * as api from "@/lib/api";

/**
 * Resolves sessions that only exist in the client-side runtime registry
 * (playground sessions created in this browser). Server render can't see
 * sessionStorage, so the dynamic route hands unknown ids to this gate
 * before falling back to "not found".
 */
export function RuntimeSessionView({ id }: { id: string }) {
  const { apps } = useApps();
  const { data: raw, loading } = useAsync(() => api.getSession(id), [id]);
  const session = raw ? toUiSession(raw, apps) : undefined;
  // NOTE: no useSessionEvents here — the inspector subscribes itself; a
  // second stream per page is a wasted server tail AND (pre-batching) a
  // duplicate render driver.

  if (loading) {
    return (
      <div className="flex min-h-[60svh] items-center justify-center">
        <p className="text-sm text-muted-foreground">Resolving session…</p>
      </div>
    );
  }

  if (!session) {
    return (
      <div className="flex min-h-[60svh] flex-col items-center justify-center gap-2 px-6 text-center">
        <SearchX className="size-6 text-faint-foreground" />
        <p className="text-sm font-medium">Session not found</p>
        <p className="max-w-72 text-xs text-muted-foreground">
          {api.PROD_MODE
            ? "No session with this id exists in your organization. Check the link, or open it from a project's sessions list."
            : api.SESSIONS_LIVE
              ? "Unknown to the gateway — the session may have been created against a different gateway."
              : "This session was created in another browser — playground sessions live in this console's local storage in the mock shell."}
        </p>
      </div>
    );
  }

  return <SessionInspector session={session} />;
}
