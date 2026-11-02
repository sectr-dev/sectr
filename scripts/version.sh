#!/usr/bin/env bash
# Prints the lockstep version (e.g. 0.0.7); fails if the manifests disagree.
set -euo pipefail
"$(dirname "$0")/set-version.sh" --check | awk 'NR == 1 { print $2 }'
