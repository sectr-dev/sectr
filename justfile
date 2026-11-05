# Sectr task runner — the single source of truth for checks (and, later,
# releases). GitHub workflows are thin wrappers that call these recipes.
# Human runbook: RELEASING.md.
#
# Tiers nest:  check ⊂ test ⊂ verify   (test runs check, verify runs test)

set shell := ["bash", "-euo", "pipefail", "-c"]

# Base ref that path-aware checks diff against (CI overrides per event).
base := env_var_or_default("CHECK_BASE", "origin/main")
pg_bin := `ls -d /opt/homebrew/opt/postgresql@16/bin /usr/local/opt/postgresql@16/bin /usr/lib/postgresql/*/bin 2>/dev/null | head -1 || true`

# List the public commands.
default:
    @just --list --unsorted

# Quick sanity check: what PRs/main run. Path-aware; `just check all` ignores the diff and checks every area.
check scope="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{scope}}" in
      "")  just check-fast ;;
      all) SECTR_CHANGED=all just check-fast ;;
      *) echo "unknown scope '{{scope}}' (only: all)" >&2; exit 2 ;;
    esac

# check + fast unit tests: what the pre-push hook runs. `just test all` covers every area.
test scope="":
    #!/usr/bin/env bash
    set -euo pipefail
    # test-unit builds every Rust target with every feature, which covers
    # check's `cargo check`: skip it rather than compile the workspace twice.
    export SECTR_SKIP_CARGO_CHECK=1
    just check {{scope}}
    if [ "{{scope}}" = all ]; then SECTR_CHANGED=all just test-unit; else just test-unit; fi

# Everything the release gate runs, no change detection. `just verify [rust|python|dashboard|docs|js|conformance]` runs one slice.
verify area="": (test "all")
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{area}}" in
      "")        just ci-full ;;
      rust)      just check-rust-full ;;
      python)    just check-python ;;
      dashboard) just check-dashboard ;;
      docs)      just check-docs ;;
      js)        just check-js ;;
      conformance) just conformance ;;
      *) echo "unknown area '{{area}}' (rust|python|dashboard|docs|js|conformance)" >&2; exit 2 ;;
    esac

# Build all release artifacts locally (CLI x4, server, SDK wheel, npm packages, base image) into target/release-artifacts/vX.Y.Z/. No publish. `just build no-image` skips the docker image.
build opt="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{opt}}" in
      "")       scripts/release-build.sh ;;
      no-image) scripts/release-build.sh --no-image ;;
      *) echo "unknown option '{{opt}}' (only: no-image)" >&2; exit 2 ;;
    esac

# Cut a release: just release patch|minor|major [--dry-run] [--require-gh] [--skip-verify "<reason>"] [--yes]. `just release --help` lists the stages. Outward-facing: agents must not run it unasked.
release *args:
    scripts/release.sh {{args}}

# Continue a release: just release-resume vX.Y.Z [stage]. After the tag was pushed it finishes/retries the publish stages; before the tag it re-runs verify on release/vX.Y.Z (after you fix the failure there), then tags and publishes.
release-resume *args:
    scripts/release.sh resume {{args}}

# Back out a release before its tag is pushed (deletes the release branch and local tag).
release-abort version="":
    scripts/release.sh abort {{version}}

# Verify required tools are installed, with fix hints. `just doctor release` also checks release/publish tools and credentials.
doctor tier="":
    scripts/doctor.sh "{{tier}}"

# Adapter conformance harness: run the cases against the generated projects, then print a summary. `just conformance <filter>` runs the cases whose adapter/endpoint/scenario/case id contains <filter>; `--output plain` prints a line per case instead of the progress bar. Needs uv.
conformance *args:
    cargo run -q -p sectr-conformance -- run {{args}}

# Regenerate the conformance projects under sdk-python/tests/conformance/gen and relock their uv workspace.
conformance-gen:
    cargo run -q -p sectr-conformance -- gen
    cd sdk-python/tests/conformance/gen && uv lock

