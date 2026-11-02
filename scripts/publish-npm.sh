#!/usr/bin/env bash
# Publish the npm packages (one tarball per js/ package, from release-build.sh)
# to npm, each at its own version: @sectr/copilotkit at the lockstep version,
# the `sectr` shell at 0.0.0. Skips any name@version already on npm (npm
# rejects re-publishes), so the shell goes out once, with the first release.
# No wait for the registry afterwards: a successful `npm publish` is the
# confirmation, and a new version can take 5+ minutes to show in `npm view`.
# Auth: NPM_TOKEN (env or Bitwarden), handed to npm through
# a temporary userconfig that references it, so the token is never written to
# disk. No --provenance: that needs a CI OIDC identity, and releases run on
# the dev machine. Usage: scripts/publish-npm.sh [VERSION]  (DRY_RUN=1 previews)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"
out=$(artifacts_dir "$ver")
live() { [ "$(npm view "$1@$2" version 2>/dev/null)" = "$2" ]; }

if [ ! -f "$out/npm/sectr-copilotkit-$ver.tgz" ]; then
  [ "$(scripts/version.sh)" = "$ver" ] || die "no npm build for v$ver and this checkout is $(scripts/version.sh) — run: just build (at v$ver)"
  log "no local build; packing the npm packages only"
  mkdir -p "$out/npm"
  for d in js/*/; do
    (cd "$d" && npm ci --no-audit --no-fund && npm run build && npm pack --pack-destination "$out/npm") >&2
  done
fi

pending=()
for tgz in "$out"/npm/*.tgz; do
  name=$(tar xzf "$tgz" -O package/package.json | jq -r .name)
  v=$(tar xzf "$tgz" -O package/package.json | jq -r .version)
  if live "$name" "$v"; then log "$name $v already on npm; skipping"
  else pending+=("$tgz"); fi
done
[ ${#pending[@]} -gt 0 ] || { log "nothing to publish"; exit 0; }

if [ "${DRY_RUN:-}" = 1 ] && [ -z "${NPM_TOKEN:-}" ]; then
  log "dry run: NPM_TOKEN not read"
else
  NPM_TOKEN="$(secret NPM_TOKEN)"
  export NPM_TOKEN
fi
npmrc=$(mktemp); trap 'rm -f "$npmrc"' EXIT
# npm expands ${NPM_TOKEN} from the environment when it reads the file.
# shellcheck disable=SC2016
echo '//registry.npmjs.org/:_authToken=${NPM_TOKEN}' > "$npmrc"

for tgz in "${pending[@]}"; do
  run npm publish "$tgz" --access public --userconfig "$npmrc"
done
