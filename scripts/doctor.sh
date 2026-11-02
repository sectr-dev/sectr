#!/usr/bin/env bash
# `just doctor [release]` — check tools (and, for `release`, credentials) with
# a one-line fix hint per problem. Exits non-zero if anything required for the
# requested tier is missing.
set -uo pipefail
cd "$(dirname "$0")/.."
tier="${1:-}"
ok=1
have() { command -v "$1" >/dev/null 2>&1; }
need() { # tool, hint
  if have "$1"; then printf '  ok       %s\n' "$1"
  else printf '  MISSING  %s  ->  %s\n' "$1" "$2"; ok=0; fi
}
cred() { # label, check-command-string, hint
  if eval "$2" >/dev/null 2>&1; then printf '  ok       %s\n' "$1"
  else printf '  MISSING  %s  ->  %s\n' "$1" "$3"; ok=0; fi
}

echo "check / test:"
need just "brew install just"
need cargo "https://rustup.rs"
need node "brew install node@22 (or nvm use; see .nvmrc)"
need uv "brew install uv"

echo "verify:"
need docker "install Docker Desktop"
if have docker && ! docker info >/dev/null 2>&1; then
  echo "  MISSING  docker daemon is not running  ->  start Docker Desktop"; ok=0
fi
pg=$(ls -d /opt/homebrew/opt/postgresql@16/bin /usr/local/opt/postgresql@16/bin /usr/lib/postgresql/*/bin 2>/dev/null | head -1 || true)
if [ -n "$pg" ]; then echo "  ok       postgres initdb ($pg)"
else echo "  MISSING  postgresql@16  ->  brew install postgresql@16"; ok=0; fi
need jq "brew install jq"
need dist "install cargo-dist 0.33.0: https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.0"
if have dist && ! dist --version 2>/dev/null | grep -q '0\.33\.0'; then
  echo "  WRONG    dist is $(dist --version 2>&1 | head -1)  ->  need 0.33.0 (cargo-dist-version in Cargo.toml)"; ok=0
fi
need cargo-zigbuild "brew install zig && cargo install cargo-zigbuild --locked"
need zig "brew install zig"

if [ "$tier" = release ]; then
  echo "release (publish / deploy):"
  need gh "brew install gh"
  cred "gh authenticated (publish-skill pushes to sectr-dev/skills; also --require-gh)" "gh auth status" "gh auth login"
  have wrangler && echo "  ok       wrangler" || echo "  note     wrangler not installed (optional: npm i -g wrangler; publish-r2 uses npx otherwise)"
  need gcloud "https://cloud.google.com/sdk/docs/install"
  cred "gcloud authenticated" "gcloud auth list --filter=status:ACTIVE --format='value(account)' | grep -q ." "gcloud auth login"
  need bws "Bitwarden Secrets Manager CLI: https://bitwarden.com/help/secrets-manager-cli/"
  cred "BWS_ACCESS_TOKEN" '[ -n "${BWS_ACCESS_TOKEN:-}" ]' "export BWS_ACCESS_TOKEN (keychain/env; never in the repo)"
  need npm "brew install node@22 (publish-npm)"

  echo "release credentials (environment, else Bitwarden Secrets Manager by name):"
  bw_keys=""
  if have bws && have jq && [ -n "${BWS_ACCESS_TOKEN:-}" ]; then
    bw_keys=$(bws secret list -o json 2>/dev/null | jq -r '.[].key' 2>/dev/null || true)
  fi
  secret_ok() { # NAME [ALT_ENV_NAME]  — names only are listed; values are never read
    local n="$1" alt="${2:-}"
    if [ -n "${!n:-}" ]; then printf '  ok       %s (environment)\n' "$n"
    elif [ -n "$alt" ] && [ -n "${!alt:-}" ]; then printf '  ok       %s (environment, as %s)\n' "$n" "$alt"
    elif printf '%s\n' "$bw_keys" | grep -qx "$n"; then printf '  ok       %s (Bitwarden)\n' "$n"
    else printf '  MISSING  %s  ->  add a Bitwarden secret named %s, or export it\n' "$n" "$n"; ok=0; fi
  }
  secret_ok PYPI_TOKEN UV_PUBLISH_TOKEN                # publish-sdk
  secret_ok NPM_TOKEN                                  # publish-npm: @sectr scope + the unscoped sectr
  secret_ok CLOUDFLARE_ACCOUNT_ID                      # publish-r2 (wrangler)
  secret_ok CF_R2_TOKEN CLOUDFLARE_API_TOKEN           # publish-r2: R2 Edit on sectr-releases

  # The hosted deploy's own needs live with it, in the private ops repo.
  ops="${SECTR_OPS_DIR:-../sectr-ops}"
  if [ -x "$ops/scripts/doctor.sh" ]; then
    SECTR_DIR="$PWD" "$ops/scripts/doctor.sh" || ok=0
  else
    printf '  MISSING  ops repo at %s  ->  clone it there, or set SECTR_OPS_DIR\n' "$ops"; ok=0
  fi
fi

[ $ok = 1 ] && echo "all good" || { echo "some tools missing"; exit 1; }
