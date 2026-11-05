import { ClerkProvider } from "@clerk/nextjs";
import type { Metadata } from "next";
import { Inter, Geist_Mono } from "next/font/google";
import localFont from "next/font/local";
import Script from "next/script";
import { ClerkBridge } from "@/components/clerk-bridge";
import { GatewayInit } from "@/components/gateway-init";
import { PostHogProvider } from "@/components/posthog-provider";
import { Toaster } from "@/components/ui/sonner";
import { CLERK_ENABLED } from "@/lib/api";
import "./globals.css";

const inter = Inter({
  variable: "--font-inter",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

/* Logo wordmark face — Varino Normal (single weight, self-hosted). */
const varino = localFont({
  src: "../fonts/VarinoNormal.otf",
  variable: "--font-varino",
  display: "swap",
});

/** Applies the stored theme (light/dark/system, localStorage `sectr.theme`)
    before first paint — no light-flash. Runtime half: lib/theme.ts. */
const themeInit = `(function(){try{var t=localStorage.getItem("sectr.theme")||"system";var d=t==="dark"||(t==="system"&&window.matchMedia("(prefers-color-scheme: dark)").matches);var c=document.documentElement.classList;d?c.add("dark"):c.remove("dark");}catch(e){}})();`;

export const metadata: Metadata = {
  title: {
    default: "sectr — console",
    template: "%s · sectr",
  },
  description:
    "Stores, streams and resumes AI agent conversations, with live inspection and human-in-the-loop approvals.",
  /* The console is an app behind sign-in; sectr.dev and docs are the search
     surfaces. */
  robots: { index: false, follow: false },
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  const app = (
    /* In-app analytics + frontend exception capture — a no-op
       wrapper without NEXT_PUBLIC_POSTHOG_KEY. */
    <PostHogProvider>
      <GatewayInit />
      {children}
      <Toaster position="bottom-right" />
    </PostHogProvider>
  );

  return (
    <html
      lang="en"
      suppressHydrationWarning
      className={`${inter.variable} ${geistMono.variable} ${varino.variable} h-full antialiased`}
    >
      <head>
        <script dangerouslySetInnerHTML={{ __html: themeInit }} />
        {/* Sets window.__SECTR_CONFIG__, which lib/api.ts reads when it loads,
            so it has to run before the app does. */}
        <Script src="/runtime-config.js" strategy="beforeInteractive" />
      </head>
      <body className="min-h-full flex flex-col">
        {/* Clerk sign-in, CLIENT-only — a non-`dynamic` provider
            never reads the request, so no clerkMiddleware / server auth()
            (the console renders control-plane data client-side only).
            Prod server only (build:worker requires the publishable key);
            off in mock/dev-gateway modes, and on a self-hosted install, whose
            build has no key. */}
        {CLERK_ENABLED ? (
          <ClerkProvider>
            <ClerkBridge />
            {app}
          </ClerkProvider>
        ) : (
          app
        )}
      </body>
    </html>
  );
}
