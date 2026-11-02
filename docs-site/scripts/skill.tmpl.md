---
name: sectr
description: Build, run locally, and deploy AI agents on Sectr, a PaaS for hosting AI agents. Covers the sectr CLI (init, dev, send, tail, deploy, secrets, auth); the sectr Python SDK (AgentApp and the adapters for the OpenAI Agents SDK, LangGraph and Pydantic AI); human-in-the-loop tool approvals; connecting a frontend over AG-UI (CopilotKit); and the public HTTP API (sessions, SSE journal streaming, approvals, AG-UI endpoints, limits). Use when writing or debugging a Sectr agent, integrating the sectr SDK, deploying to Sectr Cloud, putting a chat UI on a deployed agent, driving deployed agents over the HTTP API, or working with Sectr concepts like journals, turns, and approvals.
---

# Sectr

Sectr is a PaaS for hosting AI agents: develop with any agent framework,
debug locally with `sectr dev`, and deploy to Sectr Cloud with
`sectr deploy`. Agents are written with the OpenAI Agents SDK, LangGraph,
Pydantic AI, or no framework at all (see "Supported agent frameworks"
below). Every deployed agent
is also an AG-UI endpoint, so a CopilotKit chat (or any AG-UI client)
connects to it directly. Deployed projects run under concurrency and rate
limits (`references/api/limits.md`).

The runtime is built around a few invariants that shape everything
else — read these first, the details below all follow from them:

- **The journal is the single source of truth.** A session is an
  append-only event log; every live view (the chat stream, the dashboard,
  `sectr tail`) is a tail of that journal. Client SSE is always served by
  tailing the journal, never proxied from a runner.
- **Runners are stateless.** Your agent code is a pure function of
  `(transcript, reason)` — every turn re-invokes the handler with the full
  transcript rebuilt from the journal. State that should survive belongs
  in the conversation itself (journaled tool results), not in memory.
- **Sessions are turn-based.** A turn starts with a `user_message`
  invocation and ends at the `NO_MORE_ACTIONS` sentinel — which the
  platform appends itself; emitting it from user code is an error.
- **Approvals pause the turn.** A gated tool call suspends the turn
  (no turn-end sentinel is journaled); the human's decision resumes it as
  a fresh invocation whose reason is `approval_decision`.

## Before anything else: check that Sectr is set up

Do this before running any `sectr` command, and fix whatever is missing.
Never assume it's already installed.

1. **The `sectr` CLI.** Run `sectr --version`.
   - If it prints a version, go on.
   - If the command isn't found, run `~/.local/bin/sectr --version`. The
     installer puts `sectr` there, but a shell started before the install
     doesn't have it on its `PATH`. If that works, run
     `export PATH="$HOME/.local/bin:$PATH"` and go on.
   - Otherwise install it:
     `curl -fsSL https://sectr.dev/install.sh | sh`, then
     `export PATH="$HOME/.local/bin:$PATH"`, then check `sectr --version`
     again.
2. **`uv`.** `sectr init` and `sectr dev` run the agent's Python with uv.
   Run `uv --version`. If it isn't found, install it with
   `curl -LsSf https://astral.sh/uv/install.sh | sh`, put
   `$HOME/.local/bin` on the `PATH` as above, and check again.
3. **A Sectr project.** A project is a directory with a `sectr.toml`.
   - If there's none and the user wants a new agent, create one with
     `sectr init <name> --template <framework>`, where the framework is
     `openai-agents`, `langgraph` or `pydantic-ai`. Always pass
     `--template`: without it `sectr init` asks questions in the terminal,
     which you can't answer. Add `--frontend copilotkit` only if the user
     wants a chat UI.
   - If the user has an existing Python agent, add the SDK for its
     framework instead: the `uv add` command in "Supported agent
     frameworks" below.
