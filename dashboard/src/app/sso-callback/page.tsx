"use client";

/**
 * The OAuth return leg. The login card's GitHub / Google / Vercel buttons
 * send the browser to the provider; Clerk lands it here, where the callback
 * component finishes the sign-in (or, for an unknown user, the sign-in →
 * sign-up transfer — there is no separate sign-up page) and then forwards to
 * /login, which swaps the Clerk session for a platform key.
 */

import { AuthenticateWithRedirectCallback } from "@clerk/nextjs";
import { LoaderCircle } from "lucide-react";
import { useRouter } from "next/navigation";
import { useEffect } from "react";

import { CLERK_ENABLED } from "@/lib/api";
import { markOauthReturn } from "@/lib/next-url";

export default function SsoCallbackPage() {
  const router = useRouter();

  // Clerk off (mock/dev modes, self-hosted): nothing to complete.
  useEffect(() => {
    if (!CLERK_ENABLED) router.replace("/login");
    else markOauthReturn();
  }, [router]);

  if (!CLERK_ENABLED) return null;

  return (
    <div className="flex min-h-svh items-center justify-center">
      <div
        className="flex flex-col items-center gap-3 text-muted-foreground"
        role="status"
        aria-live="polite"
      >
        <LoaderCircle className="size-5 animate-spin" />
        <span className="text-sm">Signing you in…</span>
      </div>
      <AuthenticateWithRedirectCallback
        signInForceRedirectUrl="/login"
        signUpForceRedirectUrl="/login"
      />
      {/* Clerk's bot protection mounts its challenge here during sign-up. */}
      <div id="clerk-captcha" />
    </div>
  );
}
