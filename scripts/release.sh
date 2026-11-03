#!/usr/bin/env bash
# Local-first release driver. Runbook: RELEASING.md.
#
#   release.sh patch|minor|major [--dry-run] [--require-gh] [--skip-verify "<reason>"] [--yes]
#   release.sh resume vX.Y.Z [stage] [flags]   (also continues an untagged release after a verify failure)
#   release.sh abort [vX.Y.Z]
#
# Stages (each re-runnable; progress is logged in .release/vX.Y.Z.log):
#   before the tag is pushed (abortable):
#     preflight  doctor passes, clean tree, on main == origin/main, tag absent
#     cut        branch release/vX.Y.Z + "release: vX.Y.Z" bump commit
#     verify     `just ci-full` on the branch tip (marker .release/verified-<sha>);
#                --require-gh also waits for a green release-check.yml run
#     tag        annotated tag vX.Y.Z on the verified SHA   (--dry-run stops here)
#   after (the version is burned; use `resume` to finish):
#     push, merge-back, publish-r2, publish-sdk, publish-npm,
#     publish-skill, publish-base-image, deploy-server, smoke-docs
set -euo pipefail
. "$(dirname "$0")/lib.sh"

POST_STAGES=(push merge-back publish-r2 publish-sdk publish-npm publish-skill publish-base-image deploy-server smoke-docs)
# Test hook: branch releases are cut from / merged back into (default main).
BASE=${RELEASE_BASE_BRANCH:-main}

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

VER=""; DRY_RUN_RELEASE=0; REQUIRE_GH=0; SKIP_REASON=""; ASSUME_YES=0
logfile() { mkdir -p .release; echo ".release/v$VER.log"; }
mark()    { echo "$(date -u +%FT%TZ) $1 done" >> "$(logfile)"; }
is_done() { grep -qs " $1 done\$" "$(logfile)"; }
stage_banner() { echo; echo "==> [$1] v$VER"; }
tag_sha() { git rev-list -n1 "v$VER"; }
on_origin() { [ -n "$(git ls-remote --tags origin "refs/tags/v$VER")" ]; }

# ---------------------------------------------------------------- pre-tag
stage_preflight() {
  scripts/doctor.sh release
  git fetch origin --tags -q
  [ -z "$(git status --porcelain)" ] || die "working tree is not clean"
  [ "$(git branch --show-current)" = "$BASE" ] || die "must start on $BASE (on '$(git branch --show-current)')"
  [ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$BASE")" ] || die "$BASE is not at origin/$BASE (pull/push first)"
  scripts/set-version.sh --check >/dev/null
  git rev-parse -q --verify "refs/tags/v$VER" >/dev/null && die "tag v$VER already exists locally"
  on_origin && die "tag v$VER already exists on origin"
  [ -z "$(git ls-remote --heads origin "release/v$VER")" ] || die "branch release/v$VER already exists on origin"
}

stage_cut() {
  git switch -c "release/v$VER"
  scripts/set-version.sh "$VER" >/dev/null
  # uv.lock records the SDK's own version and `uv sync` (part of ci-full)
  # would otherwise rewrite it after verification, dirtying the tree. The
  # conformance projects' lock pins the SDK too, and the harness syncs it
  # `--locked`, so a stale one fails verify.
  (cd sdk-python && uv lock -q)
  (cd sdk-python/tests/conformance/gen && uv lock -q)
  # The examples' locks pin the SDK too (`just check-examples`).
  for lock in examples/*/uv.lock; do (cd "$(dirname "$lock")" && uv lock -q); done
  # ALL manifests set-version.sh rewrites, lockfiles too: a missing file
  # is silent (--check passes on the working tree) and only the commit is
  # incomplete (v0.0.5 bug). Plus the uv.lock files (refreshed above).
  git add Cargo.toml Cargo.lock sdk-python/pyproject.toml sdk-python/uv.lock \
    sdk-python/tests/conformance/gen/uv.lock examples/*/uv.lock \
    dashboard/package.json dashboard/package-lock.json \
    docs-site/package.json docs-site/package-lock.json \
    js/copilotkit/package.json js/copilotkit/package-lock.json
  git commit -q -m "release: v$VER"
  # Anything left over means the list above drifted from set-version.sh.
  [ -z "$(git status --porcelain)" ] || { git status --porcelain >&2; die "unstaged changes left after the release commit (file list drifted from set-version.sh)"; }
}

