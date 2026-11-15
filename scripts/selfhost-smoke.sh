#!/usr/bin/env bash
# selfhost-smoke.sh: the self-hosted stack end to end, for real. A compose stack
# from selfhost/ with local images, the demo app (no model key needed), and the
# real CLI: deploy, a turn that pauses for approval and resumes, secrets, the
# dashboard's routes, runner isolation, and a second stack on external Postgres.
#
# Usage: scripts/selfhost-smoke.sh   (or: just selfhost-smoke)
#
# Builds the three images unless SECTR_SMOKE_REGISTRY names a prefix that already
# has them: <prefix>/sectr-{runner-base,server,dashboard}:v<version>.
#
# With --release, runs once per architecture on the images `just build` made,
# which are the ones a release goes on to publish.
#
# Everything has a unique name and is removed afterward, so this can't touch a
# real install on the same Docker daemon. Set SECTR_SMOKE_KEEP=1 to keep a failed
# stack for debugging.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

ver=$(scripts/set-version.sh --check | sed -n 's/^all manifests agree at //p')
[ -n "$ver" ] || { echo "selfhost-smoke: the version manifests disagree" >&2; exit 1; }

if [ "${1:-}" = --release ]; then
  . scripts/lib.sh
  for arch in "${RELEASE_ARCHS[@]}"; do
    prefix="sectr-release-smoke-$arch"
    for image in "${RELEASE_IMAGES[@]}"; do docker tag "$image:v$ver-$arch" "$prefix/$image:v$ver"; done
    log "$arch"
    status=0
    SECTR_SMOKE_REGISTRY="$prefix" "$0" || status=$?
    for image in "${RELEASE_IMAGES[@]}"; do docker rmi "$prefix/$image:v$ver" >/dev/null; done
    [ "$status" = 0 ] || exit "$status"
  done
  exit 0
fi

id="smoke-$$"
tmp="$(mktemp -d)"
project="sectr-$id"
project_ext="sectr-$id-ext"
runners="sectr-$id-runners"
runners_ext="sectr-$id-ext-runners"
external_pg="sectr-$id-external-pg"
deployments="$tmp/deployments"
watcher=""
failed=1

log() { printf '\n== %s\n' "$*"; }
ok() { printf '  ok: %s\n' "$*"; }
die() { printf 'selfhost-smoke: FAILED: %s\n' "$*" >&2; exit 1; }

compose_main() { docker compose -p "$project" --project-directory "$tmp/main" -f "$tmp/main/compose.yml" "$@"; }
compose_ext() { docker compose -p "$project_ext" --project-directory "$tmp/ext" -f "$tmp/ext/compose.yml" "$@"; }

