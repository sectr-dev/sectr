#!/usr/bin/env bash
# Deploy a site to Cloudflare: the dashboard (app.sectr.dev) or the docs
# (docs.sectr.dev). Pushes to main deploy neither.
#
#   deploy-site.sh SITE VERSION   The release's deploy-dashboard and deploy-docs
#       stages: upload the site `just build` made and `just verify` checked for
#       VERSION. Nothing is rebuilt, so what goes live is what was verified.
#   deploy-site.sh SITE           `just redeploy SITE`: build the site from main,
#       as it is on origin, and deploy it without a release. For a fix to the
#       site alone; it still shows the last release's version.
#
# Needs CLOUDFLARE_ACCOUNT_ID and CF_WORKERS_TOKEN (an API token with Workers
# Scripts: Edit on the account), from the environment or Bitwarden.
# DRY_RUN=1 bundles and checks the Worker, uploads nothing.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
site="${1:?usage: deploy-site.sh dashboard|docs [VERSION]}"
ver="${2:-}"
dir=$(site_dir "$site")
url=$(site_url "$site")

if [ -n "$ver" ]; then
  need_artifacts "$ver"
  src="$(artifacts_dir "$ver")/sites/$site"
  [ -f "$src/wrangler.jsonc" ] || die "no $site site in the v$ver build at $src — run: just build"
  tag="v$ver"
  message="sectr v$ver"
  [ -x "$dir/node_modules/.bin/wrangler" ] || (cd "$dir" && npm ci --no-audit --no-fund >&2)
else
  git fetch origin -q
  [ -z "$(git status --porcelain)" ] || die "the working tree isn't clean; redeploy builds what's on main"
  [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
    || die "HEAD isn't origin/main: git switch main && git pull, then redeploy"
  ver=$(scripts/version.sh)
  sha=$(git rev-parse --short HEAD)
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  log "building the $site site from main ($sha)"
  build_site "$site" "$work/$site"
  src="$work/$site"
  tag="v$ver-$sha"
  message="sectr v$ver + main@$sha (redeploy, no release)"
fi

# wrangler at the site's locked version.
export WRANGLER_SEND_METRICS=false
deploy=("$dir/node_modules/.bin/wrangler" deploy --config "$src/wrangler.jsonc"
  --tag "$tag" --message "$message")

if [ "${DRY_RUN:-}" = 1 ]; then
  "${deploy[@]}" --dry-run
  log "dry run: the $site site ($tag) bundles; nothing uploaded"
  exit 0
fi

load_secrets CLOUDFLARE_ACCOUNT_ID CF_WORKERS_TOKEN
CLOUDFLARE_API_TOKEN="$CF_WORKERS_TOKEN" "${deploy[@]}"

# The docs' sidebar footer shows the version; the dashboard renders it in
# the browser, so there it's enough that the site answers.
live() {
  case "$site" in
    docs) curl -fsS --max-time 10 "$url" | grep -qF "v$ver" ;;
    *) curl -fsS --max-time 10 -o /dev/null "$url" ;;
  esac
}
for _ in $(seq 1 12); do
  if live; then
    log "ok: $url is live ($tag)"
    exit 0
  fi
  log "$url not live yet — retrying"
  sleep 5
done
die "$url isn't live a minute after the deploy. To roll back: cd $dir && npx wrangler rollback"
