# Releasing and CI

Human runbook for checks and releases. CI logic
lives in the `justfile`; GitHub workflows are thin wrappers around it.

> Releases are cut from the dev machine
> with `just release`; GitHub runs only the light `ci.yml` (`just check`), the
> advisory `release-check.yml` on `release/**` branches. There are no release or deploy workflows in `.github/`.

## Commands

| I want to... | Run |
| -- | -- |
| See what's available | `just` |
| Quick sanity check (what PRs/main run) | `just check` |
| Same, but ignore the diff and check every area | `just check all` (also `just test all`) |
| Check + fast unit tests (what pre-push runs) | `just test` |
| Run everything the release gate runs | `just verify` |
| Run one slice of that | `just verify rust\|python\|dashboard\|docs\|js\|conformance` |
| Run the adapter conformance cases | `just conformance` (`just conformance <filter>` runs a subset of the `adapter/endpoint/scenario/case` ids, e.g. `openai_agents` or `chat/two_turns`; a progress bar on a terminal, a line per case otherwise or with `--output plain`; then a matrix of known failures, the known failures grouped by issue, and the warnings: problems a case notes without failing, grouped by the issue they're filed under or flagged untracked, one example per kind; `--warnings <file>` writes every warning as JSON lines) |
| Regenerate the conformance projects and relock them | `just conformance-gen` |
| Run the self-hosted stack end to end | `just selfhost-smoke` (also part of the release's checks; builds the images unless `SECTR_SMOKE_REGISTRY` names a prefix that has them) |
| Check tools are installed | `just doctor` (`just doctor release` also checks publish/deploy tools and credentials) |
| Build all release artifacts, no publish | `just build` (`just build no-image` skips the docker image) |
| Cut a release | `just release patch\|minor\|major` (flags: `--dry-run`, `--require-gh`, `--skip-verify "<reason>"`, `--yes`; `just release --help` lists the stages) |
| Finish/retry an interrupted release (tag already pushed) | `just release-resume vX.Y.Z [stage]` |
| Back out before the tag is pushed | `just release-abort [vX.Y.Z]` |

**Tiers nest: `check` ⊂ `test` ⊂ `verify`.** `test` runs `check` first;
`verify` runs `test all` first. Never run two tiers back to back. Anything in an
inner tier must also pass under the outer ones.

- `check` is **path-aware**: `scripts/changed.sh` diffs against
  `origin/main` (override with `CHECK_BASE=<ref>`; `SECTR_CHANGED=all`
  forces every area — what `just check all` does) and skips untouched areas. It runs the version
  lockstep check and `cargo fmt --check` always, then per area:
  `cargo check` (Rust), byte-compile (Python SDK), contract conformance
  (`schema/`: pytest over the fixtures + the AG-UI golden streams, and the
  Node `@ag-ui/client` verifier), `tsc --noEmit` (dashboard), `npm ci` + typecheck of each npm package under `js/` (`js`,
  which also covers `schema/fixtures/valid/agui/`: the package tests serve
  them), and for `conformance` (`crates/sectr-conformance/`,
  `sdk-python/tests/conformance/`, `sdk-python/pyproject.toml`) that the
  generated conformance projects match the generator (`gen --check`) and
  their uv lock is current, and for `docs` (`docs-site/`, `crates/sectr-cli/`,
  the API spec sources) that the agent skill still generates cleanly from
  the docs (`check:skill`: no unsupported MDX, no dangling links). The
  docs drift checks need a built `sectr` binary and are release-only.
- `test` adds the Rust unit tests when Rust changed (including
  `schema/fixtures/` and `schema/data-model.sql`, which Rust tests read):
  it builds every target with every feature, which stands in for
  `check`'s `cargo check` (skipped under `test`, so the workspace compiles
  once), then runs `cargo test --workspace --lib --bins --all-features`
  from that build. Sequential on purpose: running the test binaries in
  parallel saved ~2 s but made the timing-sensitive worker tests flaky, and
  cargo-nextest (a process per test) starts a Postgres server per
  Postgres-backed test, which measured twice as slow. `test` also adds
  the SDK's pytest suite when Python changed, and the npm
  packages' suites (`npm test`: build + `node:test`) when `js` changed. Long
  end-to-end tests (docker, live servers) stay out of `test`. The Rust
  unit tests include `sectr-conformance`'s check that the generated
  conformance projects (`sdk-python/tests/conformance/gen/`) match what
  `just conformance-gen` would write; running the cases is in `verify`.
  Postgres-backed unit tests run if `initdb` is on PATH, else skip loudly.
- `verify` never does change detection: it always covers every area
  (it runs `test all`, then the full suite). It adds clippy (`-D warnings`), the full Rust suite (pgtemp +
  fake-gcs + docker), pytest, the adapter conformance cases (`just conformance`:
  every case on every adapter and endpoint, one per CPU at a time; about 3.5
  min on a 10-core Mac), the `schema/` conformance suites, dashboard/docs builds, the npm
  packages (typecheck, build, test), and the docs drift checks. On success it writes `.release/verified-<sha>`.

## One-time setup

1. `git config core.hooksPath .githooks` (pre-commit: fast static gates;
   pre-push: `just test`).
2. `brew install just uv node@22 postgresql@16 jq zig openssh` plus Docker Desktop
   (`openssh`: the hosted deploy refuses macOS's bundled ssh, which corrupts large uploads);
   Rust via rustup (the toolchain and cross targets are pinned by
   `rust-toolchain.toml`, Node by `.nvmrc`); `cargo install cargo-zigbuild
   --locked`; `dist` **0.33.0** (matches `cargo-dist-version` in
   `Cargo.toml`: https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.0).
3. To publish (phase 4 `just release`): `gh` (authed), `gcloud` (authed to
   push to Artifact Registry), `bws` with `BWS_ACCESS_TOKEN` in your
   keychain/env, `wrangler` (optional; `npx` otherwise), and the private ops repo checked out next to this one (`../sectr-ops`, or set
   `SECTR_OPS_DIR`), which holds the hosted deploy and its ssh access. Credentials are never stored in the repo: scripts read
   them from the environment first, then Bitwarden Secrets Manager by secret
   name (`PYPI_TOKEN`, `NPM_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CF_R2_TOKEN`, plus
   the hosted deploy's, listed in the ops repo). `just doctor release` checks every
   one, running the ops repo's own check for the hosted deploy.
4. `just doctor` (or `just doctor release`) — lists anything missing with a
   fix hint.

## Artifacts and release stages

`just build` writes everything to `target/release-artifacts/vX.Y.Z/`:
`dist/` (4 CLI tarballs + checksums, installer, `dist-manifest.json`),
`server/sectr-server` (aarch64, glibc 2.34, `--features e2b`), `sdk/`
(wheel + sdist), `npm/` (`sectr-copilotkit-X.Y.Z.tgz`, plus the
`sectr-0.0.0.tgz` shell, which is outside the lockstep), `image.txt` (local `linux/amd64` runner base image tag), and
`BUILT_FROM_SHA`. `just verify` builds them and then runs
`scripts/verify-artifacts.sh` (CLI `--version`, server runs under Oracle
Linux 9, wheel installs and imports and carries its MIT license metadata,
each npm tarball holds only `dist/` + README + LICENSE, is MIT at the right
version, and installs with its peers and imports, image has the SDK and a
linkable sidecar). The Linux CLI targets are cross-built from the Mac by `dist` via
`cargo-zigbuild`; the x86_64 base image needs Docker Desktop's Rosetta
emulation (or `docker buildx` pointed at an amd64 builder).

Publish stages are scripts in `scripts/`, each re-runnable and each honouring
`DRY_RUN=1` (prints what it would do): `publish-r2.sh` (then
`smoke-install.sh`; R2 is the only distribution channel, we publish no GitHub
Releases), `publish-sdk.sh`, `publish-npm.sh` (each tarball at its own
version, skipping any already on npm; auth via `NPM_TOKEN`, no
`--provenance` since that needs a CI identity), `publish-skill.sh` (the
agent skill, generated from the docs, pushed to the public
github.com/sectr-dev/skills repo with your `gh` login; a no-op when it's
already current), `publish-base-image.sh`, `smoke-docs.sh`, and `deploy-server.sh` in the ops repo. They are private recipes (`just publish-r2`
…), which `just release` sequences and which you can also run one at a time
from the dev machine (see "Deploying one component").

## Branching model

- `main` takes merges freely; PRs and pushes run only `just check` (one
  GitHub job).
- A release is a **short-lived** `release/vX.Y.Z` branch (hours to a day)
  that freezes code while `verify` runs. Green → tag → publish → merge back
  into `main` → delete the branch. Fix failures **on the release branch**
  only (commit there, re-run); the merge-back carries them to `main`. While
  the branch exists `main` keeps taking merges; they are not in this release.
- Once the tag is pushed the version is **burned**: if code must change,
  release the next patch. The publish stages are idempotent and re-run
  against the same tag with `just release-resume`.

## How to cut a release

1. Start from clean `main`, up to date (`git switch main && git pull`),
   `just doctor release` green, Docker Desktop running.
2. Optional rehearsal: `just release patch --dry-run`. It does everything up
   to and including the local tag (≈ 15–25 min, dominated by `just verify`
   and the artifact build), pushes nothing, and tells you to `just
   release-abort` when you are done.
3. `just release patch|minor|major`. Stages, in order (rough times, warm):
   `preflight` (checks) → `cut` (branch + bump commit) → `verify` (`just
   ci-full`, 10–25 min) → `tag` → *confirmation prompt* (`--yes` skips) →
   `push` → `merge-back` (main gets the bump; branch deleted) →
   `publish-r2` (+ `curl | sh` smoke check) → `publish-sdk` →
   `publish-npm` → `publish-skill` → `publish-base-image` → `deploy-server` (health-checked, on-VM rollback) →
   `smoke-docs` (≤ 5 min wait for Cloudflare). Progress is logged in
   `.release/vX.Y.Z.log`.
4. Optional second opinion: `--require-gh` pushes the branch before tagging
   and waits for a green `release-check.yml` run on the same SHA.
5. `--skip-verify "<reason>"` is for emergencies only; the reason is
   recorded in the tag annotation.

### After a release

- `curl -fsSL https://sectr.dev/install.sh | sh && sectr --version` shows the
  new version; `pip index versions sectr` lists it; `npm view
  @sectr/copilotkit version` shows it (and `npm view sectr version` shows
  the shell's `0.0.0`); `curl
  https://api.sectr.dev/healthz` reports it; docs.sectr.dev footer shows it.
- `git log origin/main` contains `release: vX.Y.Z` and `release/vX.Y.Z` is
  gone from origin.

## Failure playbook

- **Verify fails** (before the tag): you are left on `release/vX.Y.Z`. Fix
  the failure there (commit on the branch), then `just release-resume
  vX.Y.Z`: it re-runs `ci-full` on the new tip, tags, asks for confirmation
  and publishes. Nothing public exists yet. To start over instead: `just
  release-abort`.
- **Changed your mind before the tag is pushed:** `just release-abort`
  deletes the release branch and local tag (and the remote branch if
  `--require-gh` pushed it).
- **Failure after the tag push** (R2, PyPI, npm, image, server, docs): the output
  names the stage. Fix the cause and run `just release-resume vX.Y.Z` (it
  skips stages already logged as done) or `just release-resume vX.Y.Z
  <stage>` to re-run a stage and everything after it. Stages are idempotent:
  R2 overwrites, PyPI and npm skip an existing version, image pushes overwrite, the
  server deploy re-runs. A merge-back conflict stops at `merge-back`: resolve,
  `git commit`, then `just release-resume vX.Y.Z merge-back`. If your checkout
  no longer matches the tag, resume rebuilds the artifacts from a worktree at
  the tag.
- **Bad release:** the version is burned. Fix forward with the next patch. If
  the server is the problem, roll it back on the VM (see the ops repo); to re-point the base image, push the previous
  `:vX` as `:latest`.
- **Dev machine unavailable:** there is no GitHub fallback for building or
  deploying — by design there is one release path. Use another machine that
  has the setup above.

## Deploying one component

For an emergency fix to a single thing, run the stage from the dev machine
at the version you want live (artifacts come from `just build`, or are built
if missing): `just publish-base-image [vX.Y.Z]`, `just deploy-server
[vX.Y.Z]`, `just publish-sdk [vX.Y.Z]`, `just publish-npm [vX.Y.Z]`, `just
publish-r2 [vX.Y.Z]`. The agent skill isn't tied to a release at all: `just
publish-skill` brings github.com/sectr-dev/skills up to date with the docs
in your checkout whenever you want. Add
`DRY_RUN=1` to see exactly what would happen first, or `OPEN_PR=1` to open
a PR on the skills repo for review instead of pushing to its default branch. Agents must not run
these unless explicitly asked.
