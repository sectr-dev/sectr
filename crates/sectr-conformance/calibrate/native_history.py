"""Run one framework natively (no Sectr) for two user turns, keeping the
conversation the framework's own way, against the conformance mock: what
each SDK sends back on the next turn is the reference the `reasoning`
cases' later turns hold Sectr's rebuild to.

Driven by `examples/native_history.rs`, which serves the mock model and
answers each request; the model client is the generated projects' own
(`endpoint.py`). Not run directly:

    cargo run -q -p sectr-conformance --example native_history

`shape` is `text` (turn 1 is one response with reasoning and text) or
`tool` (reasoning and a `lookup_order` call, then the answer).

Last run 2026-10-13: on `openrouter`, `openai_responses` and `anthropic`,
every SDK sends turn 1's reasoning back on turn 2, after text and after a
tool loop alike, signed as it is within the tool loop (openai-agents on
`openrouter` as text only, everything else signed).
"""

import asyncio
import os
import sys

ADAPTER, SHAPE = sys.argv[1], sys.argv[2]
GEN = os.path.join(os.path.dirname(__file__), "../../../sdk-python/tests/conformance/gen")
sys.path.insert(0, os.path.join(GEN, ADAPTER, "01_chat"))

import endpoint  # noqa: E402

INSTRUCTIONS = "You are a customer-support agent for a small shop."
TURNS = {"text": ("Hi!", "Nothing, thanks."), "tool": ("Where is order 42?", "Thanks!")}


async def lookup_order(order_id: str) -> str:
    """Look up the status of an order."""
    return "shipped"


async def pydantic_ai() -> None:
    from pydantic_ai import Agent

    agent = Agent(endpoint.model(), instructions=INSTRUCTIONS, tools=[lookup_order])
    first, second = TURNS[SHAPE]
    result = await agent.run(first)
    await agent.run(second, message_history=result.all_messages())


async def langgraph() -> None:
    from langchain.agents import create_agent
    from langgraph.checkpoint.memory import InMemorySaver

    graph = create_agent(
        endpoint.model(), tools=[lookup_order], system_prompt=INSTRUCTIONS, checkpointer=InMemorySaver()
    )
    config = {"configurable": {"thread_id": "1"}}
    for ask in TURNS[SHAPE]:
        await graph.ainvoke({"messages": [{"role": "user", "content": ask}]}, config)


async def openai_agents() -> None:
    from agents import Agent, Runner, function_tool

    agent = Agent(
        name="native",
        instructions=INSTRUCTIONS,
        model=endpoint.model(),
        model_settings=endpoint.model_settings(),
        tools=[function_tool(lookup_order)],
    )
    first, second = TURNS[SHAPE]
    result = await Runner.run(agent, first)
    await Runner.run(agent, result.to_input_list() + [{"role": "user", "content": second}])


asyncio.run({"pydantic_ai": pydantic_ai, "langgraph": langgraph, "openai_agents": openai_agents}[ADAPTER]())
