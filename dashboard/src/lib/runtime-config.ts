/*
  Settings that a published dashboard image can't have baked in at build time.

  `NEXT_PUBLIC_*` values are fixed when the image is built, but only the person
  running it knows their public URL. `/runtime-config.js` (see
  app/runtime-config.js/route.ts) sets `window.__SECTR_CONFIG__` from the
  container's env, and the root layout loads it before the app. The hosted
  Worker sets nothing, so it gets an empty config and keeps its build-time
  values.

  This module has NO imports, so the API seam can depend on it.
*/

export interface RuntimeConfig {
  /** The server's public URL, as browsers and the CLI reach it. */
  publicApiUrl?: string;
}

declare global {
  interface Window {
    __SECTR_CONFIG__?: RuntimeConfig;
  }
}

/** The body of `/runtime-config.js`. The value comes from the operator's env,
 *  and it's escaped so that no value can break out of the script. */
export function renderRuntimeConfig(publicApiUrl: string | undefined): string {
  const url = publicApiUrl?.trim().replace(/\/+$/, "");
  const config: RuntimeConfig = url ? { publicApiUrl: url } : {};
  const json = JSON.stringify(config)
    .replace(/</g, "\\u003c")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029");
  return `window.__SECTR_CONFIG__ = ${json};\n`;
}

/** The runtime value alone: set on a self-hosted install, otherwise undefined. */
export function configuredPublicApiUrl(): string | undefined {
  if (typeof window === "undefined") return undefined;
  return window.__SECTR_CONFIG__?.publicApiUrl;
}

/** The server's public URL to show people: the runtime value, then the
 *  build-time one, then the hosted server. */
export function publicServerUrl(): string {
  return (
    configuredPublicApiUrl() ??
    process.env.NEXT_PUBLIC_SECTR_SERVER_URL ??
    "https://api.sectr.dev"
  );
}

/** `sectr auth login`, naming this server when it isn't the hosted one, so a
 *  self-hoster can't log in to hosted by accident. Later commands follow the
 *  login, so only this one needs it. */
export function cliLoginCommand(flags?: string): string {
  const url = configuredPublicApiUrl();
  return ["sectr auth login", url && `--api ${url}`, flags]
    .filter(Boolean)
    .join(" ");
}
