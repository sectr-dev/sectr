import type { Metadata } from "next";
import { CliCodeEntry } from "@/components/cli-login-view";

export const metadata: Metadata = { title: "Sign in the CLI" };

export default function CliPage() {
  return <CliCodeEntry />;
}
