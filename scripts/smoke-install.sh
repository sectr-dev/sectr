#!/usr/bin/env bash
# The real user path: run the R2-hosted installer from dl.sectr.dev/<prefix>/,
# run `sectr --version`, assert it matches VERSION. This subsumes URL checks
# (the installer bakes its download URLs at build time, so a stale artifact
# 404s on its first tarball fetch). Installs into ~/.local/bin and REMOVES it
# afterwards — it will clobber an existing `sectr` there.
# Usage: scripts/smoke-install.sh VERSION PREFIX   (PREFIX: latest | vX.Y.Z)
set -euo pipefail
. "$(dirname "$0")/lib.sh"
ver="${1:?version}"; prefix="${2:?prefix}"
export PATH="$HOME/.local/bin:$PATH"
rm -f "$HOME/.local/bin/sectr"
curl -fsSL "https://dl.sectr.dev/$prefix/sectr-cli-installer.sh" | sh
got="$(sectr --version)"
echo "  $prefix: $got"
rm -f "$HOME/.local/bin/sectr"
[[ "$got" == *"$ver"* ]] || die "expected sectr $ver, got: '$got'"
