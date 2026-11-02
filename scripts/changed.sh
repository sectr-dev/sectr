#!/usr/bin/env bash
# changed.sh — print the areas touched relative to a base ref, one per line.
#
# Usage: scripts/changed.sh [base-ref]   (default: origin/main)
#
# Areas: rust python schema dashboard docs js conformance. Compares the merge base with
# HEAD plus uncommitted changes. If the base can't be resolved (shallow
# clone, no remote), prints ALL areas — fail open, never skip checks by
# accident. SECTR_CHANGED=all|none|"rust docs" overrides (testing/forcing).
set -euo pipefail
cd "$(dirname "$0")/.."

ALL="rust python schema dashboard docs js conformance"
if [[ -n "${SECTR_CHANGED:-}" ]]; then
  case "$SECTR_CHANGED" in
    all) tr ' ' '\n' <<<"$ALL" ;;
    none) ;;
    *) tr ' ' '\n' <<<"$SECTR_CHANGED" ;;
  esac
  exit 0
fi

base="${1:-origin/main}"
if ! mb=$(git merge-base "$base" HEAD 2>/dev/null); then
  tr ' ' '\n' <<<"$ALL"
  exit 0
fi
files=$( { git diff --name-only "$mb" HEAD; git diff --name-only HEAD; git ls-files --others --exclude-standard; } | sort -u)

has() { grep -Eq "$1" <<<"$files"; }
# Rust also covers the schema/ files Rust tests read: the fixtures (golden
# event + AG-UI streams) and the canonical data model (SQLite migrations).
has '^(crates/|Cargo\.(toml|lock)$|rust-toolchain\.toml$|\.cargo/|schema/fixtures/|schema/data-model\.sql$)' && echo rust
has '^sdk-python/' && echo python
has '^schema/' && echo schema
has '^dashboard/' && echo dashboard
# docs-site plus its drift-check inputs (CLI help from sectr-cli, spec copy).
has '^(docs-site/|crates/sectr-cli/|schema/(public-api\.yaml|sessions-api\.yaml|event\.schema\.json)$|Cargo\.(toml|lock)$)' && echo docs
# The npm packages, plus the AG-UI golden streams @sectr/copilotkit's tests serve.
has '^(js/|schema/fixtures/valid/agui/)' && echo js
# The conformance generator, its generated projects, and the SDK's
# dependencies (the generated projects' lock resolves them).
has '^(crates/sectr-conformance/|sdk-python/tests/conformance/|sdk-python/pyproject\.toml$)' && echo conformance
exit 0
