# cloud-run — a Sectr agent

Scaffolded by `sectr init` — a support agent on Pydantic AI, with a
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

`refund` is a gated tool (`requires_approval=True`, Pydantic AI's native
deferred tools). When the agent calls it, the session **pauses** and shows
up in the dashboard approval inbox (or:
`sectr send --approve --session <id>`). Approve it and the same turn
resumes — that's Sectr's human-in-the-loop. Pydantic AI resumes from the
session transcript alone and runs the approved tool itself.

See https://docs.sectr.dev/getting-started/pydantic-ai/ for how Pydantic AI
runs on Sectr.

## Ship it

This agent runs on Google Cloud Run, in your own account: you deploy its image there, and Sectr wakes it for each turn. The steps, from this directory:

```text
1. gcloud run deploy cloud-run --source . --region REGION --project PROJECT_ID --allow-unauthenticated --timeout 3600 --concurrency 1
   (fill in your project, and a region near your Sectr server: hosted Sectr's is us-east4)
2. put the service URL it prints in sectr.toml ([deploy] endpoint)
3. sectr deploy    # registers it with Sectr (needs `sectr auth login` first)
```

Guide: https://docs.sectr.dev/guides/your-own-compute/

For a real model in the cloud, set its key as a project secret (values are write-only: they can be set, never read back). Sectr hands it to the agent at the start of each turn:

```bash
sectr secrets set OPENAI_API_KEY
```

Or set it on Google Cloud Run with its own secrets: the agent sees the environment it runs in.

## Files

- `app.py` — the agent + the sectr touchpoints (AgentApp, one adapter line)
- `fake.py` — deterministic mock model (key-free demo)
- `tools.py` — one free tool, one approval-gated tool
- `sectr.toml` — app name, entrypoint, Python version
- `.env.example` — the optional real-model key