# ---------------------------------------------------------------- internals

[private]
check-fast:
    #!/usr/bin/env bash
    set -euo pipefail
    areas=" $(scripts/changed.sh {{base}} | tr '\n' ' ') "
    echo "==> changed areas:${areas}"
    just check-version
    scripts/check-releasing-docs.sh
    cargo fmt --check
    has() { [[ "$areas" == *" $1 "* ]]; }
    if has rust && [ -z "${SECTR_SKIP_CARGO_CHECK:-}" ]; then cargo check --workspace --all-targets --all-features; fi
    if has python; then just check-python-fast; fi
    if has schema; then just check-schema; fi
    if has dashboard; then just check-dashboard-fast; fi
    if has js; then just check-js-fast; fi
    if has conformance; then just check-conformance-fast; fi
    if has examples; then just check-examples; fi
    if has docs; then just check-docs-fast; fi
    # The other docs drift checks need a built sectr binary: release-only (see `verify docs`).

[private]
test-unit:
    #!/usr/bin/env bash
    set -euo pipefail
    areas=" $(scripts/changed.sh {{base}} | tr '\n' ' ') "
    # Build every target with every feature (what check's `cargo check`
    # covers: the e2b/gcp code prod runs, integration tests, examples), then
    # run the lib and bin tests from that same build: no integration tests.
    # SECTR_SKIP_DOCKER_E2E skips the real-container tests (they need Linux
    # docker + a built image). Postgres-backed unit tests run when initdb is
    # on PATH and skip loudly otherwise (test_pg.rs).
    if [[ "$areas" == *" rust "* ]]; then
      cargo test --workspace --all-targets --all-features --no-run
      SECTR_SKIP_DOCKER_E2E=1 PATH="{{pg_bin}}:$PATH" cargo test --workspace --lib --bins --all-features
    fi
    # The whole SDK suite: in-process, no network or containers.
    if [[ "$areas" == *" python "* ]]; then
      (cd sdk-python && uv run --extra openai-agents --extra langgraph --extra pydantic-ai pytest -q)
    fi
    # The npm packages' suites (build + node:test against local servers).
    if [[ "$areas" == *" js "* ]]; then
      for d in js/*/; do (cd "$d" && npm run --if-present test); done
    fi

[private]
check-version:
    scripts/set-version.sh --check

[private]
check-python-fast:
    cd sdk-python && uv sync --extra openai-agents --extra langgraph --extra pydantic-ai && uv run python -m compileall -q src

# The generated conformance projects match the generator, and their lock is current.
[private]
check-conformance-fast:
    cargo run -q -p sectr-conformance -- gen --check
    cd sdk-python/tests/conformance/gen && uv lock --check

# Each example's lock matches its project and the SDK. A stale lock deploys and
# then crashes at runner start, because the image installs from the lock.
[private]
check-examples:
    for d in examples/*/; do if [ -f "$d/uv.lock" ]; then (cd "$d" && uv lock --check); fi; done

[private]
check-dashboard-fast:
    cd dashboard && npm ci --no-audit --no-fund && npm run typecheck && npm test

# The agent skill still generates cleanly from the docs (no unsupported MDX, no dangling links).
[private]
check-docs-fast:
    cd docs-site && npm ci --no-audit --no-fund && npm run check:skill

