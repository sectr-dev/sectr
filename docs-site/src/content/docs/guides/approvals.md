---
title: Approvals (human in the loop)
description: Gate dangerous tools behind explicit approval decisions — locally and via API.
---

Some actions shouldn't run unattended: issuing refunds, sending emails,
calling paid APIs. Sectr makes those tools **gated**: the turn suspends at
the call and a human decides, then the turn resumes with the decision.

## Marking as gated in code

Gate a tool where it's defined. Which way depends on how the agent is
written:

- **OpenAI Agents SDK or Pydantic AI: use the framework's own approval
  flag.** Sectr's adapter turns the framework's approval request into a
  Sectr pause and resumes the framework with the decision, so the tool stays
  ordinary framework code. These adapters read the framework's flag, not
  `sectr.tools`.
- **LangGraph: use one of the options below, not LangGraph's
  `interrupt()`.** `interrupt()` can't be resumed without a checkpointer, so
  it fails on Sectr.
- **No framework: use `sectr.tools`.**

A tool inside a subagent (another agent your agent runs from a tool) can't
be gated: gate the subagent tool itself, and approving it runs the whole
subagent. Each [adapter's page](/reference/sdk/adapters/) shows how, under
"Subagents".

### OpenAI Agents SDK

```python
from agents import function_tool

@function_tool(needs_approval=True)
async def refund(order_id: str) -> str: ...
```

### Pydantic AI

```python
from pydantic_ai import Agent, Tool

agent = Agent(model, tools=[lookup_order, Tool(refund, requires_approval=True)])
```

### LangGraph

Any of these, and they can be mixed:

- A Sectr tool, converted for a `ToolNode` or `create_agent`:

    ```python
    from sectr.adapters.langgraph import as_langgraph_tool
    from sectr.tools import tool

    @tool(needs_approval=True)
    async def refund(order_id: str) -> str:
        """Refund an order."""
        ...

    tools = [as_langgraph_tool(refund)]
    ```

- Any LangChain tool (a `@tool`, an MCP tool, a community tool), gated as
  it's converted: `as_langgraph_tool(some_tool, needs_approval=True)`.
- On `create_agent`, LangChain's own `HumanInTheLoopMiddleware`. Each
  `interrupt_on` entry must allow both approve and reject (`True` does):

    ```python
    from langchain.agents import create_agent
    from langchain.agents.middleware import HumanInTheLoopMiddleware

    graph = create_agent(
        model,
        tools=tools,
        middleware=[HumanInTheLoopMiddleware(interrupt_on={"refund": True})],
    )
    ```

Sectr pauses all of these itself (see the
[LangGraph adapter](/reference/sdk/adapters/langgraph/#behavior)).

### No framework

A handler written directly against the SDK (`@app.agent_route`) gates its
tools with [`sectr.tools`](/reference/sdk/tools/):

```python
from sectr.tools import tool

@tool(needs_approval=True)
async def refund(order_id: str) -> str:
    """Refund an order."""
    ...
```

## Marking as gated in deployment config

You can also gate tools without touching the code: list them under
`approval_tools` in `sectr.toml`.

```toml
[deploy]
approval_tools = ["refund", "delete_account"]
```

These tools pause for approval even if the code doesn't gate them, MCP
tools included. This allows declarative, policy-directed gating: which
tools need approval is configuration, set per deployment rather than in
the agent's code. The list applies in `sectr dev` and when deployed. A change takes
effect the next time you start `sectr dev` or run `sectr deploy`.

Sectr hands the list to your runner as the `SECTR_APPROVAL_POLICY`
environment variable, which the SDK reads. Don't set that variable yourself:
`approval_tools` is the only place to set the policy.

## Deciding

<!-- skill: skip section — deciding is the frontend's or operator's job, not part of writing agent code -->

**From the CLI** (`sectr dev` / cloud with `--cloud`):

```sh
sectr send --approve   # resume the turn with "approved"
sectr send --deny      # resume with the rejection
```

**From the dashboard** — the session banner or the approvals inbox, one
click each.

**From your own UI** — `POST /sessions/{id}/approval`, the endpoint the
dashboard itself uses (a user token works too, for sessions its user owns):
see [approve or deny actions](/reference/api/approvals/). In a CopilotKit
chat, approvals arrive as interrupts (see
[connecting your frontend](/getting-started/connecting-frontend/)).

If one model response calls several gated tools, they are approved one at a
time: you decide the first, and the turn pauses again on the next.

## What happens on resume

<!-- skill: skip section — internal details an agent doesn't need -->

The decision is journaled, and the agent runs again with it, as a new turn
whose reason is the decision. With a framework adapter, the framework
resumes on its own: an approved call runs exactly once, and a denied one
gets the standard rejection text. A handler without a framework does it
itself: see [resuming after an approval](/reference/sdk/no-framework/#resuming-after-an-approval).
