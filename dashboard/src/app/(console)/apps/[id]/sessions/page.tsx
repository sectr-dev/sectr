"use client";

import { useParams } from "next/navigation";
import { AppSessionsView } from "@/components/app-sessions-view";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { useApp } from "@/lib/hooks";

/** App sessions. CLIENT: the app resolves browser-side (gateway mode —
 *  the edge can't reach localhost); the table and tab count are
 *  browser-fetched too. */
export default function AppSessionsPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="pb-12">
      {/* Tabbed page: the header is the app's, identical across tabs — only
          the tab bar and content change. */}
      <AppPageHeader app={app} />
      <AppSessionsView app={app} />
    </div>
  );
}
