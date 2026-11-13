# AGENTS.md: Sectr repo guide

## What this is

Sectr stores, streams and resumes AI agent conversations. It's the database and
coordinator between an agent's frontend and its compute. Read `schema/README.md`
first. The contracts in `schema/` are authoritative: components conform to them
and never redefine them.

## Key invariants (do not break)

- The journal is the single source of truth. Client SSE is always served by
  tailing the journal, never proxied from a runner.
- The runner (user process) is stateless and receives the transcript in the
  invoke request. It never touches the journal or platform credentials.
- Per-session `seq` numbers are assigned by the journal writer (the server's
  ingest path), never by user code.
- Sessions are turn-based. A turn starts with a `user_message` invocation and
  ends at `NO_MORE_ACTIONS`. Approval pauses suspend the turn, and a resume is a
  fresh invocation with `reason: approval_decision`.
- All cross-component types derive from or validate against `schema/` artifacts.
- Whether a runner tears down at a turn boundary is one policy choice in shared
  code. Backends describe their platform and don't implement their own boundary
  behavior.

## Public repo rules

This repo is public, so everything here must be fit to publish, including
commit messages and PR text.

- No tracker ids or tracker links in code, comments, docs, commits, or PR titles
  and bodies. A `pre-commit` gate rejects them in staged content. Explain the
  reason in plain language instead. Branch names may carry a tracker id.
- No plan files or internal design docs. Contract rules that contributors need go
  in `schema/README.md`, and other reasoning goes in a comment beside the code.
- The hosted service's deployment, infrastructure and business material live in a
  private ops repo. Nothing here may depend on it, except `just deploy-server`,
  which says clearly if it's missing.

## Conventions

- Rust: workspace deps declared in the root `Cargo.toml`; errors via `thiserror`
  (libs) and `anyhow` (binaries); `tracing` for logging.
- Rust: shipped code builds JSON from `Serialize` structs, never `json!`, which
  `clippy.toml` bans outside tests. Shapes that cross components (server and
  CLI, server and sidecar) live in `sectr-common` so both sides share them.
- Python: `uv`-managed, `src/` layout, Python 3.12, pytest.

## Releasing and CI

CI logic lives in the `justfile`, and GitHub workflows are thin wrappers. The
command table, tiers (`just check` ⊂ `just test` ⊂ `just verify`) and the release
runbook are in `RELEASING.md`. Read it before touching CI or releases, and update
it in any PR that changes the `justfile`, `scripts/release*` or release workflows.
Agents must NOT run `just release`, `just release-resume` or any `publish-*` /
`deploy-*` recipe unless the user explicitly asks (outward-facing, hard to
reverse). There are no release or deploy workflows in GitHub: releases and
emergency deploys run from the dev machine.

## Git hooks

Activate after cloning: `git config core.hooksPath .githooks`.

- `pre-commit`: fast static gates (rustfmt, workflow YAML, secret scan, tracker
  ids, blob size). Agents: the hook is the check. Don't hand-run these gates before
  committing, and don't work around it.
- `pre-push`: runs `just test` (check + unit tests, path-aware). The last run's full
  output is in `.git/pre-push.log`: read it there after a failed push rather than
  re-running or filtering the push's own output.
- `--no-verify` is reserved for reviewed exceptions (for example, committing a
  known-large intentional file), always with the reason in the commit message. Slow
  gates (clippy, full tests, builds) live in `just verify` on the release branch, so
  don't run them preemptively.
