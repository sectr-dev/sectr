---
title: Your first agent
description: The parts of every Sectr project — the manifest, the project and its agents, tools and approvals, running locally and deploying — whichever framework you use.
---

A Sectr project is an agent written with your framework of choice, plus a
small manifest and one line that serves the agent. This page walks
through those Sectr parts, using the support agent `sectr init` scaffolds: it
looks up orders and issues refunds, with a human approving each refund.
See [OpenAI Agents SDK](/getting-started/openai-agents/), [LangGraph](/getting-started/langgraph/) and
[Pydantic AI](/getting-started/pydantic-ai/) for code specific to your framework.

<!-- Click-to-load: the iframe shows a local thumbnail (srcdoc) and loads YouTube's privacy-enhanced player only on play, so the page itself sets no cookies. -->

<!-- skill: skip next — a video embed; nothing for an agent to read -->

<iframe
  src="https://www.youtube-nocookie.com/embed/H-y3wynkKc0?autoplay=1"
  srcdoc="<style>*{margin:0;padding:0;overflow:hidden}html,body{height:100%}a{display:block;height:100%}img{width:100%;height:100%;object-fit:cover}span{position:absolute;inset:0;margin:auto;width:72px;height:72px;border-radius:50%;background:rgba(0,0,0,.65);color:#fff;font:32px/72px sans-serif;text-align:center;padding-left:6px;box-sizing:border-box}a:hover span{background:#e8833a}</style><a href='https://www.youtube-nocookie.com/embed/H-y3wynkKc0?autoplay=1' aria-label='Play: How to deploy your first agent on Sectr'><img src='/videos/first-agent.jpg' alt=''><span>▶</span></a>"
  title="How to deploy your first agent on Sectr"
  allow="autoplay; encrypted-media; picture-in-picture; fullscreen"
  allowfullscreen
  loading="lazy"
  style="width:100%;aspect-ratio:16/9;border:0;border-radius:8px"
></iframe>

## Project setup

Scaffold a runnable project:

```sh
sectr init my-agent
```

`sectr init` asks two things:

- **The framework:** which agent framework to scaffold for: currently OpenAI Agents SDK, LangGraph or Pydantic AI.
- **A chat frontend:** none, or a CopilotKit chat app (Next.js, needs
  Node 20+) wired to the new agent.

<!-- skill: skip next — the tutorial's choice; SKILL.md tells the agent to pass --template -->

