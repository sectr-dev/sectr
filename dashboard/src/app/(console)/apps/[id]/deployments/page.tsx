"use client";

import { useParams } from "next/navigation";
import { AppDeploymentsView } from "@/components/app-deployments-view";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { useApp } from "@/lib/hooks";

/** Project deployment history. CLIENT: the app resolves browser-side
 *  (gateway mode). */
export default function AppDeploymentsPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="pb-12">
      <AppPageHeader app={app} />
      <AppDeploymentsView app={app} />
    </div>
  );
}
