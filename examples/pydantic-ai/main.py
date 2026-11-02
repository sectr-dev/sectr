"""Pydantic AI variant — the support agent on `pydantic_ai.Agent`.

Everything the user writes to put this agent on Sectr: the agent definition
is plain Pydantic AI; the only sectr touchpoints are `AgentApp` + one adapter
line. `refund` uses Pydantic AI's native deferred tools
(`requires_approval=True`): the run pauses, the adapter journals
TOOL_APPROVAL_REQUIRED, and on approval Pydantic AI resumes from the message
history alone and runs the tool itself (see
sdk-python/src/sectr/adapters/pydantic_ai.py).

Model providers:
- Offline CI: `SECTR_FAKE=1` selects the scripted model in fake.py
  (deterministic, no provider key).
- Real runs go through OpenRouter (one key, any provider model): put
  `OPENROUTER_API_KEY` in the project `.env` (declared on the AgentApp
  below, so the platform injects it into the runner), and optionally
  `SECTR_MODEL` (default z-ai/glm-5.3-flash).

`sectr dev` / `sectr inspect` import this module BEFORE secrets are injected,
so the model is only constructed when its key is present (a model *string*
such as 'openai:gpt-4o' would raise at construction without its key unless
you pass `defer_model_check=True`).
"""

import os

from pydantic_ai import Agent, Tool

from sectr import AgentApp
from sectr.adapters.pydantic_ai import pydantic_ai

from tools import lookup_order, refund, todo_write

DEFAULT_MODEL = "z-ai/glm-5.3-flash"

model = None
if os.environ.get("SECTR_FAKE") == "1":
    from fake import scenario_model

    model = scenario_model()
elif api_key := os.environ.get("OPENROUTER_API_KEY"):
    from pydantic_ai.models.openrouter import OpenRouterModel
    from pydantic_ai.providers.openrouter import OpenRouterProvider

    model = OpenRouterModel(
        os.environ.get("SECTR_MODEL") or DEFAULT_MODEL,
        provider=OpenRouterProvider(api_key=api_key),
    )
# No key here? That's fine at INSPECT time — the import must succeed so
# routes can register. At RUNTIME the runner has OPENROUTER_API_KEY
# (declared below); with no model Pydantic AI fails with its own clear error.

agent = Agent(
    model,
    name="support",
    instructions=(
        "You are a customer-support agent for a small shop. "
        "Keep a todo list of open issues with the todo tool."
    ),
    tools=[lookup_order, Tool(refund, requires_approval=True), todo_write],
)

app = AgentApp(name="support-agent", env=["OPENROUTER_API_KEY"])
app.add_route("/chat", pydantic_ai(agent))