For this tutorial, we will select **OpenAI Agents SDK** and **CopilotKit**. See the [`sectr init` reference](/reference/cli/#sectr-init) for full options details.

This generates:

```text
my-agent/
├── agent/
│   ├── sectr.toml        # the Sectr manifest
│   ├── app.py            # your agent, and the line that serves it
│   ├── tools.py          # the agent's tools
│   ├── fake.py           # a mock model, used when no API key is set
│   ├── pyproject.toml
│   └── .env.example      # copy to .env to set OPENAI_API_KEY
├── app/                  # Next.js pages, the Copilot Runtime and the token route
├── proxy.ts
├── package.json
├── .env.development      # points the chat at `sectr dev`
└── README.md
```

## The manifest

`sectr.toml` tells Sectr what your project is:

```toml
# sectr.toml
[app]
name = "my-agent"

[agent]
entry = "app:app"
python = "3.12"
```

- **`[app] name`** names the project; its slug identifies it to `sectr deploy`
  and `sectr secrets`.
- **`[agent] entry`** is where your `AgentApp` lives, as `module:attribute`.

Optional keys come up later: `[deploy] approval_tools`, to
[gate tools by config](/guides/approvals/#marking-as-gated-in-deployment-config),
`[deploy] endpoint`, to [run it on your own compute](/guides/your-own-compute/),
and `[dev] port`, to [pin the local port](/guides/local-dev/#ports).

## Serving the agent

`app.py` defines your agent in ordinary framework code, then serves it
through an `AgentApp`, through the adapter for its framework:

```python
# app.py
from sectr import AgentApp

app = AgentApp(name="my-agent", env=["OPENAI_API_KEY"])
app.add_route("/chat", openai_agents(agent))
# or langgraph(graph), pydantic_ai(agent)
```

- **Each agent has its own path.** A project can have several agents, each served at a path like `/chat`.
- **`env`** lists the secrets the agent needs, by name. Their values come
  from `.env` locally and from `sectr secrets` when deployed (see
  [secrets](/guides/secrets/)).
- **The adapter** maps your framework's streamed output and approval requests
  onto Sectr's session events. It's the only Sectr-specific line: the agent
  itself is the code you already write for that framework.

Notice what's _absent_: no session handling, no streaming plumbing, no state
management, no database to persist conversations. Sectr keeps every session
in its journal, so a conversation survives restarts, redeploys and
scales-to-zero.

One rule applies whatever the framework: **your module must import cleanly
with no secrets set.** `sectr dev` and `sectr deploy` import your module to find
its agents before any secret is injected, so build anything that needs a key
(a model client, say) only when the key is present. The scaffolded
`fake.py` does that: with no `OPENAI_API_KEY`, a mock model answers, so the
first run needs no keys at all.

## Tools and approvals

<!-- skill: skip section — SKILL.md's "Defining Gated Tools" covers this -->

You write tools as you normally do in your agent framework. Mark one as **gated** (requiring approval) and Sectr pauses the
turn whenever the model calls it, until someone approves or denies the call;
then the agent carries on with the decision. The decision can come seconds or
weeks later, with Sectr scaling your compute down to zero and resuming durably as needed.
In the scaffolded agent, `refund` is gated:

```python
# tools.py
@function_tool(needs_approval=True)
async def refund(order_id: str) -> str:
    ...
```

How to mark a tool as gated depends on the framework:
see [approvals](/guides/approvals/#marking-as-gated-in-code) for more info.

## Run it locally

From the `agent/` directory, to use the real model locally, `cp .env.example .env`
and put your `OPENAI_API_KEY` in `.env`. Without it, the mock model answers.
Then start the local dev server:

```sh
cd my-agent/agent   # if not already there
sectr dev           # Opens up a live dashboard for local dev server
```

This opens the Sectr playground, the developer's view of the agent: chat with
it, watch its actions, and debug its behavior:

![Chat Playground in the dashboard](/screenshots/chat.png)

### Try the chat frontend

The playground is for the developer. End users talk to the agent through the
app's own frontend: here, the CopilotKit chat that `sectr init` scaffolded. Its packages are
already installed, and `.env.development` already points it at `sectr dev`.
Start it in a second terminal:

```sh
cd my-agent
npm run dev
```

Open [localhost:3000](http://localhost:3000) and try:

1. **Chat:** "Where is order 123?"
2. **Approve:** "Please refund it." The agent pauses on the gated `refund`
   tool. Approve or deny it in the chat.
3. **Reload:** the conversation lives on Sectr, so a reload picks it back up,
   approvals included.

If `sectr dev` picked a port other than 8420, put the URL it printed in
`my-agent/.env.local` as `SECTR_AGUI_URL=<that URL>`.

## Deploy your agent

To move your agent from your laptop to the cloud, run `sectr deploy` from
the `agent/` directory:

```sh
$ sectr deploy
packaging .
uploading to https://api.sectr.dev/deployments
deployment 01a0d532-d771-7563-96bd-56f40cb28b2b: version 1 — building…
✓ deployed: version 1 (01a0d532-d771-7563-96bd-56f40cb28b2b) is active
🚀 view live at: https://app.sectr.dev/apps/01a0d365-aa51-78d3-97f9-a2a02bcc00ce
🔌 connect via AG-UI at: https://api.sectr.dev/ag-ui/my-agent/chat
```

This just built your project in the cloud, found the agent, and assigned it a
live HTTP endpoint. The dashboard URL shows the deployed project. The last line
is the agent's [AG-UI](https://docs.ag-ui.com) endpoint, which your chat
frontend connects to.

Each `sectr deploy` makes a new version (deployment) of the project. New sessions run on the
newest version that built successfully; a session already under way keeps
the version it started on, so a deploy never interrupts a conversation.

Then give the deployed project its key, and chat with it:

```sh
sectr secrets set OPENAI_API_KEY
```

### Connect the chat to the deployed agent

<!-- skill: skip section — creating the key is a dashboard step; SKILL.md has an agent-facing version -->

The chat app is still talking to `sectr dev`. To point it at the deployed
agent, it needs two things: the AG-UI URL `sectr deploy` printed, and an
invocation key, which the app's server uses to mint a token for each user.

To create the key, open the project in the dashboard and pick **Connect your
frontend → CopilotKit via AG-UI**, then **Create invocation key**. (You can also
manage keys under **Settings → API keys**.) Put both in `my-agent/.env.local`:

```sh
# .env.local
SECTR_AGUI_URL=https://api.sectr.dev/ag-ui/my-agent/chat
SECTR_INVOCATION_KEY=<your invocation key>
```

Restart `npm run dev` and open [localhost:3000](http://localhost:3000) again:
the same chat now runs against your deployed agent. The key stays on the
app's server and never reaches the browser.

To ship the chat app itself, deploy the Next.js app anywhere Node runs
(Vercel, a container, `next start`) with those two variables set. See
[Connecting your frontend](/getting-started/connecting-frontend/) for more info.

Congratulations, you now have a cloud deployed agent, with a chat frontend for
your users!
