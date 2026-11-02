#!/usr/bin/env bash
# Fails if a public justfile recipe is missing from the RELEASING.md command
# table (keeps the runbook from going stale). Usage: scripts/check-releasing-docs.sh
set -euo pipefail
cd "$(dirname "$0")/.."
missing=0
for recipe in $(just --summary); do
  [ "$recipe" = default ] && continue
  if ! grep -qE "\`just $recipe([^a-z-]|\`|$)" RELEASING.md; then
    echo "RELEASING.md command table is missing: just $recipe" >&2
    missing=1
  fi
done
exit $missing
