#!/usr/bin/env bash
# Deploy a site to Cloudflare: the dashboard (app.sectr.dev) or the docs
# (docs.sectr.dev). Pushes to main deploy neither.
#
#   deploy-site.sh SITE VERSION   The release's deploy-dashboard and deploy-docs
#       stages: upload the site `just build` made and `just verify` checked for
#       VERSION. Nothing is rebuilt, so what goes live is what was verified.
#   deploy-site.sh SITE           `just redeploy SITE`: build the site from main,
#       as it is on origin, and deploy it without a release. For a fix to the
#       site alone; it still shows the last release's version. It lists what
#       main has gained since the last release and asks first (YES=1 skips).
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

# commits SINCE PATH — the commits since SINCE that touch PATH, the newest 15.
commits() {
  local all n
  all=$(git log --oneline "$1..HEAD" -- "$2")
  n=$(printf '%s' "$all" | grep -c . || true)
  [ "$n" -gt 0 ] || { echo "  (none)"; return; }
  printf '%s\n' "$all" | head -15 | sed 's/^/  /'
  [ "$n" -le 15 ] || echo "  … and $((n - 15)) more"
}

# What's on main since the last release goes live with a redeploy: show it,
# then ask. YES=1 skips the question (for a script with no terminal).
confirm_redeploy() {
  local last
  last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
  {
    echo
    if [ -n "$last" ]; then
      echo "main is $(git rev-list --count "$last..HEAD") commits past $last. Since then, $dir/ changed in:"
      commits "$last" "$dir"
      if [ "$site" = dashboard ]; then
        echo "and the API it calls (schema/public-api.yaml; live only with the next release) in:"
        commits "$last" schema/public-api.yaml
      fi
      echo
    fi
    echo "This deploys $site from main@$sha without a release: every $dir/ change since ${last:-the last release} goes live."
    case "$site" in
      dashboard) echo "The API changes aren't live until the next release, so only do it if the dashboard on main doesn't need them." ;;
      docs) echo "Only do it if none of them describes something that isn't released yet." ;;
    esac
  } >&2
  [ "${YES:-}" = 1 ] && return
  local answer
  read -r -p "Deploy $site from main@$sha? [y/N] " answer </dev/tty \
    || die "no terminal to confirm on (YES=1 skips the question)"
  case "$answer" in
    y|Y|yes) ;;
    *) die "not deployed" ;;
  esac
}

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
  [ "${DRY_RUN:-}" = 1 ] || confirm_redeploy
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
