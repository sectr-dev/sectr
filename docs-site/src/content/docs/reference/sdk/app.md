---
title: AgentApp
description: The agent registry and manifest source — the one sectr object your project exposes.
---

`sectr.AgentApp` is the one object your project exposes: `[agent] entry` in
`sectr.toml` names it as `module:attribute`, and Sectr imports your module to
find it. [Serving the agent](/getting-started/first-agent/#serving-the-agent)
shows it in use.

## `AgentApp(*, name, env)`

- **`name`** — the project slug. It should match `[app] name` in `sectr.toml`.
- **`env`** — the names of the secrets the agent needs (see
  [secrets](/guides/secrets/#reaching-your-agent-code)).

## `app.add_route(path, handler)`

Serves `handler` on `path`. With a framework, the handler is the adapter's:
`openai_agents(agent)`, `langgraph(graph)` or `pydantic_ai(agent)` (see the
[adapter reference](/reference/sdk/adapters/)).

## `@app.agent_route(path)`

The decorator form, for a handler of your own: see
[no framework](/reference/sdk/no-framework/).

Paths must start with `/` and be unique in the project: Sectr directs sessions by
`(project, path)`. Registering a bad or duplicate path raises `ValueError`.