cleanup() {
  [ -n "$watcher" ] && kill "$watcher" 2>/dev/null
  if [ "$failed" = 1 ]; then
    echo "--- server log (main stack) ---" >&2
    compose_main logs --tail 40 sectr-server >&2 2>/dev/null || true
    # When Postgres dies, its name stops resolving, so the server only reports
    # a failed lookup. The reason is here (a full Docker disk, for one).
    echo "--- postgres log (main stack) ---" >&2
    compose_main logs --tail 10 postgres >&2 2>/dev/null || true
    if [ -n "${SECTR_SMOKE_KEEP:-}" ]; then
      echo "kept: docker compose -p $project, $tmp" >&2
      return
    fi
  fi
  for network in "$runners" "$runners_ext"; do
    # Any runner still attached would stop the network being removed.
    docker ps -aq --filter "network=$network" | xargs -r docker rm -f >/dev/null 2>&1 || true
  done
  docker rm -fv "$external_pg" >/dev/null 2>&1 || true
  compose_main down -v --remove-orphans >/dev/null 2>&1 || true
  compose_ext down -v --remove-orphans >/dev/null 2>&1 || true
  if [ -f "$deployments" ]; then
    while read -r image; do docker image rm "$image" >/dev/null 2>&1 || true; done <"$deployments"
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
env_value() { sed -n "s/^$2=//p" "$1/.env"; }

# --- Images -----------------------------------------------------------------
if [ -n "${SECTR_SMOKE_REGISTRY:-}" ]; then
  registry="$SECTR_SMOKE_REGISTRY"
else
  registry="sectr-smoke"
  log "building images"
  docker build -q -f docker/runner-base.Dockerfile -t "$registry/sectr-runner-base:v$ver" .
  docker build -q -f docker/Dockerfile.server -t "$registry/sectr-server:v$ver" .
  docker build -q -f docker/Dockerfile.dashboard -t "$registry/sectr-dashboard:v$ver" .
fi
for image in sectr-runner-base sectr-server sectr-dashboard; do
  docker image inspect "$registry/$image:v$ver" >/dev/null 2>&1 || die "no local image $registry/$image:v$ver"
done

cargo build -q -p sectr-cli
sectr="${CARGO_TARGET_DIR:-$root/target}/debug/sectr"
export XDG_CONFIG_HOME="$tmp/cli-config"
# No anonymous usage pings from the smoke's CLI: a test run isn't an install.
export SECTR_TELEMETRY=off
unset SECTR_API_URL SECTR_PLATFORM_KEY SECTR_DASHBOARD_URL

# --- A stack from what a user gets ------------------------------------------
# setup.sh on a copy of selfhost/, then only the settings that keep this stack
# apart from anything else on the daemon.
new_stack_dir() {
  local dir="$1" network="$2" server_port="$3" dashboard_port="$4"
  mkdir -p "$dir"
  cp selfhost/compose.yml selfhost/setup.sh selfhost/.env.example "$dir/"
  "$dir/setup.sh" >"$dir/setup.out"
  [ "$(stat -f '%Lp' "$dir/.env" 2>/dev/null || stat -c '%a' "$dir/.env")" = 600 ] || die ".env isn't private"
  cat >>"$dir/.env" <<ENV
SECTR_REGISTRY=$registry
SECTR_RUNNER_NETWORK=$network
SECTR_SERVER_PORT=127.0.0.1:$server_port
SECTR_DASHBOARD_PORT=127.0.0.1:$dashboard_port
SECTR_PUBLIC_API_URL=http://localhost:$server_port
SECTR_DASHBOARD_URL=http://localhost:$dashboard_port
ENV
}

wait_ready() {
  local url="$1"
  for _ in $(seq 1 120); do
    curl -fsS "$url" >/dev/null 2>&1 && return 0
    sleep 1
  done
  die "$url never answered"
}

# --- Helpers for driving turns ----------------------------------------------
new_session() { # api key app route
  curl -fsS -X POST "$1/sessions" -H "Authorization: Bearer $2" -H 'Content-Type: application/json' \
    -d "{\"app\":\"$3\",\"route\":\"$4\",\"input\":\"hi\"}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["session_id"])'
}

# The session's events so far: the SSE tail replays the journal from the start.
events() { curl -s -N --max-time 2 -H "Authorization: Bearer $2" "$1/sessions/$3/events" || true; }

wait_for_event() { # api key session event_type count
  local seen=0
  for _ in $(seq 1 60); do
    seen=$(events "$1" "$2" "$3" | grep -c "^data:.*\"$4\"" || true)
    [ "$seen" -ge "$5" ] && return 0
    sleep 1
  done
  die "session $3 reached $seen of $5 $4 events"
}

runners_of() { docker ps -a --filter "label=sectr.session=$1" --format '{{.Names}}'; }

deploy_app() { # directory api
  local out
  out=$(cd "$1" && "$sectr" deploy --api "$2" 2>&1) || { echo "$out" >&2; die "deploy of $1 failed"; }
  printf '%s\n' "$out"
  local deployment version
  deployment=$(printf '%s\n' "$out" | sed -n 's/.*deployed: version \([0-9]*\) (\([^)]*\)).*/\2/p')
  version=$(printf '%s\n' "$out" | sed -n 's/.*deployed: version \([0-9]*\) (\([^)]*\)).*/\1/p')
  echo "sectr-app-$deployment:v$version" >>"$deployments"
}

# --- The main stack ---------------------------------------------------------
log "starting the stack with the bundled Postgres"
server_port=$(free_port)
dashboard_port=$(free_port)
api="http://localhost:$server_port"
dashboard="http://localhost:$dashboard_port"
new_stack_dir "$tmp/main" "$runners" "$server_port" "$dashboard_port"
key=$(env_value "$tmp/main" SECTR_BOOTSTRAP_PLATFORM_KEY)
grep -q "$key" "$tmp/main/setup.out" || die "setup.sh didn't print the key"
"$tmp/main/setup.sh" >/dev/null 2>&1 && die "setup.sh overwrote an existing .env"
ok "setup.sh refuses to overwrite .env"
compose_main up -d
wait_ready "$api/readyz"
ok "/readyz answers"

log "dashboard"
wait_ready "$dashboard/login"
curl -fsS -o /dev/null "$dashboard/api/healthz" || die "dashboard /api rewrite"
curl -fsS "$dashboard/runtime-config.js" | grep -qF "\"publicApiUrl\":\"$api\"" || die "/runtime-config.js has the wrong URL"
ok "/login, the /api rewrite and /runtime-config.js"

log "CLI login and deploy"
"$sectr" auth login --api "$api" --key "$key" 2>&1 | grep -qF "logged in to $api" || die "login"
grep -qF "dashboard = \"$dashboard\"" "$XDG_CONFIG_HOME/sectr/credentials.toml" || die "login didn't save the dashboard"
cp -R examples/localhost-demo "$tmp/demo"
out=$(deploy_app "$tmp/demo" "$api")
printf '%s\n' "$out" | grep -qF "server: $api" || die "deploy didn't print its server"
printf '%s\n' "$out" | grep -qF "view live at: $dashboard/apps/" || die "deploy's link isn't this dashboard's"
ok "deployed, with this install's links"

log "a turn: pause for approval, resume, fresh container"
session=$(new_session "$api" "$key" localhost-demo /assist)
(while :; do runners_of "$session" >>"$tmp/runner-names"; sleep 0.1; done) &
watcher=$!
wait_for_event "$api" "$key" "$session" TOOL_APPROVAL_REQUIRED 1
ok "the stream reached TOOL_APPROVAL_REQUIRED"
curl -fsS -o /dev/null -X POST "$api/sessions/$session/approval" -H "Authorization: Bearer $key" \
  -H 'Content-Type: application/json' -d '{"approved":true}'
wait_for_event "$api" "$key" "$session" NO_MORE_ACTIONS 1
ok "approving resumed it to NO_MORE_ACTIONS"
for _ in $(seq 1 30); do [ -z "$(runners_of "$session")" ] && break; sleep 1; done
kill "$watcher" 2>/dev/null || true
wait "$watcher" 2>/dev/null || true
watcher=""
[ -z "$(runners_of "$session")" ] || die "a runner remains after the turn"
names=$(sort -u "$tmp/runner-names" | grep -c . || true)
[ "$names" -eq 2 ] || die "expected a fresh container per turn, saw $names"
ok "two turns, two containers, none left"

log "secrets and the runner's environment"
mkdir -p "$tmp/probe"
cat >"$tmp/probe/sectr.toml" <<'TOML'
[app]
name = "smoke-probe"

[agent]
entry = "main:app"
TOML
cat >"$tmp/probe/main.py" <<'PY'
import json
import os
from collections.abc import AsyncIterator
from uuid import uuid4

from sectr import AgentApp, SessionContext
from sectr.events import MessageDelta, MessageEnd, MessageStart, RunnerEvent

app = AgentApp(name="smoke-probe")


@app.agent_route("/probe")
async def probe(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    report = {
        "uid": os.getuid(),
        "secret": os.environ.get("SMOKE_SECRET"),
        "sectr_vars": sorted(k for k in os.environ if k.startswith("SECTR_") and k != "SECTR_PORT" and k != "SECTR_APPROVAL_POLICY"),
    }
    msg_id = uuid4().hex[:16]
    yield MessageStart(role="assistant", message_id=msg_id)
    yield MessageDelta(message_id=msg_id, delta={"type": "text", "text": "PROBE " + json.dumps(report)})
    yield MessageEnd(message_id=msg_id)
PY
deploy_app "$tmp/probe" "$api" >/dev/null
(cd "$tmp/probe" && "$sectr" secrets set SMOKE_SECRET=smoke-value --api "$api" >/dev/null 2>&1) || die "secrets set"
session=$(new_session "$api" "$key" smoke-probe /probe)
wait_for_event "$api" "$key" "$session" NO_MORE_ACTIONS 1
report=$(events "$api" "$key" "$session" | grep -m1 -o 'PROBE {[^}]*}' | sed 's/\\"/"/g; s/^PROBE //') || true
[ -n "$report" ] || die "the probe reported nothing"
python3 - "$report" <<'PY' || die "the runner's environment is wrong: $report"
import json, sys
report = json.loads(sys.argv[1])
assert report["uid"] == 1000, report
assert report["secret"] == "smoke-value", report
assert not [v for v in report["sectr_vars"] if "PORT" not in v], report
PY
ok "the secret arrives, uid 1000, and no token or OTLP vars"

log "network isolation"
base="$registry/sectr-runner-base:v$ver"
if docker run --rm --network "$runners" "$base" python3 -c \
  'import socket; socket.create_connection(("postgres", 5432), 3)' >/dev/null 2>&1; then
  die "a container on the runner network can reach Postgres"
fi
docker run --rm --network "$runners" "$base" python3 -c \
  'import socket; socket.create_connection(("sectr-server", 8080), 3)' >/dev/null 2>&1 \
  || die "a container on the runner network can't reach the server"
ok "runners reach the server and not Postgres"

log "external Postgres, with the bundled one off"
compose_main down -v >/dev/null 2>&1
ext_server_port=$(free_port)
ext_dashboard_port=$(free_port)
ext_api="http://localhost:$ext_server_port"
new_stack_dir "$tmp/ext" "$runners_ext" "$ext_server_port" "$ext_dashboard_port"
external_password=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
docker run -d --name "$external_pg" -e POSTGRES_USER=external -e POSTGRES_PASSWORD="$external_password" -e POSTGRES_DB=external postgres:16 >/dev/null
for _ in $(seq 1 60); do docker exec "$external_pg" pg_isready -U external -d external >/dev/null 2>&1 && break; sleep 1; done
cat >>"$tmp/ext/.env" <<ENV
COMPOSE_PROFILES=
SECTR_PG_URL=postgres://external:$external_password@$external_pg:5432/external
ENV
compose_ext create
docker network connect "${project_ext}_sectr-internal" "$external_pg"
compose_ext up -d
wait_ready "$ext_api/readyz"
[ -z "$(compose_ext ps -a -q postgres 2>/dev/null)" ] || die "the bundled Postgres started with its profile off"
ext_key=$(env_value "$tmp/ext" SECTR_BOOTSTRAP_PLATFORM_KEY)
"$sectr" auth login --api "$ext_api" --key "$ext_key" >/dev/null 2>&1 || die "login to the external-Postgres stack"
deploy_app "$tmp/demo" "$ext_api" >/dev/null
session=$(new_session "$ext_api" "$ext_key" localhost-demo /assist)
wait_for_event "$ext_api" "$ext_key" "$session" TOOL_APPROVAL_REQUIRED 1
ok "a turn runs on external Postgres"

failed=0
log "selfhost-smoke passed"
