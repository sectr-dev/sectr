import type { Metadata } from "next";
import { OrgPageHeader } from "@/components/org-page-header";
import { SettingsView } from "@/components/settings-view";

export const metadata: Metadata = { title: "Settings" };

export default function SettingsPage() {
  return (
    <div className="pb-12">
      <OrgPageHeader title="Settings" />
      <div className="mx-auto max-w-3xl px-6 pt-6">
        <SettingsView />
      </div>
    </div>
  );
}
