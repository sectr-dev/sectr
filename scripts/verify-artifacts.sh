#!/usr/bin/env bash
# Assert the artifacts from release-build.sh actually work:
#   - host-arch CLI tarball: `sectr --version` == lockstep version
#   - sectr-server: right architecture, and `--help` runs (aarch64 glibc 2.34
#     userland via docker, or natively on an aarch64 Linux host)
#   - SDK wheel installs into a fresh venv and imports at the right version,
#     and carries its MIT license (License-Expression + the license file)
#   - npm tarballs: only dist/ README.md LICENSE package.json inside, MIT,
#     the right version; each installs (with its peers) and imports, and
#     @sectr/copilotkit's SectrRunner subclasses the peer's AgentRunner
#     (and SectrAgent the peer's HttpAgent)
#   - images, on each architecture: the image and its binary are that
#     architecture. Base: SDK imports at the right version, sidecar links
#     cleanly. Server: `--help`. Dashboard: starts and serves its runtime
#     config, and holds no sharp
#   - the dashboard's Cloudflare Worker bundles (wrangler deploy --dry-run)
set -euo pipefail
. "$(dirname "$0")/lib.sh"

ver=$(scripts/version.sh)
out=$(artifacts_dir "$ver")
need_artifacts "$ver"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*" >&2; fail=1; }

echo "CLI tarball (host arch)"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  t=aarch64-apple-darwin ;;
  Darwin-x86_64) t=x86_64-apple-darwin ;;
  Linux-x86_64)  t=x86_64-unknown-linux-gnu ;;
  Linux-aarch64) t=aarch64-unknown-linux-gnu ;;
  *) t="" ;;
esac
if [ -z "$t" ]; then bad "unknown host $(uname -sm)"; else
  mkdir "$tmp/cli"; tar -xJf "$out/dist/sectr-cli-$t.tar.xz" -C "$tmp/cli" --strip-components=1
  got=$("$tmp/cli/sectr" --version) || got=""
  [[ "$got" == *"$ver"* ]] && ok "sectr --version: $got" || bad "sectr --version '$got' != $ver"
fi

echo "sectr-server"
bin="$out/server/sectr-server"
desc=$(file "$bin")
[[ "$desc" == *ELF*aarch64* ]] && ok "aarch64 ELF" || bad "not an aarch64 ELF: $desc"
if [ "$(uname -s)-$(uname -m)" = Linux-aarch64 ]; then
  "$bin" --help >/dev/null && ok "--help (native)" || bad "--help failed"
elif docker info >/dev/null 2>&1 && [ "$(uname -m)" != x86_64 -o "$(uname -s)" = Darwin ]; then
  # Oracle Linux 9 = the production userland (glibc 2.34).
  docker run --rm --platform linux/arm64 -v "$out/server:/s:ro" oraclelinux:9-slim /s/sectr-server --help >/dev/null \
    && ok "--help (oraclelinux:9, arm64)" || bad "--help failed under oraclelinux:9"
else
  echo "  skip --help: no way to run aarch64 here (x86_64 Linux host)"
fi

