import { AppShell } from "@/components/app-shell";
import { AuthGate } from "@/components/auth-gate";

export default function ConsoleLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  // The gate is the console's auth choke point: in prod mode nothing
  // inside renders (shell, chrome, or data) without a validated platform
  // key. Dev/mock modes pass through for local console development.
  return (
    <AuthGate>
      <AppShell>{children}</AppShell>
    </AuthGate>
  );
}
