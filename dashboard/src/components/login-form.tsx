"use client";

/**
 * The login card: "Continue with GitHub / Google / Vercel", or an emailed
 * 6-digit code.
 *
 * Prod server: Clerk authenticates the human, then the Clerk session JWT
 * is swapped (`POST /auth/clerk`) for a short-lived, dashboard-only
 * platform key stored in localStorage — so the AuthGate, every transport
 * and `GET /me` work off that key. Unknown users are handled by Clerk's
 * sign-in → sign-up transfer in the SSO callback, so there is no separate
 * sign-up page.
 *
 * Mock mode (`npm run dev`): the same card, minus Clerk — the buttons go
 * straight into the mock console (there is no auth to perform), and
 * `?beta=full` previews the waitlist notice. The OAuth round-trip itself
 * is only exercised against the real deployment. (Dev-gateway mode never
 * routes here: the gateway has no auth.)
 */

import { useAuth, useClerk } from "@clerk/nextjs";
import { useSignIn, useSignUp } from "@clerk/nextjs/legacy";
import { isClerkAPIResponseError } from "@clerk/nextjs/errors";
import { LoaderCircle } from "lucide-react";
import { useRouter } from "next/navigation";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";

import * as api from "@/lib/api";
import { rememberNext, takeNext } from "@/lib/next-url";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

/** The marketing site's full-page waitlist / application form (the same
 *  link the `beta_full` message gives). */
const WAITLIST_URL = "https://sectr.dev/alpha";

export function LoginForm() {
  if (api.CLERK_ENABLED) return <ClerkLogin />;
  return api.PROD_MODE ? <KeyLogin /> : <MockLogin />;
}

type Strategy = "oauth_github" | "oauth_google" | "oauth_vercel";

const subscribeNever = () => () => {};

/** After a beta_full refusal the Clerk session is ended with a redirect
 *  back here; the query param carries the notice across that reload. */
function useBetaFullParam(): boolean {
  return useSyncExternalStore(
    subscribeNever,
    () => new URLSearchParams(window.location.search).get("beta") === "full",
    () => false,
  );
}

// ---------------------------------------------------------------------------
// Mock mode

function MockLogin() {
  const router = useRouter();
  const betaFull = useBetaFullParam();
  useEffect(rememberNext, []);
  return (
    <LoginCard
      ready
      betaFull={betaFull}
      onStart={() => router.push(takeNext())}
      emailStep={<MockEmailStep onDone={() => router.push(takeNext())} />}
    />
  );
}

// ---------------------------------------------------------------------------
// A pasted platform key (prod, no Clerk key in the build)

function KeyLogin() {
  const router = useRouter();
  const [key, setKey] = useState("");
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(rememberNext, []);

  // Already signed in: a stored key that still works skips the form.
  useEffect(() => {
    if (!api.getPlatformKey()) return;
    void api.me().then(
      () => router.replace(takeNext()),
      () => {},
    );
  }, [router]);

  async function submit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const value = key.trim();
    if (!value || checking) return;
    setChecking(true);
    setError(null);
    try {
      await api.login(value);
      router.replace(takeNext());
    } catch (err) {
      // 403: a real key of the wrong kind, such as an invocation key.
      const rejected =
        err instanceof api.ApiError && (err.status === 401 || err.status === 403);
      setError(
        rejected
          ? "That key isn't valid. It should be a platform key, starting with skp_."
          : `Couldn't check that key — ${err instanceof Error ? err.message : "unknown error"}`,
      );
      setChecking(false);
    }
  }

  return (
    <form onSubmit={(e) => void submit(e)} className="space-y-3">
      <Input
        type="password"
        autoComplete="off"
        autoFocus
        spellCheck={false}
        placeholder="skp_…"
        aria-label="Platform key"
        aria-invalid={error ? true : undefined}
        className="h-11 font-mono text-base"
        value={key}
        disabled={checking}
        onChange={(e) => setKey(e.target.value)}
      />
      {error && <p className="text-sm text-danger">{error}</p>}
      <Button
        type="submit"
        size="lg"
        className="h-11 w-full text-base"
        disabled={checking || !key.trim()}
      >
        {checking && <LoaderCircle className="animate-spin" />}
        Sign in
      </Button>
      <p className="text-center text-xs text-faint-foreground">
        <code className="font-mono">setup.sh</code> printed your first key. You
        can make more in Settings.
      </p>
    </form>
  );
}

