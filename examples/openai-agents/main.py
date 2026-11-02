"""openai-agents variant — the support agent on the OpenAI Agents SDK.

Everything the user writes to put this agent on Sectr: zero platform
concepts leak into the agent definition; the only sectr touchpoints are
`AgentApp` + one adapter line. The adapter maps the SDK's native streaming
events and HITL interruptions onto the platform contract (see
sdk-python/src/sectr/adapters/openai_agents.py).

Model providers:
- Offline CI: `SECTR_FAKE=1` selects the scripted model in fake.py
  (deterministic, no provider key).
- Real runs go through OpenRouter (one key, any provider model):
  put `OPENROUTER_API_KEY` in the project `.env` (declared on the
  AgentApp below, so the platform injects it into the runner), and
  optionally `SECTR_MODEL` (default z-ai/glm-5.3-flash).

Why the setup below (same pattern as the SDK's model_providers examples):
OpenRouter is OpenAI-compatible but only speaks Chat Completions, so we
bind a concrete OpenAIChatCompletionsModel to our client and disable
tracing (no OpenAI platform key). The model must be an INSTANCE, not a
bare name: the SDK prefix-routes name strings with a slash and rejects
unknown vendors ("Unknown prefix: z-ai").
"""

import os

from agents import (
    Agent,
    OpenAIChatCompletionsModel,
    set_default_openai_client,
    set_tracing_disabled,
)
from openai import AsyncOpenAI

from sectr import AgentApp
from sectr.adapters.openai_agents import openai_agents

from tools import lookup_order, refund, todo_write

OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1"
DEFAULT_MODEL = "z-ai/glm-5.3-flash"

model = None
if os.environ.get("SECTR_FAKE") == "1":
    from fake import scenario_model

    model = scenario_model()
elif api_key := os.environ.get("OPENROUTER_API_KEY"):
    client = AsyncOpenAI(base_url=OPENROUTER_BASE_URL, api_key=api_key)
    set_default_openai_client(client, use_for_tracing=False)
    set_tracing_disabled(disabled=True)
    model = OpenAIChatCompletionsModel(
        model=os.environ.get("SECTR_MODEL") or DEFAULT_MODEL,
        openai_client=client,
    )
# No key here? That's fine at INSPECT time (`sectr dev` imports this module
# before .env secrets are injected) — the import must succeed so routes can
# register. At RUNTIME the runner has OPENROUTER_API_KEY (declared below);
# if it's genuinely missing there, the SDK fails with its own clear error.

agent = Agent(
    name="support",
    instructions=(
        "You are a customer-support agent for a small shop. "
        "Keep a todo list of open issues with the todo tool."
    ),
    tools=[lookup_order, refund, todo_write],
    model=model,
)

app = AgentApp(name="support-agent", env=["OPENROUTER_API_KEY"])
app.add_route("/chat", openai_agents(agent, model=model))