stage_verify() {
  if [ -n "$SKIP_REASON" ]; then
    echo "!! verification SKIPPED: $SKIP_REASON"
    just build   # artifacts are still needed for publishing
    return
  fi
  just ci-full
  if [ $REQUIRE_GH = 1 ]; then
    local sha; sha=$(git rev-parse HEAD)
    git push --no-verify -u origin "release/v$VER"
    echo "waiting for a green release-check.yml run on $sha ..."
    local deadline=$((SECONDS + 5400)) out
    while [ $SECONDS -lt $deadline ]; do
      out=$(gh run list --workflow release-check.yml --commit "$sha" --json status,conclusion -L 1 --jq '.[0] | "\(.status) \(.conclusion)"' 2>/dev/null || true)
      case "$out" in
        "completed success") echo "release-check: green"; return ;;
        completed*) die "release-check.yml did not pass ($out)" ;;
      esac
      sleep 30
    done
    die "timed out waiting for release-check.yml"
  fi
}

stage_tag() {
  local sha; sha=$(git rev-parse HEAD)
  if [ -z "$SKIP_REASON" ] && [ ! -f ".release/verified-$sha" ]; then
    die "refusing to tag $sha: no verification marker (.release/verified-$sha). Run: just verify (or --skip-verify \"<reason>\")"
  fi
  ensure_artifacts_at_head "$sha"
  local msgs=(-m "release v$VER")
  [ -n "$SKIP_REASON" ] && msgs+=(-m "verification skipped: $SKIP_REASON")
  git tag -a "v$VER" "${msgs[@]}"
}

# Artifacts must be built from exactly the commit being tagged.
ensure_artifacts_at_head() {
  local out; out=$(artifacts_dir "$VER")
  if [ -f "$out/BUILT_FROM_SHA" ] && [ "$(head -1 "$out/BUILT_FROM_SHA")" = "$1" ] && ! grep -q dirty "$out/BUILT_FROM_SHA"; then return; fi
  echo "artifacts missing or not built from $1; rebuilding"
  just build
}

# ---------------------------------------------------------------- post-tag
# Artifacts for publish stages: reuse the local build if it is from the tag's
# commit, else build in a throwaway worktree at the tag (resume on a moved checkout).
ensure_artifacts_for_tag() {
  local out sha wt; out=$(artifacts_dir "$VER"); sha=$(tag_sha)
  if [ -f "$out/BUILT_FROM_SHA" ] && [ "$(head -1 "$out/BUILT_FROM_SHA")" = "$sha" ] && ! grep -q dirty "$out/BUILT_FROM_SHA"; then return; fi
  wt="$root/target/release-worktree"
  echo "building artifacts from tag v$VER in a worktree"
  git worktree remove --force "$wt" 2>/dev/null || true
  git worktree add --detach "$wt" "v$VER"
  (cd "$wt" && SECTR_ARTIFACTS_DIR="$root/target/release-artifacts" scripts/release-build.sh)
  git worktree remove --force "$wt"
}

stage_push() {
  local refs=("refs/tags/v$VER")
  git rev-parse -q --verify "refs/heads/release/v$VER" >/dev/null && refs+=("refs/heads/release/v$VER")
  # --no-verify: the pre-push hook re-runs `just test`; this SHA is already verified.
  git push --no-verify origin "${refs[@]}"
}