echo "SDK wheel"
whl=$(ls "$out"/sdk/*.whl)
uv venv -q "$tmp/venv"
uv pip install -q --python "$tmp/venv/bin/python" "$whl"
got=$("$tmp/venv/bin/python" -c 'import importlib.metadata as m, sectr; print(m.version("sectr"))') || got=""
[ "$got" = "$ver" ] && ok "wheel imports, version $got" || bad "wheel version '$got' != $ver"
python3 - "$whl" <<'EOF' && ok "wheel is MIT (License-Expression + LICENSE)" || bad "wheel license metadata missing"
import sys, zipfile
names = zipfile.ZipFile(sys.argv[1]).namelist()
meta = zipfile.ZipFile(sys.argv[1]).read(next(n for n in names if n.endswith(".dist-info/METADATA"))).decode()
assert "License-Expression: MIT" in meta.splitlines(), "no License-Expression: MIT"
assert any(n.endswith(".dist-info/licenses/LICENSE") for n in names), "no licenses/LICENSE"
EOF

# check_npm TGZ NAME VERSION — the tarball's contents and manifest.
check_npm() {
  local tgz="$1" name="$2" want="$3" extra
  [ -f "$tgz" ] || { bad "missing $tgz"; return 1; }
  extra=$(tar tzf "$tgz" | grep -Ev '^package/(dist/.+|README\.md|LICENSE|package\.json)$' || true)
  [ -z "$extra" ] && ok "$name: only dist/, README.md, LICENSE, package.json" || bad "$name: unexpected files: $extra"
  got=$(tar xzf "$tgz" -O package/package.json | jq -r '"\(.name) \(.version) \(.license)"')
  [ "$got" = "$name $want MIT" ] && ok "$name: version $want, MIT" || bad "$name: manifest says '$got', want '$name $want MIT'"
}

# npm_project DIR PKG... — a fresh ESM project with PKG... installed.
npm_project() {
  local dir="$1"; shift
  mkdir -p "$dir" && echo '{"name": "verify", "private": true, "type": "module"}' > "$dir/package.json"
  (cd "$dir" && npm install --no-audit --no-fund --loglevel=error "$@" >&2)
}

echo "npm: @sectr/copilotkit"
tgz="$out/npm/sectr-copilotkit-$ver.tgz"
if check_npm "$tgz" @sectr/copilotkit "$ver"; then
  # Its peers at the versions its tests ran against (devDependencies).
  peers=$(jq -r '.peerDependencies | keys[] as $k | "\($k)@\($ARGS.named.dev[$k])"' --argjson dev "$(jq .devDependencies js/copilotkit/package.json)" js/copilotkit/package.json)
  # shellcheck disable=SC2086
  if npm_project "$tmp/npm-copilotkit" "$tgz" $peers; then
    cat > "$tmp/npm-copilotkit/check.mjs" <<'EOF'
import { HttpAgent } from "@ag-ui/client";
import { AgentRunner } from "@copilotkit/runtime/v2";
import { SectrAgent, SectrRunner } from "@sectr/copilotkit";
const agent = new SectrAgent({ url: "http://127.0.0.1:9/" });
if (!(agent instanceof HttpAgent)) throw new Error("SectrAgent is not an HttpAgent");
const runner = new SectrRunner({ agents: { default: agent } });
if (!(runner instanceof AgentRunner)) throw new Error("SectrRunner is not an AgentRunner");
EOF
    (cd "$tmp/npm-copilotkit" && node check.mjs) && ok "installs with its peers; SectrAgent is an HttpAgent, SectrRunner an AgentRunner" || bad "@sectr/copilotkit import check failed"
  else bad "@sectr/copilotkit: npm install failed"; fi
fi

echo "npm: sectr (shell, outside the lockstep)"
shell_ver=$(jq -r .version js/sectr/package.json)
tgz="$out/npm/sectr-$shell_ver.tgz"
if check_npm "$tgz" sectr "$shell_ver"; then
  if npm_project "$tmp/npm-sectr" "$tgz"; then
    (cd "$tmp/npm-sectr" && node -e 'import("sectr").then(() => {})') && ok "installs and imports" || bad "sectr import failed"
  else bad "sectr: npm install failed"; fi
fi

# elf_machine IMAGE PLATFORM PATH — a binary's ELF machine field, as hex. Emulation
# runs either architecture, so running a binary doesn't show which one it is.
elf_machine() { docker run --rm --platform "$2" --entrypoint sh "$1" -c "od -An -tx1 -j18 -N2 $3" | tr -d ' \n'; }

if [ -f "$out/image.txt" ]; then
  for arch in "${RELEASE_ARCHS[@]}"; do
    platform="linux/$arch"
    case "$arch" in amd64) machine=3e00 ;; arm64) machine=b700 ;; esac
    for image in "${RELEASE_IMAGES[@]}"; do
      grep -qx "$image:v$ver-$arch" "$out/image.txt" || bad "$image:v$ver-$arch is not in image.txt"
      got=$(docker image inspect "$image:v$ver-$arch" --format '{{.Architecture}}') || got=""
      [ "$got" = "$arch" ] || bad "$image:v$ver-$arch is '$got'"
    done

    img="sectr-runner-base:v$ver-$arch"
    echo "$img"
    got=$(docker run --rm --platform "$platform" "$img" python -c 'import importlib.metadata as m, sectr; print(m.version("sectr"))') || got=""
    [ "$got" = "$ver" ] && ok "SDK in image, version $got" || bad "image SDK version '$got' != $ver"
    # The sidecar has no --help (it starts serving from env), so check it links.
    docker run --rm --platform "$platform" --entrypoint sh "$img" -c \
      'test -x /usr/local/bin/sectr-worker && ! ldd /usr/local/bin/sectr-worker | grep -q "not found"' \
      && ok "sectr-worker present, libraries resolve" || bad "sectr-worker missing or has unresolved libraries"
    [ "$(elf_machine "$img" "$platform" /usr/local/bin/sectr-worker)" = "$machine" ] \
      && ok "sectr-worker is $arch" || bad "sectr-worker is not $arch"

    img="sectr-server:v$ver-$arch"
    echo "$img"
    docker run --rm --platform "$platform" "$img" --help >/dev/null \
      && ok "--help" || bad "$img --help failed"
    [ "$(elf_machine "$img" "$platform" /usr/local/bin/sectr-server)" = "$machine" ] \
      && ok "sectr-server is $arch" || bad "sectr-server is not $arch"

    img="sectr-dashboard:v$ver-$arch"
    echo "$img"
    docker run --rm --platform "$platform" --entrypoint sh "$img" -c \
      'test ! -e node_modules/sharp && test ! -e node_modules/@img' \
      && ok "no sharp or libvips (LGPL)" || bad "sharp or libvips is back in the image"
    id=$(docker run -d --platform "$platform" -p 127.0.0.1::3000 -e SECTR_PUBLIC_API_URL=https://verify.example "$img")
    port=$(docker port "$id" 3000/tcp | head -1 | sed 's/.*://')
    got=""
    for _ in $(seq 1 60); do
      got=$(curl -fsS -m 3 "http://127.0.0.1:$port/runtime-config.js" 2>/dev/null) && break
      sleep 1
    done
    [[ "$got" == *'"publicApiUrl":"https://verify.example"'* ]] \
      && ok "starts and serves its runtime config" || bad "dashboard did not start: $(docker logs --tail 5 "$id" 2>&1)"
    docker rm -f "$id" >/dev/null
  done
fi

echo "dashboard Worker (app.sectr.dev)"
# What deploy-dashboard uploads: wrangler bundles it as it would for the
# deploy, with no credentials and no upload.
DRY_RUN=1 scripts/deploy-dashboard.sh "$ver" >"$tmp/worker.log" 2>&1 \
  && ok "bundles for Cloudflare" || bad "the dashboard Worker doesn't bundle: $(tail -5 "$tmp/worker.log")"

[ $fail = 0 ] && echo "artifacts verified" || { echo "artifact verification FAILED" >&2; exit 1; }
