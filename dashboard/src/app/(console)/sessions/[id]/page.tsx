import type { Metadata } from "next";
import { SessionInspector } from "@/components/session-inspector";
import { RuntimeSessionView } from "@/components/runtime-session-view";
import * as api from "@/lib/api";
import { getSession, listApps } from "@/lib/api";
import { toUiSession } from "@/lib/view";

export function generateStaticParams() {
  // Mock-shell fixtures only; live modes resolve client-side (no SSG —
  // session ids are runtime data).
  return process.env.NEXT_PUBLIC_SECTR_API
    ? []
    : [
        { id: "7c62c9a1-165b-4386-aebd-6647b35e472f" },
        { id: "95d6d7ac-d4e1-4f31-86ea-233a57996bed" },
        { id: "dffb03f5-2e6f-4fea-9fb3-d0b897b7656d" },
        { id: "91a8fa09-1e82-4e85-bdd7-a9e62b36c5e9" },
        { id: "db814059-43a3-4399-a9ca-084766f2361c" },
        { id: "6771f464-27f9-4a07-b6fa-334f8e0f4658" },
        { id: "b5b6fcc0-f3e2-4fb3-a532-bbe3bda75fae" },
        { id: "d20a9332-3b00-4df8-8a8b-e7621391b5d2" },
      ];
}

export async function generateMetadata({
  params,
}: PageProps<"/sessions/[id]">): Promise<Metadata> {
  const { id } = await params;
  return { title: id };
}

export default async function SessionPage({
  params,
}: PageProps<"/sessions/[id]">) {
  const { id } = await params;
  // Session reads are CLIENT-side in live modes: a dev-gateway base is
  // unreachable from the edge, and a prod fetch can't use the relative
  // /api rewrite URL server-side (fetch("/api/…") throws Invalid URL on
  // Node) nor carry the credential seam. Server components serve MOCK
  // fixtures only; every live mode resolves through the client view.
  if (api.MOCK_MODE) {
    const [session, apps] = await Promise.all([getSession(id), listApps()]);
    if (session) {
      // No PageHeader here on purpose: the inspector's meta bar is the header —
      // the session fills the viewport (minus topbar) exactly, so the log and
      // payload panels scroll internally instead of scrolling the page.
      return <SessionInspector session={toUiSession(session, apps)} />;
    }
  }
  // Mock: unknown id (created in another browser). Live modes: always.
  return <RuntimeSessionView id={id} />;
}