4. **Logged in, only before a cloud command** (`sectr deploy`,
   `sectr secrets`, `sectr send --cloud`; local `sectr dev` doesn't need
   it). Run `sectr auth whoami`. If it fails or prints
   `no stored credentials`, run `sectr auth login --no-browser` yourself.
   It prints a link and a code, then waits up to 10 minutes for the user
   to approve, so run it in the background (or with a long timeout).
   Show the user the link and code from its output and ask them to open
   the link, sign in if asked, check the code matches and click Approve.
   When the command finishes, run `sectr auth whoami` again.

   Use `--no-browser` because a browser you open may not be the user's.
   The key is a credential and never passes through the conversation:
   don't ask the user to paste one, and don't use `--key` yourself.

## Supported agent frameworks

Sectr runs agents written with these. Each framework's agent is served
through an adapter from the `sectr` SDK:

{{include guides/framework-adapters#support-matrix}}

## Conceptual overview: how a Sectr project fits together

This section walks through the parts every Sectr project has, whatever its
framework: the `sectr.toml` manifest, serving the agent, tools and
approvals, running locally, and deploying. It uses the support agent that
`sectr init` scaffolds (it looks up orders and issues refunds, with a human
approving each refund) as the example.

**Read it to understand Sectr, not as steps to follow.** Don't scaffold a
project, copy this example, or run these commands unless the user asked
for that. In an existing project, find the same pieces in the user's own
code (`sectr.toml`, the `AgentApp` and its agents, their tools) and work
from those.

{{weave getting-started/first-agent content}}

#### Connect the chat to the deployed agent

The chat app is still talking to `sectr dev`. To point it at the deployed
agent, the frontend's `.env.local` (in `my-agent/`, next to `package.json`)
needs two variables:

- `SECTR_AGUI_URL`: the AG-UI URL `sectr deploy` printed
  (`🔌 connect via AG-UI at: …`). Set this one yourself.
- `SECTR_INVOCATION_KEY`: an invocation key, which the app's server uses to
  mint a token for each user. **Don't create or handle this key yourself.**
  Ask the user to create one in the dashboard (open the project, then
  **Connect your frontend → CopilotKit via AG-UI → Create invocation key**,
  or **Settings → API keys**) and add it to `.env.local` as
  `SECTR_INVOCATION_KEY` themselves, without pasting it into the
  conversation.

Then restart `npm run dev`: the same chat runs against the deployed agent.
To ship the chat app, deploy the Next.js app anywhere Node runs with both
variables set (see `references/getting-started/connecting-frontend.md`).

## Defining Gated Tools

Gate a tool when it shouldn't run unattended (issuing refunds, sending
emails, calling paid APIs): the turn suspends at the call, a human approves
or denies it, and the turn resumes with the decision. A tool is gated if
either its code or the deployment's config says so.

{{weave guides/approvals content}}

## Running and debugging an agent headlessly

Test an agent you write yourself, from the terminal: no browser or
dashboard needed. Run these from the agent's project directory (the one
with `sectr.toml`).

1. **Start the local engine** in the background: `sectr dev --no-open`.
   It keeps running and reloads the agent code on the next turn, so leave
   it up while you edit. If its port is busy it moves to the next free one
   and says so; `sectr send` and `sectr tail` find it either way.
2. **Send a message:** `sectr send "Where is order 123?"`. It prints
   `session: <id>`, then follows the turn until it ends or pauses on an
   approval. Add `--json` for the raw event envelopes, one per line.
3. **Continue the conversation:** `sectr send "Please refund it." --session <id>`.
4. **Decide a pending approval:** `sectr send --session <id> --approve`
   (or `--deny`). The turn resumes, and `sectr send` follows it.
5. **Watch a session's whole journal:** `sectr tail --session <id>`
   (`--json` works here too). It never exits on its own, so run it in the
   background or with a timeout.

Details: `references/guides/local-dev.md` (the dev loop, and
[ports](references/guides/local-dev.md#ports)), and the CLI reference for
[`sectr dev`](references/cli.md#sectr-dev),
[`sectr send`](references/cli.md#sectr-send) and
[`sectr tail`](references/cli.md#sectr-tail).

## Reference

Deeper material lives in `references/`, one file per docs page:

{{references}}
