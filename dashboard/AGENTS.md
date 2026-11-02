<!-- BEGIN:nextjs-agent-rules -->

# This is NOT the Next.js you know

This version has breaking changes — APIs, conventions, and file structure may all differ from your training data. Read the relevant guide in `node_modules/next/dist/docs/` (resolved from this file's directory; in monorepos the `next` package may not be visible from the repo root) before writing any code. Heed deprecation notices.

This block is written and re-added by `next dev` — verify at `node_modules/next/dist/server/lib/generate-agent-files.js`. Removing it from a diff only re-creates the uncommitted change; committing it with your work keeps the tree clean.

<!-- END:nextjs-agent-rules -->

## Agent notes

- Headless-chromium (playwright-core, headless shell v1243) **crashes the
  renderer** when animating the U+2590 (`▍`) glyph in Geist Mono's
  box-drawing subset — glyph alone or animation alone is fine, the
  combination kills the whole browser. Symptom: `page.goto` → "Target page,
  context or browser has been closed" with no dumpio output, only on pages
  that paint the animated glyph. Rule: terminal carets must be CSS-drawn
  bars (`animate-caret-blink` on an `inline-block w-[2px] bg-current`
  span), never the glyph (fixed in dashboard 2026-09-07).
