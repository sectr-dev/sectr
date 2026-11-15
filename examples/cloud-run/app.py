"""Your Sectr agent — the support agent on Pydantic AI.

Everything here is ordinary Pydantic AI code: the agent definition has no
platform concepts; the only sectr touchpoints are `AgentApp` and one adapter
line. `refund` is a native deferred tool (`requires_approval=True`): the run
pauses, Sectr journals the pause, and on approval Pydantic AI resumes from
the session transcript and runs the tool itself.

Model selection:
- `OPENAI_API_KEY` set → gpt-4o-mini on the standard OpenAI API.
- No key → the scripted model in fake.py answers, so `sectr dev` works
  with ZERO env vars.
"""

import os

from pydantic_ai import Agent, Tool
from pydantic_ai.models import Model
from sectr import AgentApp
from sectr.adapters.pydantic_ai import pydantic_ai

from fake import scenario_model
from tools import lookup_order, refund

# If OPENAI_API_KEY is set, uses gpt-4o-mini, otherwise uses a mock model. To set OPENAI_API_KEY:
# - For local `sectr dev`, cp .env.example .env and put in your key.
# - For prod deployment, set secrets with `sectr secrets set OPENAI_API_KEY` or visit https://app.sectr.dev/settings
#
# The real model is only constructed when the key exists: `sectr dev` imports
# this module before secrets are injected, and a model *string* such as
# "openai:gpt-4o-mini" raises at construction without its key.
model: Model
if api_key := os.environ.get("OPENAI_API_KEY"):
    from pydantic_ai.models.openai import OpenAIChatModel
    from pydantic_ai.providers.openai import OpenAIProvider

    model = OpenAIChatModel("gpt-4o-mini", provider=OpenAIProvider(api_key=api_key))
else:
    model = scenario_model()

agent = Agent(
    model,
    instructions=(
        "You are a customer-support agent for a small shop. "
        "Look up order status with the lookup_order tool, "
        "and issue refunds with the refund tool."
    ),
    tools=[lookup_order, Tool(refund, requires_approval=True)],
)

app = AgentApp(name="cloud-run", env=["OPENAI_API_KEY"])
app.add_route("/chat", pydantic_ai(agent))
