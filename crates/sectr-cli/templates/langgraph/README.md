# __SECTR_APP_NAME__ — a Sectr agent

Scaffolded by `sectr init` — a support agent on LangGraph, with a
human-in-the-loop approval built in.

## Run it locally (no keys needed)

```bash
sectr dev
```

The scripted mock (fake.py) answers the model — the first run works with
zero env vars. Send messages with `sectr send ...` (the dashboard opens
automatically and shows the live session).

## Go real (optional)

```bash
cp .env.example .env      # put your OPENAI_API_KEY in .env
```

Real runs use the standard OpenAI API (`gpt-4o-mini`).

## The approval flow

`refund` is a gated tool (`needs_approval=True`, a sectr tool converted
with `as_langgraph_tool`). When the agent calls it, the session **pauses**
and shows up in the dashboard approval inbox (or:
`sectr send --approve --session <id>`). Approve it and the same turn
resumes — that's Sectr's human-in-the-loop.

The graph has no checkpointer: Sectr's journal is the checkpoint. See
https://docs.sectr.dev/getting-started/langgraph/ for how LangGraph runs on
Sectr.

## Ship it

```bash
sectr deploy
```

Your agent becomes an HTTPS endpoint with durable sessions and SSE
streaming. To go real on the cloud, set the model key as a project
secret (values are write-only: they can be set, never read back):

```bash
sectr secrets set OPENAI_API_KEY
```

## Files

- `app.py` — the graph + the sectr touchpoints (AgentApp, one adapter line)
- `fake.py` — deterministic mock model (key-free demo)
- `tools.py` — one free tool, one approval-gated tool
- `sectr.toml` — app name, entrypoint, Python version
- `.env.example` — the optional real-model key