// ---------------------------------------------------------------------------
// Clerk (prod)

function ClerkLogin() {
  const router = useRouter();
  const { isLoaded, isSignedIn, getToken } = useAuth();
  const { signOut } = useClerk();
  const { isLoaded: signInLoaded, signIn, setActive } = useSignIn();
  const { isLoaded: signUpLoaded, signUp } = useSignUp();
  const [signingIn, setSigningIn] = useState(false);
  const [refused, setRefused] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // One exchange per mount: React strict mode double-runs effects, and a
  // second concurrent exchange could race the first's org creation.
  const started = useRef(false);

  const betaFull = useBetaFullParam() || refused;

  // `?next=` survives the OAuth round trip in sessionStorage.
  useEffect(rememberNext, []);

  // Signed in with Clerk (fresh OAuth return, or a revisit): swap the
  // session for a platform key — unless the stored key is still good.
  useEffect(() => {
    if (!isLoaded || !isSignedIn || started.current) return;
    started.current = true;
    setSigningIn(true);
    void (async () => {
      try {
        if (api.getPlatformKey()) {
          try {
            await api.me();
            router.replace(takeNext());
            return;
          } catch {
            // Stale/expired key: fall through to a fresh exchange.
          }
        }
        const token = await getToken();
        if (!token) throw new Error("Clerk returned no session token");
        const result = await api.exchangeClerkSession(token);
        await api.login(result.key);
        router.replace(takeNext());
      } catch (err) {
        if (err instanceof api.ClerkExchangeError && err.betaFull) {
          setRefused(true);
          setSigningIn(false);
          // Not half signed in: end the Clerk session too.
          await signOut({ redirectUrl: "/login?beta=full" });
          return;
        }
        setError(
          err instanceof Error ? `Sign-in failed — ${err.message}` : "Sign-in failed.",
        );
        setSigningIn(false);
      }
    })();
  }, [isLoaded, isSignedIn, getToken, signOut, router]);

  function start(strategy: Strategy) {
    if (!signInLoaded) return;
    setError(null);
    void signIn
      .authenticateWithRedirect({
        strategy,
        redirectUrl: "/sso-callback",
        redirectUrlComplete: "/login",
      })
      .catch((err: unknown) =>
        setError(err instanceof Error ? err.message : "Couldn't start sign-in."),
      );
  }

  if (signingIn) {
    return (
      <div
        className="flex flex-col items-center gap-3 py-6 text-muted-foreground"
        role="status"
        aria-live="polite"
      >
        <LoaderCircle className="size-5 animate-spin" />
        <span className="text-base">Signing you in…</span>
      </div>
    );
  }

  return (
    <LoginCard
      ready={signInLoaded}
      betaFull={betaFull}
      error={error}
      onStart={start}
      emailStep={
        <EmailStep
          ready={signInLoaded && signUpLoaded}
          signIn={signIn}
          signUp={signUp}
          setActive={setActive}
        />
      }
      captcha
      onSwitchAccount={
        isSignedIn ? () => void signOut({ redirectUrl: "/login" }) : undefined
      }
    />
  );
}

// ---------------------------------------------------------------------------
// The card itself (shared by both modes)

