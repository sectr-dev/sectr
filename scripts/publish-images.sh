#!/usr/bin/env bash
# Publish the images `just build` made and `just verify` checked. Nothing is
# rebuilt here, so what gets published is exactly what was verified.
#   GHCR: every per-arch tag, then :vX.Y.Z and :latest joining the two
#     architectures, for sectr-runner-base, sectr-server and sectr-dashboard.
#   Artifact Registry: the amd64 base image as :vX.Y.Z and :latest, for hosted
#     builds (E2B is x86_64).
# Needs docker, gh, GHCR_TOKEN (write:packages) and `gcloud auth login` as an
# identity with Artifact Registry upload on sectr-runners.
# Usage: scripts/publish-images.sh [VERSION]  (DRY_RUN=1 previews)
# SECTR_IMAGE_REGISTRY replaces GHCR with another registry prefix and skips
# Artifact Registry, to rehearse this against a throwaway local registry.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"; tag="v$ver"
registry="${SECTR_IMAGE_REGISTRY:-ghcr.io/sectr-dev}"
AR_REGISTRY=us-east4-docker.pkg.dev
AR_IMAGE=$AR_REGISTRY/sectr-dev-506916/sectr-runners/sectr-runner-base
dry() { [ "${DRY_RUN:-}" = 1 ]; }

# A later build can reuse these tags, so check they still hold this build.
if ! dry; then
  need_artifacts "$ver"
  built=$(head -1 "$(artifacts_dir "$ver")/BUILT_FROM_SHA")
  for image in "${RELEASE_IMAGES[@]}"; do
    for arch in "${RELEASE_ARCHS[@]}"; do
      local_tag="$image:$tag-$arch"
      revision=$(docker image inspect "$local_tag" --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' 2>/dev/null) \
        || die "no local $local_tag: run just build"
      [ "$revision" = "$built" ] || die "$local_tag is from $revision, not this build ($built): run just build"
    done
  done
fi

if [ -z "${SECTR_IMAGE_REGISTRY:-}" ]; then
  run gcloud auth configure-docker "$AR_REGISTRY" --quiet
  run docker tag "sectr-runner-base:$tag-amd64" "$AR_IMAGE:$tag"
  run docker tag "sectr-runner-base:$tag-amd64" "$AR_IMAGE:latest"
  run retry 3 docker push "$AR_IMAGE:$tag"
  run retry 3 docker push "$AR_IMAGE:latest"
  if ! dry; then
    gcloud artifacts docker tags list "$AR_IMAGE" --format='value(tag)' | sort -u | grep -qx "$tag" \
      || die "tag $tag not found in $AR_IMAGE"
    echo "pushed: $AR_IMAGE:$tag (+ :latest)"

    trap 'docker logout ghcr.io >/dev/null 2>&1 || true' EXIT
    secret GHCR_TOKEN | docker login ghcr.io -u "$(gh api user --jq .login)" --password-stdin >/dev/null
  fi
fi

want=$(printf '%s\n' "${RELEASE_ARCHS[@]}" | sort | xargs)
for image in "${RELEASE_IMAGES[@]}"; do
  ref="$registry/$image"
  sources=()
  for arch in "${RELEASE_ARCHS[@]}"; do
    run docker tag "$image:$tag-$arch" "$ref:$tag-$arch"
    run retry 3 docker push "$ref:$tag-$arch"
    sources+=("$ref:$tag-$arch")
  done
  run retry 3 docker buildx imagetools create -t "$ref:$tag" -t "$ref:latest" "${sources[@]}"
  dry && continue
  for t in "$tag" latest; do
    got=$(docker buildx imagetools inspect --raw "$ref:$t" | jq -r '.manifests[].platform.architecture' | sort | xargs)
    [ "$got" = "$want" ] || die "$ref:$t has architectures [$got], not [$want]"
  done
  echo "published: $ref:$tag (+ :latest), $want"
done
