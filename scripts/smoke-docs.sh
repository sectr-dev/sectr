#!/usr/bin/env bash
# Poll docs.sectr.dev until the sidebar footer shows vVERSION. docs deploy via
# the Cloudflare Workers Builds integration on the push to main, so this is a
# smoke check that catches a stale-build deploy (up to ~5 min of retries).
# Usage: scripts/smoke-docs.sh VERSION
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:?version}"
if ! curl -fsS --max-time 10 https://docs.sectr.dev/ -o /dev/null; then
  echo "docs.sectr.dev not reachable — docs smoke check skipped"
  exit 0
fi
for _ in $(seq 1 30); do
  if curl -fsS https://docs.sectr.dev/ | grep -qF "v$ver"; then
    echo "ok: live docs show v$ver"; exit 0
  fi
  echo "v$ver not live yet — retrying (Cloudflare Builds)"; sleep 10
done
die "live docs did not show v$ver within 5 minutes"
