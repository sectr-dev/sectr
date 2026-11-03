import type { NextConfig } from "next";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Build-time version inlining: package.json carries the lockstep product
// version. Inlined as
// NEXT_PUBLIC_SECTR_VERSION so the console shows what build is live —
// Workers Builds deploys on every push to main, and a semver alone doesn't
// disambiguate builds within one version (a /api/version exposing
// CLOUDFLARE_VERSION_ID is the follow-up if build-level debugging lands).
const pkg = JSON.parse(
  readFileSync(new URL("./package.json", import.meta.url), "utf8"),
) as { version: string };

const nextConfig: NextConfig = {
  // The self-hosted image runs Next's own server. The hosted build goes
  // through OpenNext and must not set this.
  ...(process.env.NEXT_OUTPUT === "standalone"
    ? {
        output: "standalone" as const,
        outputFileTracingRoot: fileURLToPath(new URL(".", import.meta.url)),
      }
    : {}),

  env: {
    NEXT_PUBLIC_SECTR_VERSION: pkg.version,
  },

  // SSE through the /api rewrite must NOT be compressed: when the client
  // sends `Accept-Encoding: gzip` (every browser does; curl doesn't —
  // which is why curl-based probes missed this), Next's gzip layer
  // buffers the proxied chunked stream and the console's journal tail
  // starves silently (verified live 2026-09-14: compress:true kills the
  // journal in the browser while curl keeps streaming). Cost is nil
  // where this config matters: `next dev` and self-hosted Node. The
  // deployed console is Cloudflare Workers, where this setting doesn't
  // apply and edge compression streams properly (spike S0).
  // M1 run finding — do not remove without re-testing SSE in a BROWSER.
  compress: false,
  /**
   * Same-origin proxy to sectr-server: the console's prod base URL
   * is `/api`, and every request — including SSE tails and the Bearer
   * platform key — rides same-origin through this rewrite. No CORS on the
   * Rust side (the shared sessions router stays bare by design).
   *
   * SECTR_SERVER_URL is the deployment's server origin (compose: the
   * sectr-server service; Workers/OpenNext: the public api.sectr.dev).
   *
   * CAVEAT (learned live 2026-09-18): OpenNext BAKES rewrites into the
   * build manifest — SECTR_SERVER_URL must be set at BUILD time, and a
   * runtime Worker var alone does nothing (symptom: `TypeError: Expected
   * "8080" to be a string` in the routingHandler, from the baked
   * localhost default). See build:worker in package.json.
   */
  async rewrites() {
    const target = process.env.SECTR_SERVER_URL ?? "http://localhost:8080";
    return [
      {
        source: "/api/:path*",
        destination: `${target}/:path*`,
      },
    ];
  },
};

export default nextConfig;
