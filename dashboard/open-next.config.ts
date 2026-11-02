// OpenNext adapter config for Cloudflare Workers.
// No incremental cache (the shell is prerendered/client-side); add an R2
// binding here if ISR/cache lands with the real API in Phase 7.
// See: https://opennext.js.org/cloudflare
import { defineCloudflareConfig } from "@opennextjs/cloudflare";

export default defineCloudflareConfig({});
