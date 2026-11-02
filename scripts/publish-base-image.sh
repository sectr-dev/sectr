#!/usr/bin/env bash
# Push the runner base image (linux/amd64 only — E2B is x86_64) to Artifact
# Registry as :vX.Y.Z and :latest, then verify the tag exists. Uses the image
# from `just build` (builds it if missing). Needs docker + `gcloud auth login`
# as an identity with Artifact Registry upload on sectr-runners.
# Usage: scripts/publish-base-image.sh [VERSION]  (DRY_RUN=1 previews)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"; tag="v$ver"
REGISTRY=us-east4-docker.pkg.dev
IMAGE=us-east4-docker.pkg.dev/sectr-dev-506916/sectr-runners/sectr-runner-base
local_tag="sectr-runner-base:$tag"

if ! docker image inspect "$local_tag" >/dev/null 2>&1; then
  log "no local $local_tag; building"
  docker buildx build --platform linux/amd64 -f docker/runner-base.Dockerfile --load -t "$local_tag" .
fi
run gcloud auth configure-docker "$REGISTRY" --quiet
run docker tag "$local_tag" "$IMAGE:$tag"
run docker tag "$local_tag" "$IMAGE:latest"
run docker push "$IMAGE:$tag"
run docker push "$IMAGE:latest"

[ "${DRY_RUN:-}" = 1 ] && exit 0
gcloud artifacts docker tags list "$IMAGE" --format='value(tag)' | sort -u | grep -qx "$tag" \
  || die "tag $tag not found in $IMAGE"
echo "pushed: $IMAGE:$tag (+ :latest)"
