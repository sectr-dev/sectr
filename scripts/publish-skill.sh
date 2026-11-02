#!/usr/bin/env bash
# Publish the `sectr` agent skill to the public github.com/sectr-dev/skills
# repo, which users install with `npx skills add sectr-dev/skills`. Clones the
# repo, regenerates `sectr/` from this checkout's docs (gen-skill.mjs) and the
# README from docs-site/scripts/skills-repo-README.md, then commits and pushes
# if anything changed. Not tied to a CLI version: run it whenever the skill
# should catch up with the docs; `just release` also runs it.
# Auth: your `gh` login (push access to sectr-dev/skills).
# Usage: scripts/publish-skill.sh [VERSION]  (DRY_RUN=1 shows the diff, pushes nothing)
# OPEN_PR=1 pushes a branch and opens a PR on the skills repo instead of
# pushing to its default branch.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:-$(scripts/version.sh)}"
repo=sectr-dev/skills

command -v gh >/dev/null || die "gh is not installed (brew install gh)"
gh auth status >/dev/null 2>&1 || die "gh is not logged in: run gh auth login"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh repo clone "$repo" "$work/skills" -- --quiet --depth 1

(cd docs-site && npm ci --no-audit --no-fund >/dev/null && node scripts/gen-skill.mjs --out "$work/skills/sectr")
cp docs-site/scripts/skills-repo-README.md "$work/skills/README.md"

cd "$work/skills"
git add -A
if git diff --cached --quiet; then
  log "the published skill is already current"
  exit 0
fi
git diff --cached --stat >&2
sha=$(git -C "$root" rev-parse --short HEAD)
msg="sectr skill from sectr v$ver ($sha)"
if [ "${OPEN_PR:-}" = 1 ]; then
  base=$(git symbolic-ref --short HEAD)
  branch="sectr-v$ver-$sha"
  run git switch --quiet -c "$branch"
  run git commit --quiet -m "$msg"
  run git push --quiet origin "$branch"
  run gh pr create --repo "$repo" --base "$base" --head "$branch" \
    --title "$msg" --body "Regenerated from the docs at sectr-dev/sectr@$sha."
  log "opened a PR on github.com/$repo"
  exit 0
fi
run git commit --quiet -m "$msg"
run git push --quiet origin HEAD
log "published the skill to github.com/$repo"