stage_merge_back() {
  git fetch origin -q
  git switch "$BASE" 2>/dev/null || die "cannot switch to $BASE (resolve any in-progress merge first, then: just release-resume v$VER merge-back)"
  if ! git merge-base --is-ancestor "v$VER" HEAD; then
    git merge --ff-only "origin/$BASE" -q || die "local $BASE diverged from origin/$BASE"
    git merge --no-edit "v$VER" || die "merge conflict. Resolve, 'git commit', then: just release-resume v$VER merge-back"
  fi
  git push --no-verify origin "$BASE"
  # The tag is now the source of truth; the branch has served its purpose.
  if [ -n "$(git ls-remote --heads origin "release/v$VER")" ]; then git push --no-verify origin --delete "release/v$VER"; fi
  git branch -D "release/v$VER" 2>/dev/null || true
}

stage_publish_r2() {
  ensure_artifacts_for_tag
  local newest; newest=$(git tag -l 'v[0-9]*' | sed 's/^v//' | sort -V | tail -1)
  if [ "$newest" = "$VER" ]; then just publish-r2 "$VER" latest; else just publish-r2 "$VER"; fi
}
stage_publish_sdk()        { ensure_artifacts_for_tag; just publish-sdk "$VER"; }
stage_publish_npm()        { ensure_artifacts_for_tag; just publish-npm "$VER"; }
stage_publish_skill()      { just publish-skill "$VER"; }
stage_publish_base_image() { ensure_artifacts_for_tag; just publish-base-image "$VER"; }
stage_deploy_server()      { ensure_artifacts_for_tag; just deploy-server "$VER"; }
stage_smoke_docs()         { scripts/smoke-docs.sh "$VER"; }

run_post() { # start-stage (empty = first not done); explicit => run it and all later
  local start="${1:-}" explicit=0 s fn started=0
  [ -n "$start" ] && explicit=1
  if [ $explicit = 1 ]; then
    local ok=0; for s in "${POST_STAGES[@]}"; do [ "$s" = "$start" ] && ok=1; done
    [ $ok = 1 ] || die "unknown stage '$start' (one of: ${POST_STAGES[*]})"
  fi
  for s in "${POST_STAGES[@]}"; do
    [ $explicit = 1 ] && [ "$s" = "$start" ] && started=1
    if [ $explicit = 1 ] && [ $started = 0 ]; then continue; fi
    if [ $explicit = 0 ] && is_done "$s"; then echo "(skip $s: already done)"; continue; fi
    stage_banner "$s"
    # Re-invoke ourselves per stage: bash ignores `set -e` inside functions
    # called from an `if`, which would let a failed step go unnoticed.
    if "$0" __stage "$s" "$VER"; then mark "$s"; else
      echo >&2; echo "FAILED at stage '$s'. Fix the cause, then: just release-resume v$VER $s" >&2
      echo "Troubleshooting: RELEASING.md#failure-playbook" >&2; exit 1
    fi
  done
}

summary() {
  cat <<EOS

Release v$VER complete:
  CLI + installer  https://dl.sectr.dev/v$VER/  (latest/ if newest)
  SDK              sectr==$VER on PyPI
  npm              @sectr/copilotkit@$VER (+ the sectr shell, first release only)
  Base image       sectr-runner-base:v$VER (+ :latest) in Artifact Registry
  Server           deployed and health-checked
  Docs             docs.sectr.dev shows v$VER
  main             contains the bump; release/v$VER deleted
Post-release sanity checks: RELEASING.md#after-a-release
EOS
}

# ---------------------------------------------------------------- commands
cmd_release() {
  local bump="$1"; shift
  parse_flags "$@"
  VER=$(scripts/set-version.sh --next "$bump")
  echo "Releasing v$VER ($bump bump from $(scripts/version.sh))"
  stage_banner preflight; stage_preflight
  stage_banner cut;       stage_cut
  stage_banner verify;    stage_verify
  stage_banner tag;       stage_tag
  if [ $DRY_RUN_RELEASE = 1 ]; then
    echo; echo "--dry-run: stopped before pushing. Local state: branch release/v$VER, tag v$VER, artifacts in $(artifacts_dir "$VER")"
    echo "Undo with: just release-abort"
    exit 0
  fi
  confirm_push
  run_post ""
  summary
}

