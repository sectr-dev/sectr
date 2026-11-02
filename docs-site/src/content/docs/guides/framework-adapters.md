---
title: Choosing a framework
description: Which agent frameworks Sectr supports, how each is served, and which to pick.
---

<!-- skill: skip file — SKILL.md includes its support matrix; the rest is for a human choosing a framework -->

Your agent stays ordinary framework code. The `sectr` SDK's adapter for the
framework is the one line that serves it:

```python
app.add_route("/chat", openai_agents(agent))
```

## Support matrix

| Framework | Status | Serve it with | Add to a project |
| --- | --- | --- | --- |
| [OpenAI Agents SDK](/getting-started/openai-agents/) | Supported | `openai_agents(agent)`, from `sectr.adapters.openai_agents` | `uv add "sectr[openai-agents]"` |
| [LangGraph](/getting-started/langgraph/) | Supported, for ReAct-shaped graphs | `langgraph(graph)`, from `sectr.adapters.langgraph` | `uv add "sectr[langgraph]"` |
| [Pydantic AI](/getting-started/pydantic-ai/) | Supported, for a plain `pydantic_ai.Agent` | `pydantic_ai(agent)`, from `sectr.adapters.pydantic_ai` | `uv add "sectr[pydantic-ai]"`, plus a model provider such as `"pydantic-ai-slim[openai]"` |
| [No framework](/reference/sdk/no-framework/) | Supported | your own handler, `@app.agent_route("/chat")`, with tools from [`sectr.tools`](/reference/sdk/tools/) | `uv add sectr` |

Each adapter supports most, not all, of its framework: the
[adapter reference](/reference/sdk/adapters/) lists what each one does and
its limits (subagents, structured output, checkpointers and so on).

## Which to pick

- **The OpenAI Agents SDK** if your agent is already built on it. Start with
  [using the OpenAI Agents SDK](/getting-started/openai-agents/).
- **LangGraph** if your agent is already a LangGraph graph, or you want
  LangChain's model and tool ecosystem. ReAct-shaped graphs are supported;
  start with [using LangGraph](/getting-started/langgraph/).
- **Pydantic AI** if your agent is a `pydantic_ai.Agent`, or you want typed
  tools and the broadest model-provider support. Start with
  [using Pydantic AI](/getting-started/pydantic-ai/).
- **No framework** if you'd rather write the agent loop yourself: see
  [no framework](/reference/sdk/no-framework/).

The Claude Agent SDK has no adapter yet; it's on the backlog.
