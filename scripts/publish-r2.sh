#!/usr/bin/env bash
# Mirror the CLI release assets to the public R2 bucket (served at
# dl.sectr.dev): <tag>/ always, latest/ only with --latest (never clobber the
# installer sectr.dev/install.sh serves when backfilling an old tag). Then
# install-smoke-check from the real public URL.
# Usage: scripts/publish-r2.sh [--latest] [VERSION]   (uploads the local build's
#   dist/ artifacts; DRY_RUN=1 previews uploads.)
# Needs CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN (the R2 token, a.k.a. CF_R2_TOKEN).
set -euo pipefail
. "$(dirname "$0")/lib.sh"
latest=0; [ "${1:-}" = "--latest" ] && { latest=1; shift; }
ver="${1:-$(scripts/version.sh)}"; tag="v$ver"
dir="$(artifacts_dir "$ver")/dist"
[ -d "$dir" ] || die "asset dir $dir not found (run: just build)"
BUCKET=sectr-releases

need=(CLOUDFLARE_ACCOUNT_ID); [ -n "${CLOUDFLARE_API_TOKEN:-}" ] || need+=(CF_R2_TOKEN)
load_secrets "${need[@]}"
export CLOUDFLARE_API_TOKEN="${CLOUDFLARE_API_TOKEN:-$CF_R2_TOKEN}"
if command -v wrangler >/dev/null; then wr=(wrangler); else wr=(npx --yes wrangler@4); fi

# `wrangler r2 object put` fails on roughly one call in twenty with a bare
# "fetch failed" (no HTTP status, ~350ms in), on a connection that is fine
# otherwise. Each put overwrites, so retrying is safe.
put() { # put <key> <file>
  run retry 4 "${wr[@]}" r2 object put "$BUCKET/$1" --file "$2" --remote || die "upload of $1 failed 4 times"
}

for f in "$dir"/*; do
  [ -f "$f" ] || continue
  name=$(basename "$f")
  put "$tag/$name" "$f"
  if [ $latest = 1 ]; then put "latest/$name" "$f"; fi
done

if [ "${DRY_RUN:-}" = 1 ]; then echo "[dry-run] smoke-install skipped"; exit 0; fi
if [ $latest = 1 ]; then scripts/smoke-install.sh "$ver" latest; fi
scripts/smoke-install.sh "$ver" "$tag"
