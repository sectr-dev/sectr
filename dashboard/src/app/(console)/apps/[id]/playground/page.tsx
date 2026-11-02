"use client";

import { useParams } from "next/navigation";
import { PlaygroundView } from "@/components/playground-view";
import { AppPageSkeleton } from "@/components/app-page-skeleton";
import { AppNotFound, AppPageHeader } from "@/components/app-header";
import { useApp } from "@/lib/hooks";

/** Playground. CLIENT: the app resolves browser-side (gateway mode — the
 *  edge can't reach localhost); the playground streams in the browser too. */
export default function PlaygroundPage() {
  const { id } = useParams<{ id: string }>();
  const { app, loading } = useApp(id);
  if (loading) return <AppPageSkeleton />;
  if (!app) return <AppNotFound id={id} />;

  return (
    <div className="flex h-[calc(100svh-3.5rem)] flex-col overflow-hidden">
      {/* Tabbed page: the header is the app's, identical across tabs.
          The playground fills the remaining viewport exactly — the
          conversation area scrolls internally, the input stays visible. */}
      <div className="shrink-0">
        <AppPageHeader app={app} />
      </div>
      <PlaygroundView app={app} />
    </div>
  );
}
