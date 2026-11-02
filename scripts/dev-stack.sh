#!/usr/bin/env bash
# The dashboard's live local stack — what `npm run dev:server` (dashboard/)
# runs: Postgres in docker, a native debug sectr-server on :8080 signing in
# against the Clerk DEV instance, and `next dev` in live mode on :3000.
# Ctrl-C stops the server and the dashboard; Postgres keeps running (and
# keeps your data) — `docker rm -f sectr-dev-pg` to stop it.
#
# State lives in .dev-stack/ (gitignored): the secrets master key (stable,
# so stored secrets stay decryptable across restarts), the cached Clerk
# public key, and the build/artifact dirs (under /Users, so Docker Desktop
# can bind-mount them). Postgres data is the `sectr-dev-pg` docker volume.
# Wipe everything: docker rm -f sectr-dev-pg && docker volume rm
# sectr-dev-pg && rm -rf .dev-stack
set -euo pipefail
. "$(dirname "$0")/lib.sh"

state="$root/.dev-stack"
port=8080
dashboard_origin=http://localhost:3000
pg_container=sectr-dev-pg
pg_port=55432 # not 5432: don't collide with a host (e.g. Homebrew) Postgres

command -v docker >/dev/null || die "docker is not installed"
docker info >/dev/null 2>&1 || die "docker is not running (start Docker Desktop)"
command -v cargo >/dev/null || die "cargo is not on PATH (install Rust: https://rustup.rs)"
if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
  die "port $port is already in use — is another sectr-server running?"
fi
mkdir -p "$state/build" "$state/artifacts"

# --- Postgres ---------------------------------------------------------------
if ! docker ps --format '{{.Names}}' | grep -qx "$pg_container"; then
  if docker ps -a --format '{{.Names}}' | grep -qx "$pg_container"; then
    log "starting Postgres ($pg_container)"
    docker start "$pg_container" >/dev/null
  else
    log "creating Postgres ($pg_container, localhost:$pg_port)"
    docker run -d --name "$pg_container" -p "127.0.0.1:$pg_port:5432" \
      -e POSTGRES_USER=sectr -e POSTGRES_PASSWORD=sectr -e POSTGRES_DB=sectr \
      -v sectr-dev-pg:/var/lib/postgresql/data postgres:16 >/dev/null
  fi
fi
# Over TCP: the first-boot init server listens on the unix socket only, so
# a socket probe would pass before the real server is up.
until docker exec "$pg_container" pg_isready -q -h 127.0.0.1 -U sectr; do sleep 0.5; done

# --- Keys -------------------------------------------------------------------
[ -f "$state/secrets-master-key" ] || openssl rand -hex 32 >"$state/secrets-master-key"

# Clerk: the dev instance's issuer is encoded in its publishable key
# (pk_test_<base64 "host$">); the server verifies session JWTs offline
# against the instance's JWKS key, as SPKI base64 on one line (config.rs).
pk="${NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY:-$(sed -n 's/^NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY=//p' dashboard/.env.local 2>/dev/null)}"
[[ "$pk" == pk_test_* ]] \
  || die "set NEXT_PUBLIC_CLERK_PUBLISHABLE_KEY in dashboard/.env.local to the Clerk dev instance's pk_test_… key"
clerk_host=$(printf '%s' "${pk#pk_test_}" | base64 -d | tr -d '$')
clerk_key="$state/clerk-$clerk_host.key"
if jwks=$(curl -fsS --max-time 10 "https://$clerk_host/.well-known/jwks.json"); then
  node -e '
    const { keys } = JSON.parse(process.argv[1]);
    const key = require("crypto").createPublicKey({ key: keys[0], format: "jwk" });
    process.stdout.write(key.export({ type: "spki", format: "der" }).toString("base64"));
  ' "$jwks" >"$clerk_key"
elif [ -s "$clerk_key" ]; then
  log "warning: couldn't fetch Clerk's JWKS from $clerk_host; using the cached key"
else
  die "couldn't fetch Clerk's JWKS from https://$clerk_host/.well-known/jwks.json"
fi

# --- Runner base image (needed only by deploys) -----------------------------
version=$(node -p 'require("./dashboard/package.json").version')
base_image="${SECTR_RUNNER_BASE_IMAGE:-sectr-runner-base:v$version}"
docker image inspect "$base_image" >/dev/null 2>&1 \
  || log "warning: runner base image $base_image not found; deploys will fail until you build it:
    docker build -f docker/runner-base.Dockerfile -t $base_image ."

# --- sectr-server -----------------------------------------------------------
log "building sectr-server"
cargo build -p sectr-server
server_bin="${CARGO_TARGET_DIR:-$root/target}/debug/sectr-server"

server_env=(
  SECTR_PG_URL="postgres://sectr:sectr@127.0.0.1:$pg_port/sectr"
  SECTR_BIND_ADDR="0.0.0.0:$port" # 0.0.0.0: runner containers dial back in
  SECTR_SECRETS_MASTER_KEY="$(cat "$state/secrets-master-key")"
  SECTR_DOCKER_SOCKET="$(docker context inspect --format '{{.Endpoints.docker.Host}}' | sed 's|^unix://||')"
  SECTR_RUNNER_BASE_IMAGE="$base_image"
  SECTR_BUILD_DIR="$state/build"
  SECTR_ARTIFACTS_DIR="$state/artifacts"
  SECTR_CLERK_ISSUER="https://$clerk_host"
  SECTR_CLERK_JWT_KEY="$(cat "$clerk_key")"
  SECTR_CLERK_AUTHORIZED_PARTIES="$dashboard_origin"
  SECTR_DASHBOARD_URL="$dashboard_origin"
  SECTR_LOG_FORMAT=text
)
# Docker Desktop: containers reach the host via host.docker.internal (the
# server's default, the Linux bridge 172.17.0.1, doesn't work on macOS).
[ "$(uname)" != Darwin ] \
  || server_env+=(SECTR_RUNNER_SERVER_URL="ws://host.docker.internal:$port/ingest")

env "${server_env[@]}" "$server_bin" serve > >(awk '{ print "[server] " $0; fflush() }') 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null; wait "$server_pid" 2>/dev/null' EXIT
until curl -fsS "http://localhost:$port/healthz" >/dev/null 2>&1; do
  kill -0 "$server_pid" 2>/dev/null || die "sectr-server exited during startup (log above)"
  sleep 0.5
done
log "sectr-server up on http://localhost:$port"

# --- Dashboard --------------------------------------------------------------
cd dashboard
NEXT_PUBLIC_SECTR_API=/api NEXT_PUBLIC_SECTR_SERVER_URL="http://localhost:$port" SECTR_SERVER_URL="http://localhost:$port" ./node_modules/.bin/next dev
