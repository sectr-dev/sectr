import { renderRuntimeConfig } from "@/lib/runtime-config";

/* Read per request: the value is the running container's, not the build's. */
export const dynamic = "force-dynamic";

export function GET() {
  return new Response(renderRuntimeConfig(process.env.SECTR_PUBLIC_API_URL), {
    headers: {
      "Content-Type": "text/javascript; charset=utf-8",
      "Cache-Control": "no-store",
    },
  });
}
