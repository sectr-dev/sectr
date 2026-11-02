# docs-site agent notes

- pagefind's `@pagefind/linux-arm64` binary aborts on systems with 16K pages
  (`<jemalloc>: Unsupported system page size`). For a local docs build there,
  temporarily set `pagefind: false` in astro.config.mjs (do NOT commit that).