function LoginCard({
  ready,
  betaFull,
  error,
  onStart,
  emailStep,
  captcha,
  onSwitchAccount,
}: {
  ready: boolean;
  betaFull: boolean;
  error?: string | null;
  onStart(strategy: Strategy): void;
  emailStep: React.ReactNode;
  /** Mount Clerk's bot-protection (Turnstile) target — prod only. */
  captcha?: boolean;
  /** Shown with an error while a Clerk session is live. */
  onSwitchAccount?: () => void;
}) {
  return (
    <div className="space-y-4">
      {betaFull && (
        <p className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-600 dark:text-amber-400">
          Sectr&apos;s beta is full.{" "}
          <a
            href={WAITLIST_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="underline underline-offset-2"
          >
            Join the waitlist →
          </a>
        </p>
      )}
      {error && (
        <div className="space-y-2">
          <p className="rounded-md border border-danger/30 bg-danger/10 px-3 py-2 text-sm text-danger">
            {error}
          </p>
          {onSwitchAccount && (
            <Button
              type="button"
              variant="outline"
              className="w-full"
              onClick={onSwitchAccount}
            >
              Use a different account
            </Button>
          )}
        </div>
      )}

      <div className="space-y-2">
        <Button
          type="button"
          variant="outline"
          size="lg" className="h-11 w-full text-base [&_svg:not([class*='size-'])]:size-5"
          disabled={!ready}
          onClick={() => onStart("oauth_github")}
        >
          <GitHubMark />
          Continue with GitHub
        </Button>
        <Button
          type="button"
          variant="outline"
          size="lg" className="h-11 w-full text-base [&_svg:not([class*='size-'])]:size-5"
          disabled={!ready}
          onClick={() => onStart("oauth_google")}
        >
          <GoogleMark />
          Continue with Google
        </Button>
        <Button
          type="button"
          variant="outline"
          size="lg" className="h-11 w-full text-base [&_svg:not([class*='size-'])]:size-5"
          disabled={!ready}
          onClick={() => onStart("oauth_vercel")}
        >
          <VercelMark />
          Continue with Vercel
        </Button>
      </div>

      <div className="flex items-center gap-3 text-xs text-faint-foreground">
        <div className="h-px flex-1 bg-border" />
        or via email
        <div className="h-px flex-1 bg-border" />
      </div>

      {emailStep}

      <p className="text-center text-xs text-faint-foreground">
        Free during beta. By continuing you agree to the{" "}
        <a
          href="https://sectr.dev/terms/"
          target="_blank"
          rel="noopener noreferrer"
          className="underline underline-offset-2 hover:text-muted-foreground"
        >
          Terms
        </a>{" "}
        and{" "}
        <a
          href="https://sectr.dev/privacy/"
          target="_blank"
          rel="noopener noreferrer"
          className="underline underline-offset-2 hover:text-muted-foreground"
        >
          Privacy Policy
        </a>
        .
      </p>
      {captcha && <div id="clerk-captcha" />}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Email code: sign in, or sign up if the email is unknown. Both
// end in the same "enter the code" step, then `setActive` hands the session
// to ClerkLogin's existing exchange effect.

type Phase = "enter-email" | "sending" | "enter-code" | "verifying";
type Mode = "signIn" | "signUp";

const RESEND_COOLDOWN_S = 30;

function clerkMessage(err: unknown): { code?: string; message: string } {
  if (isClerkAPIResponseError(err) && err.errors[0]) {
    const e = err.errors[0];
    return { code: e.code, message: e.longMessage ?? e.message };
  }
  return { message: err instanceof Error ? err.message : "Something went wrong." };
}

function EmailStep({
  ready,
  signIn,
  signUp,
  setActive,
}: {
  ready: boolean;
  signIn: ReturnType<typeof useSignIn>["signIn"];
  signUp: ReturnType<typeof useSignUp>["signUp"];
  setActive: ReturnType<typeof useSignIn>["setActive"];
}) {
  const [phase, setPhase] = useState<Phase>("enter-email");
  const [mode, setMode] = useState<Mode>("signIn");
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [expired, setExpired] = useState(false);
  const [cooldown, setCooldown] = useState(0);
  const codeRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (cooldown <= 0) return;
    const t = setTimeout(() => setCooldown((c) => c - 1), 1000);
    return () => clearTimeout(t);
  }, [cooldown]);

  /** Send (or re-send) the code for `m`; the resources already exist. */
  async function prepare(m: Mode) {
    if (!signIn || !signUp) return;
    if (m === "signIn") {
      const factor = signIn.supportedFirstFactors?.find(
        (f) => f.strategy === "email_code",
      );
      if (!factor || !("emailAddressId" in factor)) {
        throw new Error("Email sign-in isn't available right now.");
      }
      await signIn.prepareFirstFactor({
        strategy: "email_code",
        emailAddressId: factor.emailAddressId,
      });
    } else {
      await signUp.prepareEmailAddressVerification({ strategy: "email_code" });
    }
    setCooldown(RESEND_COOLDOWN_S);
  }

  async function submitEmail(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (!signIn || !signUp || phase !== "enter-email") return;
    setError(null);
    setPhase("sending");
    const address = email.trim();
    try {
      let m: Mode = "signIn";
      try {
        await signIn.create({ identifier: address });
      } catch (err) {
        if (clerkMessage(err).code !== "form_identifier_not_found") throw err;
        m = "signUp";
        await signUp.create({ emailAddress: address });
      }
      await prepare(m);
      setMode(m);
      setCode("");
      setExpired(false);
      setPhase("enter-code");
      setTimeout(() => codeRef.current?.focus(), 0);
    } catch (err) {
      setError(clerkMessage(err).message);
      setPhase("enter-email");
    }
  }

  async function verify(value: string) {
    if (!signIn || !signUp || phase !== "enter-code") return;
    setError(null);
    setPhase("verifying");
    try {
      const res =
        mode === "signIn"
          ? await signIn.attemptFirstFactor({ strategy: "email_code", code: value })
          : await signUp.attemptEmailAddressVerification({ code: value });
      if (res.status === "complete" && res.createdSessionId) {
        // The login card's signed-in effect takes over from here.
        await setActive?.({ session: res.createdSessionId });
        return;
      }
      if (res.status === "missing_requirements") {
        // Operator misconfiguration: the instance asks for more than an email.
        console.error(
          "Clerk sign-up needs more than an email:",
          signUp.missingFields,
        );
        setError("Sign-up isn't available right now.");
      } else {
        setError("Couldn't finish signing in. Try again.");
      }
      setPhase("enter-code");
    } catch (err) {
      const { code: errCode, message } = clerkMessage(err);
      if (errCode === "form_code_incorrect") {
        setError("That code isn't right. Check the email and try again.");
        setTimeout(() => codeRef.current?.select(), 0);
      } else if (errCode === "verification_expired") {
        setError("That code expired.");
        setExpired(true);
      } else {
        setError(message);
      }
      setPhase("enter-code");
    }
  }

  async function resend() {
    setError(null);
    setExpired(false);
    try {
      await prepare(mode);
    } catch (err) {
      setError(clerkMessage(err).message);
    }
  }

  function changeEmail() {
    setPhase("enter-email");
    setError(null);
    setExpired(false);
    setCode("");
  }

  if (phase === "enter-email" || phase === "sending") {
    return (
      <form onSubmit={(e) => void submitEmail(e)} className="space-y-2">
        <Input
          type="email"
          autoComplete="email"
          placeholder="you@example.com"
          aria-label="Email address"
          aria-invalid={error ? true : undefined}
          className="h-11 text-base"
          value={email}
          disabled={!ready || phase === "sending"}
          onChange={(e) => setEmail(e.target.value)}
        />
        {error && <p className="text-sm text-danger">{error}</p>}
        <Button
          type="submit"
          variant="outline"
          size="lg"
          className="h-11 w-full text-base"
          disabled={!ready || phase === "sending" || !email.trim() || !/^\S+@\S+$/.test(email.trim())}
        >
          {phase === "sending" && <LoaderCircle className="animate-spin" />}
          Continue with email
        </Button>
      </form>
    );
  }

  const busy = phase === "verifying";
  return (
    <div className="space-y-2">
      <p className="text-sm text-muted-foreground">
        We sent a 6-digit code to{" "}
        <strong className="font-medium text-foreground">{email.trim()}</strong>.
      </p>
      <Input
        ref={codeRef}
        inputMode="numeric"
        autoComplete="one-time-code"
        maxLength={6}
        placeholder="123456"
        aria-label="6-digit code"
        aria-invalid={error ? true : undefined}
        className="h-11 text-center text-base tracking-[0.4em]"
        value={code}
        disabled={busy}
        onChange={(e) => {
          const v = e.target.value.replace(/\D/g, "").slice(0, 6);
          setCode(v);
          if (v.length === 6) void verify(v);
        }}
      />
      {error && <p className="text-sm text-danger">{error}</p>}
      {busy && (
        <p className="flex items-center gap-2 text-sm text-muted-foreground" role="status">
          <LoaderCircle className="size-4 animate-spin" /> Checking…
        </p>
      )}
      <div className="flex items-center justify-between text-sm">
        <button
          type="button"
          className="text-muted-foreground underline underline-offset-2 hover:text-foreground disabled:no-underline disabled:opacity-60"
          disabled={busy || (cooldown > 0 && !expired)}
          onClick={() => void resend()}
        >
          {cooldown > 0 && !expired ? `Resend in ${cooldown} s` : "Resend code"}
        </button>
        <button
          type="button"
          className="text-muted-foreground underline underline-offset-2 hover:text-foreground"
          disabled={busy}
          onClick={changeEmail}
        >
          Use a different email
        </button>
      </div>
    </div>
  );
}

/** Mock-mode walk-through: any email → code step; `000000` is "incorrect". */
function MockEmailStep({ onDone }: { onDone: () => void }) {
  const [email, setEmail] = useState("");
  const [sent, setSent] = useState(false);
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);

  if (!sent) {
    return (
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          setSent(true);
        }}
      >
        <Input
          type="email"
          autoComplete="email"
          placeholder="you@example.com"
          aria-label="Email address"
          className="h-11 text-base"
          value={email}
          onChange={(e) => setEmail(e.target.value)}
        />
        <Button
          type="submit"
          variant="outline"
          size="lg"
          className="h-11 w-full text-base"
          disabled={!email.trim() || !/^\S+@\S+$/.test(email.trim())}
        >
          Continue with email
        </Button>
      </form>
    );
  }
  return (
    <div className="space-y-2">
      <p className="text-sm text-muted-foreground">
        We sent a 6-digit code to{" "}
        <strong className="font-medium text-foreground">{email.trim()}</strong>.
      </p>
      <Input
        inputMode="numeric"
        autoComplete="one-time-code"
        maxLength={6}
        placeholder="123456"
        aria-label="6-digit code"
        aria-invalid={error ? true : undefined}
        className="h-11 text-center text-base tracking-[0.4em]"
        value={code}
        onChange={(e) => {
          const v = e.target.value.replace(/\D/g, "").slice(0, 6);
          setCode(v);
          if (v.length < 6) return;
          if (v === "000000") setError("That code isn't right. Check the email and try again.");
          else onDone();
        }}
      />
      {error && <p className="text-sm text-danger">{error}</p>}
      <button
        type="button"
        className="text-sm text-muted-foreground underline underline-offset-2 hover:text-foreground"
        onClick={() => {
          setSent(false);
          setCode("");
          setError(null);
        }}
      >
        Use a different email
      </button>
    </div>
  );
}