# Every npm package under js/: install + typecheck.
[private]
check-js-fast:
    for d in js/*/; do (cd "$d" && npm ci --no-audit --no-fund && npm run typecheck); done

[private]
services-up:
    docker compose -f docker-compose.test.yml up -d fake-gcs-server

[private]
services-down:
    docker compose -f docker-compose.test.yml down

[private]
check-rust-full:
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="{{pg_bin}}:$PATH"
    export SECTR_TEST_GCS_URL="${SECTR_TEST_GCS_URL:-http://localhost:4443}"
    just services-up
    trap 'just services-down' EXIT
    docker pull postgres:16 >/dev/null
    # The daemon-gated tests find a non-default socket (Docker Desktop's)
    # only through DOCKER_HOST, and otherwise skip without saying so.
    export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{"{{"}}.Endpoints.docker.Host{{"}}"}}')}"
    cargo fmt --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features

[private]
check-python:
    cd sdk-python && uv sync --extra openai-agents --extra langgraph --extra pydantic-ai && uv run pytest -q

# Contract conformance (schema/): fixtures vs the JSON Schemas/OpenAPI, plus the
# AG-UI endpoint's golden streams vs the vendored upstream schema (pytest) and
# the real @ag-ui/client stream verifier + reducer (node).
[private]
check-schema:
    cd schema && uv run pytest -q
    cd schema/agui-conformance && npm ci --no-audit --no-fund && npm test

[private]
check-dashboard:
    cd dashboard && npm ci --no-audit --no-fund && npm test && npm run build

# The npm packages: typecheck, build, test.
[private]
check-js:
    for d in js/*/; do (cd "$d" && npm ci --no-audit --no-fund && npm run typecheck && npm run build && npm run --if-present test); done

# Drift checks regenerate CLI help from the debug sectr binary (built here).
[private]
check-docs:
    cargo build -p sectr-cli
    cd docs-site && npm ci --no-audit --no-fund && npm run check:cli && npm run check:spec && npm run check:skill && npm run build

# Full suite on the current tree, then build + verify the release artifacts.
# On success writes .release/verified-<sha> (the release
# gate's marker) — but only for a clean tree, since the marker vouches for
# exactly the committed SHA.
[private]
ci-full: check-version check-rust-full check-python check-examples conformance check-schema check-dashboard check-docs check-js release-build verify-artifacts
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${E2E_STAGING:-}" = 1 ]; then just e2e-staging; fi
    if [ -n "$(git status --porcelain)" ]; then
      echo "ci-full passed, but the tree is dirty: no verification marker written (commit first)" >&2
      exit 0
    fi
    mkdir -p .release
    touch ".release/verified-$(git rev-parse HEAD)"
    echo "verified $(git rev-parse --short HEAD)"

# Deferred hook: end-to-end tests against a live staging server.
[private]
e2e-staging:
    @echo "e2e-staging: not implemented"

# ------------------------------------------------------------ release stages
# Implementation lives in scripts/ (also called by the GitHub fallback
# workflows). Outward-facing ones honour DRY_RUN=1. AGENTS.md: agents must not
# run publish-* / deploy-* unless the user asks.

[private]
release-build:
    scripts/release-build.sh

[private]
verify-artifacts:
    scripts/verify-artifacts.sh

# latest="latest" also points latest/ at this version.
[private]
publish-r2 version="" latest="":
    #!/usr/bin/env bash
    set -euo pipefail
    args=(); [ "{{latest}}" = latest ] && args+=(--latest)
    [ -n "{{version}}" ] && args+=("{{version}}")
    scripts/publish-r2.sh "${args[@]}"

[private]
publish-sdk version="":
    scripts/publish-sdk.sh {{version}}

# Each npm package at its own version; skips versions already on npm.
[private]
publish-npm version="":
    scripts/publish-npm.sh {{version}}

# The agent skill, generated from the docs, to github.com/sectr-dev/skills (`npx skills add sectr-dev/skills`); not tied to a release.
[private]
publish-skill version="":
    scripts/publish-skill.sh {{version}}

[private]
publish-base-image version="":
    scripts/publish-base-image.sh {{version}}

[private]
deploy-server version="":
    #!/usr/bin/env bash
    set -euo pipefail
    ops="${SECTR_OPS_DIR:-../sectr-ops}"
    if [ ! -x "$ops/scripts/deploy-server.sh" ]; then
      echo "deploy-server: $ops/scripts/deploy-server.sh not found. It deploys the hosted service from the private ops repo; set SECTR_OPS_DIR if it's elsewhere." >&2
      exit 1
    fi
    SECTR_DIR="{{justfile_directory()}}" "$ops/scripts/deploy-server.sh" {{version}}
