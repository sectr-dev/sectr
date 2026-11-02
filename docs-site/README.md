# docs-site

[docs.sectr.dev](https://docs.sectr.dev) — documentation for the `sectr`
CLI, the Python SDK, and the public API. Starlight (Astro) static build,
deployed as a static-assets-only Cloudflare Worker.

## Commands

```bash
npm install
npm run dev      # local dev server
npm run build    # static build → dist/
npm run preview  # serve the built output
```

Deploy: automatic — the Cloudflare↔GitHub integration (Workers Builds)
builds and deploys on every push to `main` (same model as dashboard/).
`just release` smoke-checks the live site (scripts/smoke-docs.sh). Manual
fallback: `npx wrangler deploy` from this directory.

## Layout

- `src/content/docs/` — the content (Starlight docs collection)
  - `getting-started/` — install → `sectr init` → first agent → deploy
  - `guides/` — local dev, secrets, approvals/HITL, connecting your
    frontend, framework adapters
  - `reference/` — CLI reference, SDK reference, API reference
- `src/styles/custom.css` — brand theme (tokens lifted from marketing/)
- `src/components/starlight/Sidebar.astro` — sidebar override that renders
  the lockstep version footer

## Notes

- **No SSR adapter**: every page is prerendered; the site deploys as pure
  static assets (`wrangler.jsonc`, no Worker code, no bindings).
- **Safari/iOS sidebar fixes** (2026-09-24): Starlight 0.42's popover-based
  mobile sidebar breaks on WebKit in two ways — a ~10px window below the
  800px breakpoint in desktop Safari where the sidebar covers the page
  (em/rem media-query divergence, withastro/starlight#3424), and iOS < 17
  showing the sidebar permanently (no Popover API; iPhone 8+ cannot update
  past iOS 16). Mitigated in `custom.css` plus a vendored
  `public/popover-polyfill.min.js` loaded in `astro.config.mjs` — see
  PLAN.md "WebKit sidebar compat" for details.
- **Versioning**: docs are unversioned (one `latest`, no `/v` paths). The
  `v0.0.x` in the sidebar footer is a lockstep sanity check, not API
  versioning — `scripts/set-version.sh` keeps `package.json` in lockstep
  with the rest of the repo, and the release train proves the live docs
  carry the released version.
- Sandbox note: pagefind's musl binary needs 4K pages; builds in the
  16K-page sandbox must temporarily disable it (see AGENTS.md).