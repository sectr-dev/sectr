"""LangGraph variant — the support agent as a StateGraph, no checkpointer.

The platform journal is the checkpoint: each Sectr turn is a fresh graph
invocation whose message state is rebuilt from the transcript. Tools are
sectr-registered (`sectr.tools.tool`, see tools.py — framework-free) and
converted to LangChain tools here with `as_langgraph_tool`, which keeps
approval gating: a gated call pauses the turn (TOOL_APPROVAL_REQUIRED), and
on the approval turn the graph resumes at its tool node, which runs the
approved call, and continues. See
sdk-python/src/sectr/adapters/langgraph.py for the full mapping.

Model providers:
- Offline CI: `SECTR_FAKE=1` selects the scripted chat model in fake.py
  (deterministic, no provider key).
- Real runs go through OpenRouter (one key, any provider model): put
  `OPENROUTER_API_KEY` in the project `.env` (declared on the AgentApp
  below, so the platform injects it into the runner), and optionally
  `SECTR_MODEL` (default z-ai/glm-5.3-flash).
"""

import os

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import SystemMessage
from langgraph.graph import END, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode

from sectr import AgentApp
from sectr.adapters.langgraph import as_langgraph_tool, langgraph

from tools import TOOLS

OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
DEFAULT_MODEL = "z-ai/glm-5.3-flash"

SYSTEM_PROMPT = (
    "You are a customer-support agent for a small shop. "
    "Keep a todo list of open issues with the todo tool."
)

lc_tools = [as_langgraph_tool(t) for t in TOOLS]

chat: BaseChatModel | None = None
if os.environ.get("SECTR_FAKE") == "1":
    from fake import scenario_model

    chat = scenario_model()
elif api_key := os.environ.get("OPENROUTER_API_KEY"):
    from langchain.chat_models import init_chat_model

    chat = init_chat_model(
        os.environ.get("SECTR_MODEL") or DEFAULT_MODEL,
        model_provider="openai",
        base_url=OPENROUTER_BASE_URL,
        api_key=api_key,
    )
# No key here? That's fine at INSPECT time (`sectr dev` imports this module
# before .env secrets are injected) — the import must succeed so routes can
# register. At RUNTIME the runner has OPENROUTER_API_KEY (declared below);
# call_model fails with a clear error if it is genuinely missing.
model = chat.bind_tools(lc_tools) if chat is not None else None


async def call_model(state: MessagesState) -> dict[str, list]:
    if model is None:
        raise RuntimeError(
            "no chat model configured: set OPENROUTER_API_KEY (or SECTR_FAKE=1 for the scripted model)"
        )
    response = await model.ainvoke([SystemMessage(SYSTEM_PROMPT), *state["messages"]])
    return {"messages": [response]}


def route_tools(state: MessagesState) -> str:
    last = state["messages"][-1]
    return "tools" if getattr(last, "tool_calls", None) else END


builder = StateGraph(MessagesState)
builder.add_node("agent", call_model)
builder.add_node("tools", ToolNode(lc_tools))
builder.add_conditional_edges("agent", route_tools, ["tools", END])
builder.add_edge("tools", "agent")
builder.set_entry_point("agent")
graph = builder.compile()  # no checkpointer — the journal is the checkpoint

app = AgentApp(name="support-agent", env=["OPENROUTER_API_KEY"])
app.add_route("/chat", langgraph(graph))
