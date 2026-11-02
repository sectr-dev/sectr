import type { Metadata } from "next";
import { CliLoginView } from "@/components/cli-login-view";

export const metadata: Metadata = { title: "Sign in the CLI" };

export default async function CliLoginPage({
  params,
}: {
  params: Promise<{ code: string }>;
}) {
  const { code } = await params;
  return <CliLoginView code={decodeURIComponent(code)} />;
}
