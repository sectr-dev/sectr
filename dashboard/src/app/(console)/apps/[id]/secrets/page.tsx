"use client";

import { useParams } from "next/navigation";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { AppSecretsView } from "@/components/app-secrets-view";
import { useApp } from "@/lib/hooks";

/** Project secrets. CLIENT: the app resolves browser-side (gateway mode). */
export default function AppSecretsPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="pb-12">
      <AppPageHeader app={app} />
      <AppSecretsView app={app} />
    </div>
  );
}
