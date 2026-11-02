"use client";

import { PageHeader } from "@/components/page-header";
import { useAsync } from "@/lib/hooks";
import * as api from "@/lib/api";

/**
 * PageHeader for org-level pages: the breadcrumb is `org › title`, like the
 * Projects page, so it adds the org instead of repeating the title.
 */
export function OrgPageHeader({
  title,
  children,
}: {
  title: string;
  children?: React.ReactNode;
}) {
  const { data: me, loading } = useAsync(api.me, []);
  const org = me?.org.name ?? (loading ? "…" : "Local dev");
  return (
    <PageHeader title={title} breadcrumbs={[{ label: org }, { label: title }]}>
      {children}
    </PageHeader>
  );
}
