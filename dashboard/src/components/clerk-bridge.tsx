"use client";

/**
 * Publishes the live Clerk session to lib/clerk-session.ts so
 * non-hook code (AuthGate's 401 re-exchange, the account menu's sign-out)
 * can use it. Mounted inside <ClerkProvider> only; renders nothing.
 */

import { useAuth, useClerk } from "@clerk/nextjs";
import { useEffect } from "react";

import { setClerkBridge } from "@/lib/clerk-session";

export function ClerkBridge() {
    const { isLoaded, isSignedIn, getToken } = useAuth();
    const { signOut } = useClerk();

    useEffect(() => {
        setClerkBridge({
            isLoaded,
            isSignedIn: isSignedIn === true,
            getToken: () => getToken(),
            signOut: (redirectUrl) => signOut({ redirectUrl }),
        });
    }, [isLoaded, isSignedIn, getToken, signOut]);

    useEffect(() => () => setClerkBridge(null), []);

    return null;
}
