#!/bin/sh
# First-time setup for self-hosted Sectr: writes .env with generated secrets.
# Run once, from anywhere. It never overwrites an existing .env.
set -eu
cd "$(dirname "$0")"

die() {
  echo "setup: $*" >&2
  exit 1
}

command -v docker >/dev/null 2>&1 || die "Docker isn't installed. Get it from https://docs.docker.com/get-docker/"
docker info >/dev/null 2>&1 || die "Docker isn't running, or this user can't reach it. Start it and run this again."
docker compose version >/dev/null 2>&1 || die "the Docker Compose plugin isn't installed. See https://docs.docker.com/compose/install/"
[ -e .env ] && die ".env already exists, so nothing was changed. Delete it only if you mean to start over: its keys protect your stored data."
[ -f .env.example ] || die ".env.example is missing from $(pwd)"

base64url() { head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'; }
hex() { od -An -tx1 -N32 /dev/urandom | tr -d ' \n'; }

platform_key="skp_$(base64url)"
master_key="$(hex)"
postgres_password="$(hex)"

# Written to a temp file and moved into place, private from the start, so no
# partial or world-readable .env can exist.
umask 077
tmp=".env.$$"
trap 'rm -f "$tmp"' EXIT
awk \
  -v key="$platform_key" \
  -v master="$master_key" \
  -v pw="$postgres_password" '
  /^SECTR_BOOTSTRAP_PLATFORM_KEY=/ { print "SECTR_BOOTSTRAP_PLATFORM_KEY=" key; next }
  /^SECTR_SECRETS_MASTER_KEY=/ { print "SECTR_SECRETS_MASTER_KEY=" master; next }
  /^POSTGRES_PASSWORD=/ { print "POSTGRES_PASSWORD=" pw; next }
  /^SECTR_PG_URL=/ { print "SECTR_PG_URL=postgres://sectr:" pw "@postgres:5432/sectr"; next }
  { print }
' .env.example >"$tmp"
chmod 600 "$tmp"
mv "$tmp" .env
trap - EXIT

cat <<DONE
Wrote .env (only you can read it).

Your platform key, which is also in .env:

  $platform_key

Next:

  docker compose up -d

Then open http://localhost:3000 and sign in with the key. To deploy an agent:

  curl -fsSL https://sectr.dev/install.sh | sh
  sectr auth login --api http://localhost:8080
  sectr init my-agent && cd my-agent && sectr deploy

On a remote machine, set SECTR_PUBLIC_API_URL and SECTR_DASHBOARD_URL in .env
first: https://docs.sectr.dev/guides/self-hosting/
DONE
