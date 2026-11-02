"use client";

import { useParams } from "next/navigation";
import { AppDetailView } from "@/components/app-detail-view";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { ProjectStatChips } from "@/components/project-stat-chips";
import { useApp } from "@/lib/hooks";

/** App detail. CLIENT: the app resolves browser-side (gateway mode — the
 *  edge can't reach localhost); the tab bar's live session count is
 *  browser-fetched inside AppDetailView. */
export default function AppDetailPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="pb-12">
      <AppPageHeader app={app}>
        <ProjectStatChips app={app} />
      </AppPageHeader>
      <AppDetailView app={app} />
    </div>
  );
}
