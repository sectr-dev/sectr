# Shared helpers for release scripts. Source it:  . "$(dirname "$0")/lib.sh"
# DRY_RUN=1 makes `run` print instead of execute (publish/deploy scripts).

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

die() { echo "error: $*" >&2; exit 1; }
log() { echo "==> $*" >&2; }

# run CMD...  — execute, or just print under DRY_RUN=1.
run() {
  if [ "${DRY_RUN:-}" = 1 ]; then
    printf '[dry-run]'; printf ' %q' "$@"; echo
  else
    "$@"
  fi
}

# load_secrets NAME... — export each NAME not already in the environment from
# Bitwarden Secrets Manager (`bws`, secret whose key == NAME; needs
# BWS_ACCESS_TOKEN), all from ONE `bws secret list`: every call is a round
# trip that can time out, so a script fetches what it needs at once. Bitwarden
# answers `secret list` with an occasional 504 (several during the v0.0.9
# deploy, fine seconds later), so a failed list is retried with backoff.
load_secrets() {
  local missing=() name json val
  for name in "$@"; do [ -n "${!name:-}" ] || missing+=("$name"); done
  [ ${#missing[@]} -gt 0 ] || return 0
  command -v bws >/dev/null || die "${missing[*]} not set and bws is not installed (see RELEASING.md)"
  [ -n "${BWS_ACCESS_TOKEN:-}" ] || die "${missing[*]} not set and BWS_ACCESS_TOKEN is not set"
  # bws writes its errors to stderr, so only the successful attempt's JSON is captured.
  json=$(retry 4 bws secret list -o json) || die "bws secret list failed 4 times (errors above); nothing was read"
  for name in "${missing[@]}"; do
    val=$(jq -r --arg k "$name" 'first(.[] | select(.key == $k) | .value) // empty' <<<"$json")
    [ -n "$val" ] || die "secret $name not found in Bitwarden Secrets Manager"
    export "$name=$val"
  done
}

# retry ATTEMPTS CMD... — run CMD, retrying with backoff (5s, 10s, …) while it
# fails; only for idempotent steps: the Bitwarden list above, R2 puts
# (publish-r2), and the deploy's ssh/scp staging (SSH's integrity check means
# a corrupted upload never lands, so retrying an upload is safe). Progress
# goes to stderr, so `x=$(retry …)` captures only CMD's output.
retry() {
  local attempts=$1 attempt
  shift
  for ((attempt = 1; ; attempt++)); do
    "$@" && return 0
    [ "$attempt" -lt "$attempts" ] || { log "failed $attempt times: $*"; return 1; }
    log "failed (attempt $attempt of $attempts); retrying in $((attempt * 5))s: $*"
    sleep $((attempt * 5))
  done
}

# find_openssh — set ssh_bin/scp_bin to an upstream OpenSSH (Homebrew's), never
# macOS's bundled /usr/bin/ssh (OpenSSH_10.2p1 + LibreSSL 3.3.6): it corrupts
# uploads now and then, about once per 30 MB, which the VM's sshd rejects with
# "message authentication code incorrect". Homebrew's OpenSSH on the same
# machine and network is clean (2026-10-23). Fails if there is no upstream one.
find_openssh() {
  local dir
  ssh_bin=$(command -v ssh || true); scp_bin=$(command -v scp || true)
  for dir in /opt/homebrew/bin /usr/local/bin; do
    if [ -x "$dir/ssh" ] && [ -x "$dir/scp" ]; then ssh_bin=$dir/ssh; scp_bin=$dir/scp; break; fi
  done
  [ -n "$ssh_bin" ] && [ -n "$scp_bin" ] && ! "$ssh_bin" -V 2>&1 | grep -q LibreSSL
}

# secret NAME — value of NAME: the environment wins, else Bitwarden (above).
secret() {
  load_secrets "$1"
  printf '%s' "${!1}"
}

# artifacts_dir VERSION — where `just build` leaves everything for a version.
# SECTR_ARTIFACTS_DIR lets a build from a tag worktree land in the main checkout.
artifacts_dir() { echo "${SECTR_ARTIFACTS_DIR:-$root/target/release-artifacts}/v$1"; }

# The sites a release deploys to Cloudflare, each a Worker: the dashboard
# (app.sectr.dev) and the docs (docs.sectr.dev). site_dir SITE is its
# source folder, site_url SITE its address.
RELEASE_SITES=(dashboard docs)
site_dir() {
  case "$1" in
    dashboard) echo dashboard ;;
    docs) echo docs-site ;;
    *) die "unknown site '$1' (dashboard or docs)" ;;
  esac
}
site_url() {
  case "$1" in
    dashboard) echo https://app.sectr.dev/ ;;
    docs) echo https://docs.sectr.dev/ ;;
    *) die "unknown site '$1' (dashboard or docs)" ;;
  esac
}

# build_site SITE DEST — build SITE from this checkout and copy what wrangler
# uploads into DEST: its wrangler.jsonc and the build output that config
# points at (the dashboard's OpenNext Worker, the docs' static pages).
build_site() {
  local site=$1 dest=$2 dir output
  dir=$(site_dir "$site")
  case "$site" in
    dashboard) (cd "$dir" && npm ci --no-audit --no-fund && npm run build:worker) >&2; output=.open-next ;;
    docs) (cd "$dir" && npm ci --no-audit --no-fund && npm run build) >&2; output=dist ;;
  esac
  rm -rf "$dest"; mkdir -p "$dest"
  cp "$dir/wrangler.jsonc" "$dest/"
  cp -R "$dir/$output" "$dest/$output"
}

# need_artifacts VERSION — fail unless a build for VERSION exists.
need_artifacts() {
  [ -f "$(artifacts_dir "$1")/BUILT_FROM_SHA" ] \
    || die "no build for v$1 at $(artifacts_dir "$1") — run: just build"
}

# The images a release builds and publishes, each once per architecture, tagged
# locally as <image>:vX.Y.Z-<arch>.
RELEASE_IMAGES=(sectr-runner-base sectr-server sectr-dashboard)
# CI is an advisory check on an amd64 runner, where an emulated arm64 Rust
# compile would outlast the job. Releases are cut from the dev machine.
RELEASE_ARCHS=(amd64 arm64)
if [ -n "${CI:-}" ]; then RELEASE_ARCHS=(amd64); fi

# image_dockerfile IMAGE — the Dockerfile that builds a release image.
image_dockerfile() {
  case "$1" in
    sectr-runner-base) echo docker/runner-base.Dockerfile ;;
    sectr-server) echo docker/Dockerfile.server ;;
    sectr-dashboard) echo docker/Dockerfile.dashboard ;;
    *) die "unknown image $1" ;;
  esac
}
