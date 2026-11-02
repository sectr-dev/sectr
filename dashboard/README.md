This is a [Next.js](https://nextjs.org) project bootstrapped with [`create-next-app`](https://nextjs.org/docs/app/api-reference/cli/create-next-app).

## Local dev

Two scripts; the mode is set by the script, not by `.env.local`:

```bash
npm run dev          # mock mode: fixtures, no backend needed
npm run dev:server   # live mode: the whole local stack
```

In mock mode, click the gray **demo data** badge in the topbar to swap
between the full demo and an empty account (no apps, sessions, API keys
or secrets) for testing empty states. The choice is kept in localStorage
and shows as **demo data · empty**.

`dev:server` runs `scripts/dev-stack.sh`, which starts Postgres (docker,
`localhost:55432`) and a debug `sectr-server` on `:8080` signing in
against the Clerk dev instance, then `next dev` with `/api` rewritten to
it. Ctrl-C stops the server and the dashboard. The full manual test
(deploys, approvals, CLI) is the self-hosting guide in the docs.

Either way, open [http://localhost:3000](http://localhost:3000).
`.env.local` only needs `NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY`, set to the
dev instance's `pk_test_…` key.

## Learn More

To learn more about Next.js, take a look at the following resources:

- [Next.js Documentation](https://nextjs.org/docs) - learn about Next.js features and API.
- [Learn Next.js](https://nextjs.org/learn) - an interactive Next.js tutorial.

You can check out [the Next.js GitHub repository](https://github.com/vercel/next.js) - your feedback and contributions are welcome!

## Deploy on Vercel

The easiest way to deploy your Next.js app is to use the [Vercel Platform](https://vercel.com/new?utm_medium=default-template&filter=next.js&utm_source=create-next-app&utm_campaign=create-next-app-readme) from the creators of Next.js.

Check out our [Next.js deployment documentation](https://nextjs.org/docs/app/building-your-application/deploying) for more details.

## Metrics

PostHog powers the console's in-app analytics + frontend exception capture
(no Sentry). Gated on build-time env: set `NEXT_PUBLIC_POSTHOG_KEY` (and
optionally `NEXT_PUBLIC_POSTHOG_HOST`, default `https://us.i.posthog.com`)
in the Cloudflare Workers Builds environment — unset means nothing loads
and nothing reports. The browser identifies by the org id from `/me`, the
same distinct_id the server's `track()` calls use, so server and client
events correlate in PostHog.

## Clerk sign-in

GitHub / Google sign-in is Clerk, client-side only (no `clerkMiddleware`),
against the prod server. `NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY` (public, baked
at build time) is written into the `build:worker` script in
`package.json`.

Locally, `npm run dev` (mock mode) shows the same login card without Clerk:
the buttons go straight into the mock console, and `/login?beta=full`
previews the waitlist notice. `npm run dev:server` runs the real OAuth
round-trip against the Clerk **development** instance and a local server
(dev-gateway mode has no auth, so no login).
