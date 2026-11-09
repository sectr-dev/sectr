"use client";

import { useParams } from "next/navigation";
import { AppConnectionsView } from "@/components/app-connections-view";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { ProjectStatChips } from "@/components/project-stat-chips";
import { useApp } from "@/lib/hooks";

/** The project's first tab, Connections. CLIENT: the app resolves
 *  browser-side (gateway mode — the edge can't reach localhost); the tab
 *  bar's live session count is browser-fetched inside the view. */
export default function AppConnectionsPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="pb-12">
      <AppPageHeader app={app}>
        <ProjectStatChips app={app} />
      </AppPageHeader>
      <AppConnectionsView app={app} />
    </div>
  );
}
