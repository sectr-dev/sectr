"""Your Sectr agent — the support agent on LangGraph.

The graph is plain `langchain.agents.create_agent` code with no
checkpointer: Sectr's journal is the checkpoint. Each turn is a fresh graph
run whose messages are rebuilt from the session transcript, and an approval
pause ends the turn; on approval the adapter runs the approved call and the
graph continues from the answered conversation. The tools are sectr tools
(see tools.py) converted with `as_langgraph_tool`, which is what makes
`refund` approval-gated. The only other sectr touchpoints are `AgentApp`
and one adapter line.

Model selection:
- `OPENAI_API_KEY` set → gpt-4o-mini on the standard OpenAI API.
- No key → the scripted model in fake.py answers, so `sectr dev` works
  with ZERO env vars.
"""

import os

from langchain.agents import create_agent
from langchain_core.language_models.chat_models import BaseChatModel
from sectr import AgentApp
from sectr.adapters.langgraph import as_langgraph_tool, langgraph

from fake import ScriptedChatModel
from tools import TOOLS

# If OPENAI_API_KEY is set, uses gpt-4o-mini, otherwise uses a mock model. To set OPENAI_API_KEY:
# - For local `sectr dev`, cp .env.example .env and put in your key.
# - For prod deployment, set secrets with `sectr secrets set OPENAI_API_KEY` or visit https://app.sectr.dev/settings
model: BaseChatModel
if api_key := os.environ.get("OPENAI_API_KEY"):
    from langchain.chat_models import init_chat_model

    model = init_chat_model("openai:gpt-4o-mini", api_key=api_key)
else:
    model = ScriptedChatModel()

graph = create_agent(
    model,
    tools=[as_langgraph_tool(t) for t in TOOLS],
    system_prompt=(
        "You are a customer-support agent for a small shop. "
        "Look up order status with the lookup_order tool, "
        "and issue refunds with the refund tool."
    ),
)

app = AgentApp(name="__SECTR_APP_NAME__", env=["OPENAI_API_KEY"])
app.add_route("/chat", langgraph(graph))
