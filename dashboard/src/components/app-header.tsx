"use client";

import { ExternalLink } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { ConnectFrontendMenu } from "@/components/connect-frontend-menu";
import { PageHeader } from "@/components/page-header";
import * as api from "@/lib/api";
import { FRAMEWORK_LABEL } from "@/lib/events";
import type { App } from "@/lib/types";

/**
 * Shared app-page header — identical across the app's tabs (detail,
 * sessions, playground). CLIENT: the app resolves browser-side in gateway
 * mode (dev gateway's /dev/status — the edge can't reach localhost), so
 * the pages that render this are client components.
 */
export function AppPageHeader({
  app,
  actions,
  children,
}: {
  app: App;
  actions?: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <PageHeader
      title={
        <span className="flex items-center gap-2">
          <span className="flex size-8 items-center justify-center rounded-lg border border-brand/25 bg-brand/10 text-[13px] font-semibold text-brand-foreground dark:text-brand">
            {app.name[0].toUpperCase()}
          </span>
          {app.name}
        </span>
      }
      breadcrumbs={[{ label: "Projects", href: "/" }, { label: app.name }]}
      actions={
        <>
          <ConnectFrontendMenu app={app} />
          {actions}
        </>
      }
    >
      <div className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
        {app.framework && <Badge variant="muted">{FRAMEWORK_LABEL[app.framework]}</Badge>}
        {app.url && (
          <a
            href={/^https?:\/\//.test(app.url) ? app.url : `https://${app.url}`}
            target="_blank"
            rel="noreferrer"
            className="flex items-center gap-1 font-mono text-[13px] text-foreground hover:underline"
          >
            {app.url}
            <ExternalLink className="size-3.5" />
          </a>
        )}
        {app.url && app.description && <span>·</span>}
        {app.description && <span>{app.description}</span>}
      </div>
      {children}
    </PageHeader>
  );
}

/** App pages resolve the app client-side; this is their "no such app". */
export function AppNotFound({ id }: { id: string }) {
  return (
    <div className="flex h-[60vh] flex-col items-center justify-center gap-2 text-center">
      <p className="text-sm font-medium">No project with id {id}</p>
      <p className="max-w-md text-sm text-muted-foreground">
        {api.PROD_MODE ? (
          "It may have been deleted, or it belongs to a different organization. Head back to your projects to pick another."
        ) : (
          <>
            In dev-gateway mode the console shows only your local project (from the gateway&apos;s
            /dev/status). Start it with{" "}
            <code className="rounded border border-border bg-surface px-1 py-0.5 font-mono text-[12px]">
              sectr dev
            </code>{" "}
            — or clear the saved gateway to browse the demo data.
          </>
        )}
      </p>
    </div>
  );
}