confirm_push() {
  [ $ASSUME_YES = 1 ] && return
  [ -t 0 ] || die "refusing to push without a terminal; pass --yes to confirm"
  echo; echo "Point of no return: pushing tag v$VER burns the version."
  read -r -p "Push and publish v$VER? [y/N] " a
  [ "$a" = y ] || [ "$a" = Y ] || die "not confirmed; undo with: just release-abort"
}

parse_flags() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run) DRY_RUN_RELEASE=1 ;;
      --require-gh) REQUIRE_GH=1 ;;
      --skip-verify) shift; SKIP_REASON="${1:?--skip-verify needs a reason}" ;;
      --yes) ASSUME_YES=1 ;;
      *) die "unknown flag '$1' (see: just release --help)" ;;
    esac; shift
  done
}

cmd_resume() {
  VER="${1:?usage: release-resume vX.Y.Z [stage] [flags]}"; VER="${VER#v}"; shift
  local stage=""
  if [ $# -gt 0 ] && [[ "$1" != --* ]]; then stage="$1"; shift; fi
  parse_flags "$@"
  git fetch origin --tags -q
  if ! git rev-parse -q --verify "refs/tags/v$VER" >/dev/null; then
    # Not tagged yet: continue a release whose verify failed (or that you
    # fixed on the release branch) from `verify` onward.
    git rev-parse -q --verify "refs/heads/release/v$VER" >/dev/null \
      || die "no tag v$VER and no branch release/v$VER; nothing to resume (start with: just release ...)"
    [ -z "$stage" ] || die "v$VER is not tagged yet; stages after the tag can't run (omit the stage)"
    git switch "release/v$VER"
    [ -z "$(git status --porcelain)" ] || die "working tree is not clean (commit your fix on release/v$VER)"
    stage_banner verify; stage_verify
    stage_banner tag;    stage_tag
    if [ $DRY_RUN_RELEASE = 1 ]; then echo "--dry-run: stopped before pushing. Undo with: just release-abort"; exit 0; fi
    scripts/doctor.sh release
    confirm_push
    run_post ""
    summary
    return
  fi
  scripts/doctor.sh release
  # Tagged locally but never pushed (e.g. after --dry-run): resuming will push.
  on_origin || confirm_push
  run_post "$stage"
  summary
}

cmd_abort() {
  local arg="${1:-}"
  if [ -n "$arg" ]; then VER="${arg#v}"
  else
    local b; b=$(git branch --show-current)
    if [[ "$b" == release/v* ]]; then VER="${b#release/v}"
    else
      local c; c=$(git for-each-ref --format='%(refname:short)' 'refs/heads/release/v*')
      [ "$(echo "$c" | grep -c .)" = 1 ] || die "cannot tell which release to abort; pass a version: just release-abort vX.Y.Z"
      VER="${c#release/v}"
    fi
  fi
  git fetch origin --tags -q
  on_origin && die "tag v$VER is on origin: the version is burned. Finish it with: just release-resume v$VER"
  [ -z "$(git status --porcelain)" ] || die "working tree is not clean"
  git switch "$BASE"
  git tag -d "v$VER" 2>/dev/null || true
  git branch -D "release/v$VER" 2>/dev/null || true
  if [ -n "$(git ls-remote --heads origin "release/v$VER")" ]; then git push --no-verify origin --delete "release/v$VER"; fi
  rm -f ".release/v$VER.log"
  echo "aborted v$VER: local/remote release branch and local tag removed; on $BASE"
}

case "${1:-}" in
  patch|minor|major) bump="$1"; shift; cmd_release "$bump" "$@" ;;
  resume) shift; cmd_resume "$@" ;;
  __stage) VER="$3"; "stage_${2//-/_}" ;;
  abort)  shift; cmd_abort "$@" ;;
  ""|-h|--help|help) usage ;;
  *) echo "unknown command '$1'" >&2; usage >&2; exit 2 ;;
esac
