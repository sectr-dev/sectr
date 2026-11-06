// @ts-check
// docs.sectr.dev — Starlight (Astro) static build.
//
// Deliberately NO adapter: every page is prerendered so the site deploys as
// pure static assets on a Cloudflare Worker (wrangler.jsonc). Same toolchain
// as the marketing site (Astro 7, Tailwind v4 via @tailwindcss/vite) — one toolchain
// across the two public web properties.
//
// The lockstep version rides in package.json (set-version.sh treats it as
// the 5th manifest). It is injected into Starlight config below (build-time
// read) and rendered as the sidebar version footer. It is NOT API
// versioning and implies nothing about compatibility — it exists so the
// release train can prove the live docs are the latest build.
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
    site: "https://docs.sectr.dev",
    integrations: [
        starlight({
            title: "Sectr",
            description: "Stores, streams and resumes AI agent conversations.",
            favicon: "/favicon.svg",
            // The beta Discord, via the sectr.dev/discord redirect
            // (never a raw invite — the invite rotates in one place).
            social: [
                {
                    icon: "discord",
                    label: "Discord",
                    href: "https://sectr.dev/discord",
                },
            ],
            customCss: ["./src/styles/custom.css"],
            // The SiteTitle renders the configured `title` in the Varino
            // Normal wordmark (all caps, single weight) via custom.css —
            // same font as the marketing site (Inter + Geist Mono + Varino).
            // Inter + Geist Mono from the Google Fonts CDN.
            head: [
                // Safari < 17 (e.g. iPhone 8+ / iOS 16, which cannot update
                // further) has no Popover API, and Starlight 0.42's mobile
                // sidebar is a `[popover]` element: without support the UA
                // never applies display:none, so the fixed full-width sidebar
                // pane permanently covers the content and the hamburger is
                // dead. Vendored oddbird/popover-polyfill@0.7.2 (jsDelivr
                // dist, feature-detected — a no-op where the Popover API
                // exists). Loaded synchronously in head so it applies before
                // Starlight's sidebar custom-element script runs in the body.
                // See custom.css (sidebar pane hardening) for the related
                // WebKit em/rem breakpoint bug this complements.
                {
                    tag: "script",
                    attrs: { src: "/popover-polyfill.min.js" },
                },
                {
                    tag: "link",
                    attrs: { rel: "apple-touch-icon", href: "/apple-touch-icon.png" },
                },
                {
                    tag: "link",
                    attrs: {
                        rel: "preconnect",
                        href: "https://fonts.googleapis.com",
                    },
                },
                {
                    tag: "link",
                    attrs: {
                        rel: "preconnect",
                        href: "https://fonts.gstatic.com",
                        crossorigin: "true",
                    },
                },
                {
                    tag: "link",
                    attrs: {
                        rel: "stylesheet",
                        href: "https://fonts.googleapis.com/css2?family=Geist+Mono:wght@400;500;600&family=Inter:wght@400;500;600;700&display=swap",
                    },
                },
            ],
            // Swap in the sidebar override that renders the lockstep
            // version footer (see src/components/starlight/Sidebar.astro).
            components: {
                Sidebar: "./src/components/starlight/Sidebar.astro",
                // Top-left "SECTR / Docs" breadcrumb (wordmark → sectr.dev,
                // Docs → docs root); see the override's header.
                SiteTitle: "./src/components/starlight/SiteTitle.astro",
                // WebKit em/rem fix: Starlight's PageFrame JS strips the sidebar
                // pane's popover attribute at `matchMedia('(min-width: 50em)')`
                // while its CSS reveals the pane at `50rem`; the mismatch leaves a
                // sliver below 800px in Safari where the hamburger is dead. Our
                // override aligns the JS threshold to the CSS `50rem` — see the
                // override's file header for the full story.
                PageFrame: "./src/components/starlight/PageFrame.astro",
            },
            sidebar: [
                {
                    label: "Getting started",
                    items: [
                        {
                            label: "Installing the CLI",
                            link: "/getting-started/install/",
                        },
                        {
                            label: "Your first agent",
                            link: "/getting-started/first-agent/",
                        },
                        {
                            label: "Using the OpenAI Agents SDK",
                            link: "/getting-started/openai-agents/",
                        },
                        {
                            label: "Using LangGraph",
                            link: "/getting-started/langgraph/",
                        },
                        {
                            label: "Using Pydantic AI",
                            link: "/getting-started/pydantic-ai/",
                        },
                        {
                            label: "Connecting your frontend",
                            link: "/getting-started/connecting-frontend/",
                        },
                    ],
                },
                {
                    label: "CLI reference",
                    collapsed: false,
                    items: [{ label: "Commands", link: "/reference/cli/" }],
                },
                {
                    label: "SDK reference",
                    collapsed: false,
                    items: [
                        { label: "Overview", link: "/reference/sdk/" },
                        { label: "AgentApp", link: "/reference/sdk/app/" },
                        {
                            label: "Framework adapters",
                            items: [
                                {
                                    label: "Overview",
                                    link: "/reference/sdk/adapters/",
                                },
                                {
                                    label: "OpenAI Agents SDK",
                                    link: "/reference/sdk/adapters/openai-agents/",
                                },
                                {
                                    label: "LangGraph",
                                    link: "/reference/sdk/adapters/langgraph/",
                                },
                                {
                                    label: "Pydantic AI",
                                    link: "/reference/sdk/adapters/pydantic-ai/",
                                },
                            ],
                        },
                        // { label: "Tools", link: "/reference/sdk/tools/" },
                        // Writing the agent loop yourself (reference/sdk/no-framework/).
                        // Underdeveloped: comment this group out to hide it.
                        {
                            label: "Without a framework",
                            collapsed: true,
                            items: [
                                {
                                    label: "Overview",
                                    link: "/reference/sdk/no-framework/",
                                },
                                {
                                    label: "SessionContext",
                                    link: "/reference/sdk/no-framework/context/",
                                },
                            ],
                        },
                    ],
                },
                {
                    label: "Guided Topics",
                    items: [
                        { label: "Local dev", link: "/guides/local-dev/" },
                        { label: "Secrets", link: "/guides/secrets/" },
                        {
                            label: "Approvals (human in the loop)",
                            link: "/guides/approvals/",
                        },
                        { label: "Self-hosting", link: "/guides/self-hosting/" },
                        // {
                        //     label: "Choosing a framework",
                        //     link: "/guides/framework-adapters/",
                        // },
                    ],
                },
                {
                    label: "API reference",
                    collapsed: false,
                    items: [
                        { label: "Overview", link: "/reference/api/" },
                        {
                            label: "Authentication",
                            link: "/reference/api/auth/",
                        },
                        {
                            label: "Send a message",
                            link: "/reference/api/send-message/",
                        },
                        {
                            label: "Stream a session",
                            link: "/reference/api/stream/",
                        },
                        {
                            label: "Approve or deny actions",
                            link: "/reference/api/approvals/",
                        },
                        {
                            label: "AG-UI endpoint",
                            link: "/reference/api/ag-ui/",
                        },
                        {
                            label: "Limits",
                            link: "/reference/api/limits/",
                        },
                        // {
                        //     label: "Deploy lifecycle",
                        //     link: "/reference/api/deploy-lifecycle/",
                        // },
                        {
                            label: "Full API Spec",
                            link: "/reference/api-spec/",
                        },
                    ],
                },
            ],
        }),
    ],
    vite: {
        plugins: [tailwindcss()],
    },
});
