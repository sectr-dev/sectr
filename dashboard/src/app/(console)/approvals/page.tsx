import type { Metadata } from "next";
import { ApprovalInbox } from "@/components/approval-inbox";
import { OrgPageHeader } from "@/components/org-page-header";

export const metadata: Metadata = { title: "Pending Approvals" };

export default function ApprovalsPage() {
  return (
    <div className="pb-12">
      <OrgPageHeader title="Pending Approvals">
        <p className="text-sm text-muted-foreground">
          Sessions paused on a gated tool call, across every project. Once
          you decide, the agent continues from where it paused.
        </p>
      </OrgPageHeader>
      <div className="px-6 pt-6">
        <ApprovalInbox />
      </div>
    </div>
  );
}
