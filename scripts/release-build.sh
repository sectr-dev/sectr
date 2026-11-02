#!/usr/bin/env bash
# Build every release artifact locally into target/release-artifacts/vX.Y.Z/
#   dist/    CLI tarballs + checksums + installer + dist-manifest.json (dist)
#   server/  sectr-server (aarch64 glibc 2.34, --features e2b)
#   sdk/     sectr wheel + sdist
#   npm/     one tarball per js/ package (sectr-copilotkit-<ver>.tgz, sectr-0.0.0.tgz)
#   image.txt  local runner-base image tag (linux/amd64)
# Usage: scripts/release-build.sh [--no-image]
# Built once; publish-* / deploy-server reuse these. BUILT_FROM_SHA records
# the commit so the release gate can check they match the tag.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

build_image=1
[ "${1:-}" = "--no-image" ] && build_image=0

ver=$(scripts/version.sh)
out=$(artifacts_dir "$ver")
rm -rf "$out"; mkdir -p "$out"/{dist,server,sdk,npm}
command -v dist >/dev/null || die "dist not installed (see RELEASING.md)"
command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild not installed (brew install zig; cargo install cargo-zigbuild --locked)"

case "$(uname -s)" in
  Darwin) targets=(aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu) ;;
  Linux)  targets=(aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu) ;; # macOS targets need a Mac
  *) die "unsupported host $(uname -s)" ;;
esac

log "CLI via dist (${targets[*]})"
# Start clean: dist ignores (with a warning) but never deletes old output, and
# we copy everything in target/distrib into the artifact dir.
rm -rf target/distrib
targs=(); for t in "${targets[@]}"; do targs+=(--target="$t"); done
dist build --artifacts=all "${targs[@]}" >&2
# Temp file: dist reads any existing target/distrib/dist-manifest.json, and a
# shell redirect straight onto it would truncate it first.
dist manifest --artifacts=all --output-format=json --no-local-paths "${targs[@]}" > target/dist-manifest.tmp
mv target/dist-manifest.tmp target/distrib/dist-manifest.json
# Only this build's files (target/distrib may hold stale ones).
for f in target/distrib/*; do
  [ -f "$f" ] && cp "$f" "$out/dist/"
done

log "sectr-server (aarch64, glibc 2.34, e2b)"
rustup target add aarch64-unknown-linux-gnu >&2
cargo zigbuild --release -p sectr-server --features e2b --target aarch64-unknown-linux-gnu.2.34 >&2
cp target/aarch64-unknown-linux-gnu/release/sectr-server "$out/server/"

log "SDK wheel + sdist"
(cd sdk-python && uv build --out-dir "$out/sdk" >&2)

log "npm packages"
for d in js/*/; do
  (cd "$d" && npm ci --no-audit --no-fund && npm run build && npm pack --pack-destination "$out/npm") >&2
done

if [ "$build_image" = 1 ]; then
  log "runner base image (linux/amd64)"
  # CI runners are disk-tight: the debug build (from clippy/tests/docs checks)
  # is dead weight by now, and docker needs the space.
  if [ -n "${CI:-}" ]; then rm -rf target/debug; fi
  df -h . >&2 || true
  tag="sectr-runner-base:v$ver"
  cache=()
  # Local layer-cache export needs a docker-container builder; the default
  # driver already keeps layers in the engine, so re-runs are cached anyway.
  # Skipped on CI: nothing persists there and mode=max writes every layer to a
  # disk that is already full of cargo target dirs.
  if [ -z "${CI:-}" ] && [ "$(docker buildx inspect 2>/dev/null | awk '/^Driver:/ { print $2 }')" != docker ]; then
    cache=(--cache-from "type=local,src=.cache/buildx" --cache-to "type=local,dest=.cache/buildx,mode=max")
  fi
  docker buildx build --platform linux/amd64 -f docker/runner-base.Dockerfile \
    "${cache[@]}" --load -t "$tag" . >&2
  echo "$tag" > "$out/image.txt"
fi

( cd "$out" && find . -type f ! -name SHA256SUMS ! -name BUILT_FROM_SHA | sort | xargs shasum -a 256 > SHA256SUMS )
git rev-parse HEAD > "$out/BUILT_FROM_SHA"
[ -z "$(git status --porcelain)" ] || echo "(built from a dirty tree)" >> "$out/BUILT_FROM_SHA"
log "artifacts in $out"
