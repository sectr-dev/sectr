#!/usr/bin/env bash
# Build every release artifact locally into target/release-artifacts/vX.Y.Z/
#   dist/    CLI tarballs + checksums + installer + dist-manifest.json (dist)
#   server/  sectr-server (aarch64 glibc 2.34, --features e2b)
#   sdk/     sectr wheel + sdist
#   npm/     one tarball per js/ package (sectr-copilotkit-<ver>.tgz, sectr-0.0.0.tgz)
#   sites/   the Cloudflare Workers deploy-site uploads, each with its wrangler.jsonc:
#            dashboard/ (app.sectr.dev, OpenNext) and docs/ (docs.sectr.dev, static)
#   image.txt  the local image tags, one per line: sectr-runner-base, sectr-server
#              and sectr-dashboard, each as :vX.Y.Z-amd64 and :vX.Y.Z-arm64
# Usage: scripts/release-build.sh [--no-image]
# Built once; publish-* / deploy-server / deploy-site reuse these. BUILT_FROM_SHA records
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

for site in "${RELEASE_SITES[@]}"; do
  log "$site site (Cloudflare Worker)"
  build_site "$site" "$out/sites/$site"
done

if [ "$build_image" = 1 ]; then
  log "images (${RELEASE_ARCHS[*]})"
  # CI runners are disk-tight: the debug build (from clippy/tests/docs checks)
  # is dead weight by now, and docker needs the space.
  if [ -n "${CI:-}" ]; then rm -rf target/debug; fi
  df -h . >&2 || true
  # Each build adds about 12 GB of layers, and nothing else limits the cache.
  # Most of the old ones can't be reused anyway, once the crates have changed.
  docker buildx prune -f --max-used-space 15GB >&2
  sha=$(git rev-parse HEAD)
  for image in "${RELEASE_IMAGES[@]}"; do
    for arch in "${RELEASE_ARCHS[@]}"; do
      tag="$image:v$ver-$arch"
      log "$tag"
      # publish-images.sh pushes and joins plain single-platform images, so
      # attestations are off by name: --load alone keeps them on some image
      # stores. It also uses the revision label to check an image is this build.
      SECONDS=0
      docker buildx build --platform "linux/$arch" -f "$(image_dockerfile "$image")" \
        --provenance=false --sbom=false \
        --label "org.opencontainers.image.source=https://github.com/sectr-dev/sectr" \
        --label "org.opencontainers.image.version=$ver" \
        --label "org.opencontainers.image.revision=$sha" \
        --load -t "$tag" . >&2
      log "$tag built in ${SECONDS}s"
      echo "$tag" >> "$out/image.txt"
    done
  done
  # The name the daemon-gated tests look for, in this machine's architecture.
  case "$(uname -m)" in
    arm64|aarch64) native=arm64 ;;
    *) native=amd64 ;;
  esac
  docker tag "sectr-runner-base:v$ver-$native" "sectr-runner-base:v$ver"
fi

( cd "$out" && find . -type f ! -name SHA256SUMS ! -name BUILT_FROM_SHA | sort | xargs shasum -a 256 > SHA256SUMS )
git rev-parse HEAD > "$out/BUILT_FROM_SHA"
[ -z "$(git status --porcelain)" ] || echo "(built from a dirty tree)" >> "$out/BUILT_FROM_SHA"
log "artifacts in $out"