// lucide dropped brand icons; these are the official single-path marks.

function GitHubMark() {
  return (
    <svg viewBox="0 0 24 24" className="size-5" fill="currentColor" aria-hidden="true">
      <path d="M12 .5C5.65.5.5 5.65.5 12c0 5.08 3.29 9.39 7.86 10.91.58.1.79-.25.79-.56v-2c-3.2.7-3.87-1.54-3.87-1.54-.52-1.33-1.28-1.69-1.28-1.69-1.05-.71.08-.7.08-.7 1.15.08 1.76 1.19 1.76 1.19 1.03 1.76 2.7 1.25 3.36.96.1-.75.4-1.25.73-1.54-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.28 1.19-3.09-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.78 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.83 1.19 3.09 0 4.42-2.69 5.39-5.25 5.68.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 23.5 12C23.5 5.65 18.35.5 12 .5Z" />
    </svg>
  );
}

function VercelMark() {
  return (
    <svg viewBox="0 0 24 24" className="size-5" fill="currentColor" aria-hidden="true">
      <path d="M12 2 23 21H1L12 2Z" />
    </svg>
  );
}

function GoogleMark() {
  return (
    <svg viewBox="0 0 24 24" className="size-5" aria-hidden="true">
      <path
        fill="#4285F4"
        d="M23.52 12.27c0-.85-.08-1.67-.22-2.45H12v4.64h6.46a5.52 5.52 0 0 1-2.4 3.62v3h3.88c2.27-2.09 3.58-5.17 3.58-8.81Z"
      />
      <path
        fill="#34A853"
        d="M12 24c3.24 0 5.96-1.07 7.94-2.91l-3.88-3c-1.07.72-2.45 1.15-4.06 1.15-3.13 0-5.78-2.11-6.73-4.96H1.27v3.09A12 12 0 0 0 12 24Z"
      />
      <path
        fill="#FBBC05"
        d="M5.27 14.28A7.2 7.2 0 0 1 4.9 12c0-.79.14-1.56.37-2.28V6.63H1.27a12 12 0 0 0 0 10.74l4-3.09Z"
      />
      <path
        fill="#EA4335"
        d="M12 4.75c1.76 0 3.34.61 4.59 1.8l3.44-3.44C17.95 1.19 15.24 0 12 0A12 12 0 0 0 1.27 6.63l4 3.09C6.22 6.86 8.87 4.75 12 4.75Z"
      />
    </svg>
  );
}
