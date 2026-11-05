import type { Metadata } from "next";
import Link from "next/link";
import { Logo } from "@/components/logo";
import { LoginForm } from "@/components/login-form";
import { CLERK_ENABLED, PROD_MODE } from "@/lib/api";

export const metadata: Metadata = { title: "Sign in or sign up" };

export default function LoginPage() {
    return (
        <div className="bg-dusk relative flex min-h-svh flex-col items-center justify-center px-4">
            <div className="relative z-10 w-full max-w-md">
                <div className="mb-10 flex flex-col items-center gap-6">
                    <Link href="/">
                        <Logo size="lg" />
                    </Link>
                    {/* One page for both: Continue with GitHub/Google/Vercel/email signs
                        in an existing user or creates the account. A self-hosted
                        install has no accounts: its form takes a platform key. */}
                    {PROD_MODE && !CLERK_ENABLED ? (
                        <div className="space-y-2 text-center">
                            <h1 className="text-2xl font-medium text-foreground">
                                Sign in to Sectr
                            </h1>
                            <p className="text-base text-muted-foreground">
                                Paste a platform key for this server.
                            </p>
                        </div>
                    ) : (
                        <div className="space-y-2 text-center">
                            <h1 className="text-2xl font-medium text-foreground">
                                Get started with Sectr
                            </h1>
                            <p className="text-base text-muted-foreground">
                                New here? Continue with GitHub, Google, Vercel or your email
                                to create your free account.
                            </p>
                        </div>
                    )}
                </div>

                <div className="rounded-xl border border-border bg-surface p-7 shadow-[0_0_24px_rgb(240_154_82/8%),0_8px_30px_var(--shadow-card)]">
                    <LoginForm />
                </div>
            </div>
        </div>
    );
}
