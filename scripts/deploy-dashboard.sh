#!/usr/bin/env bash
# Deploy the dashboard Worker that `just build` made and `just verify` checked
# to Cloudflare (app.sectr.dev). Nothing is rebuilt, so what goes live is
# exactly what was verified. The release runs it after deploy-server, so the
# live dashboard never runs ahead of the API it calls. Pushes to main don't
# deploy the dashboard.
# Needs CLOUDFLARE_ACCOUNT_ID and CF_WORKERS_TOKEN (an API token with Workers
# Scripts: Edit on the account), from the environment or Bitwarden.
# Usage: scripts/deploy-dashboard.sh [VERSION]  (DRY_RUN=1 bundles and checks the
# Worker, uploads nothing)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"
url=https://app.sectr.dev/

need_artifacts "$ver"
worker="$(artifacts_dir "$ver")/dashboard"
[ -f "$worker/wrangler.jsonc" ] && [ -f "$worker/.open-next/worker.js" ] \
  || die "no dashboard Worker in the v$ver build at $worker — run: just build"

# wrangler at the dashboard's locked version.
[ -x dashboard/node_modules/.bin/wrangler ] || (cd dashboard && npm ci --no-audit --no-fund >&2)
export WRANGLER_SEND_METRICS=false
deploy=(dashboard/node_modules/.bin/wrangler deploy --config "$worker/wrangler.jsonc"
  --tag "v$ver" --message "sectr v$ver")

if [ "${DRY_RUN:-}" = 1 ]; then
  "${deploy[@]}" --dry-run
  log "dry run: the v$ver dashboard Worker bundles; nothing uploaded"
  exit 0
fi

load_secrets CLOUDFLARE_ACCOUNT_ID CF_WORKERS_TOKEN
CLOUDFLARE_API_TOKEN="$CF_WORKERS_TOKEN" "${deploy[@]}"

for _ in $(seq 1 12); do
  if curl -fsS --max-time 10 -o /dev/null "$url"; then
    log "ok: $url answers on v$ver"
    exit 0
  fi
  log "$url not answering yet — retrying"; sleep 5
done
die "$url didn't answer within a minute of the deploy. To roll back: cd dashboard && npx wrangler rollback"
