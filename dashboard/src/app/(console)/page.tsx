import type { Metadata } from "next";
import { OverviewView } from "@/components/overview-view";

export const metadata: Metadata = { title: "Projects" };

/** Server wrapper: session data is browser-fetched (see OverviewView). */
export default function AppsOverviewPage() {
  return <OverviewView />;
}
