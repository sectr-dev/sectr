#!/usr/bin/env bash
# Publish the `sectr` wheel+sdist to PyPI. Skips if this version is already
# there (PyPI rejects re-uploads); polls to confirm. Auth: UV_PUBLISH_TOKEN /
# PYPI_TOKEN (env or Bitwarden). Usage: scripts/publish-sdk.sh [VERSION]  (DRY_RUN=1 previews)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"
live() { [ "$(curl -s -o /dev/null -w '%{http_code}' "https://pypi.org/pypi/sectr/$ver/json")" = 200 ]; }

if live; then log "sectr $ver already on PyPI; nothing to do"; exit 0; fi
out=$(artifacts_dir "$ver")
if [ ! -d "$out/sdk" ]; then
  log "no local build; building the SDK only"
  mkdir -p "$out/sdk"; (cd sdk-python && uv build --out-dir "$out/sdk")
fi

UV_PUBLISH_TOKEN="${UV_PUBLISH_TOKEN:-$(secret PYPI_TOKEN)}"
export UV_PUBLISH_TOKEN
run uv publish "$out"/sdk/*

[ "${DRY_RUN:-}" = 1 ] && exit 0
for _ in $(seq 1 12); do live && { echo "sectr $ver live on PyPI"; exit 0; }; sleep 10; done
die "sectr $ver not visible on PyPI after 2 minutes"
